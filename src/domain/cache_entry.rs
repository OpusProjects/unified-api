use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;

use super::dataset::{Dataset, HostVars};

// The dataset serialized to JSON once, shared by every response that serves
// it. `Bytes` is a reference-counted byte buffer: cloning it (which every
// HTTP response does) shares the allocation instead of copying it.
#[derive(Debug)]
pub struct SerializedJson {
    pub bytes: Bytes,
    // Strong ETag derived from the bytes — lets clients poll with
    // If-None-Match and get an empty 304 while the dataset is unchanged
    pub etag: String,
}

// Everything the cache knows about one host, captured so it can be put back
// after a replace that could not reach it: its variables, how old they truly
// are, and the groups it belongs to.
//
// Group membership is part of a host's state, not decoration around it. A
// connector derives its groups from the hosts it managed to gather, so keeping
// a host's variables but not its memberships leaves it in the dataset and in
// no group at all — invisible to every consumer that renders an inventory from
// `groups`, which is precisely the disappearance the retention exists to
// prevent.
#[derive(Debug)]
pub struct RetainedHost {
    hostname: String,
    vars: HostVars,
    age_seconds: u64,
    groups: Vec<String>,
}

// CacheEntry wraps a Dataset with cache metadata at three levels:
// - dataset level: when was the last full sync performed
// - host level: when was each individual host refreshed
// - group level: resolved by querying the group's hosts
#[derive(Debug, Clone)]
pub struct CacheEntry {
    // Arc<Dataset> = shared, reference-counted pointer (like a shared_ptr in
    // C++). Cloning a CacheEntry — which CachePort::get does on EVERY read —
    // only bumps a counter instead of deep-copying the whole dataset, so
    // concurrent full-dataset requests no longer multiply memory usage.
    // Reads are transparent thanks to Deref (entry.dataset.hostvars just
    // works); writers go through Arc::make_mut below.
    pub dataset: Arc<Dataset>,
    pub fetched_at: Instant,
    pub ttl: Duration,
    // Individual timestamp per host — allows knowing when each one was refreshed
    // If a host is not here, fetched_at from the dataset is used.
    // Also behind Arc for the same reason as dataset: get() clones the entry
    // on every read, and cloning one String per host on a large source is
    // real allocation work the read path doesn't need.
    pub host_timestamps: Arc<HashMap<String, Instant>>,
    // Whether this entry has ever come from a sync of the WHOLE source, as
    // opposed to one scoped to individual hosts or a group.
    //
    // The dataset-level fields answer "how old is the full inventory", and a
    // scoped sync has no answer to give: it gathered the hosts it was asked
    // for, not the source. An entry created by one used to report age 0 and
    // fresh — telling a consumer the whole datacenter had just been collected
    // when a single host had, and hiding a source that has never fully synced
    // from the very gauge meant to catch it.
    complete: bool,
    // Lazily-built JSON of the dataset, shared through the Arc<OnceLock> by
    // every clone of this entry: the first reader serializes, later readers
    // (and clones taken before a mutation) reuse the same bytes. OnceLock is
    // a thread-safe "write once, read many" cell. Mutators drop the whole
    // cell (fresh Arc) so the next reader re-serializes the new dataset.
    serialized: Arc<OnceLock<SerializedJson>>,
}

impl CacheEntry {
    // `impl Into<Arc<Dataset>>` accepts BOTH a plain Dataset and an already
    // shared Arc<Dataset> (Into<Arc<T>> exists for both), so callers that
    // just built a dataset and callers restoring a snapshot don't need to
    // wrap or unwrap anything.
    pub fn new(dataset: impl Into<Arc<Dataset>>, ttl_seconds: u64) -> Self {
        let dataset: Arc<Dataset> = dataset.into();
        let now = Instant::now();

        // When creating, all hosts have the same timestamp
        let host_timestamps: HashMap<String, Instant> = dataset
            .hostvars
            .keys()
            .map(|host| (host.clone(), now))
            .collect();

        Self {
            dataset,
            fetched_at: now,
            ttl: Duration::from_secs(ttl_seconds),
            host_timestamps: Arc::new(host_timestamps),
            // A full gather is the default; the scoped sync paths say otherwise
            complete: true,
            serialized: Arc::new(OnceLock::new()),
        }
    }

    // Rebuild an entry from a persisted snapshot. Instants cannot be stored on
    // disk (they are process-relative), so snapshots store AGES (elapsed
    // seconds) and this constructor converts them back: fetched_at = now - age.
    //
    // checked_sub returns None if the subtraction would go below the earliest
    // instant the OS clock can represent (age older than system boot). In that
    // corner case the entry is at least `age` old, so it is expired for any
    // reasonable TTL: we keep the data but with a zero TTL, making it served
    // as stale until the next sync refreshes it.
    pub fn restore(
        dataset: impl Into<Arc<Dataset>>,
        ttl_seconds: u64,
        age_seconds: u64,
        host_ages: HashMap<String, u64>,
    ) -> Self {
        let now = Instant::now();

        let (fetched_at, ttl) = match now.checked_sub(Duration::from_secs(age_seconds)) {
            Some(instant) => (instant, Duration::from_secs(ttl_seconds)),
            None => (now, Duration::ZERO),
        };

        let host_timestamps: HashMap<String, Instant> = host_ages
            .into_iter()
            .filter_map(|(host, age)| {
                now.checked_sub(Duration::from_secs(age))
                    .map(|instant| (host, instant))
            })
            .collect();

        Self {
            dataset: dataset.into(),
            fetched_at,
            ttl,
            host_timestamps: Arc::new(host_timestamps),
            complete: true,
            serialized: Arc::new(OnceLock::new()),
        }
    }

    // The dataset as JSON bytes + ETag, serialized at most once per dataset
    // version no matter how many requests ask for it. Errors are practically
    // impossible for our types (string keys, JSON-compatible values) but are
    // propagated rather than swallowed.
    pub fn serialized_json(&self) -> Result<&SerializedJson, String> {
        if let Some(cached) = self.serialized.get() {
            return Ok(cached);
        }
        let bytes = serde_json::to_vec(&*self.dataset).map_err(|e| e.to_string())?;
        // DefaultHasher::new() uses fixed keys, so the same bytes produce the
        // same ETag across processes and restarts (unlike HashMap's hasher,
        // which is randomly seeded per process).
        let mut hasher = std::hash::DefaultHasher::new();
        bytes.hash(&mut hasher);
        let etag = format!("\"{:016x}-{}\"", hasher.finish(), bytes.len());
        // set() fails if another thread won the race — either way get() below
        // returns the one value that ended up in the cell
        let _ = self.serialized.set(SerializedJson {
            bytes: Bytes::from(bytes),
            etag,
        });
        Ok(self.serialized.get().expect("just initialized"))
    }

    // Every mutation replaces the OnceLock with a fresh, empty one: clones
    // taken BEFORE the mutation keep serving the bytes they already share,
    // the entry in the cache re-serializes on its next read.
    fn invalidate_serialized(&mut self) {
        self.serialized = Arc::new(OnceLock::new());
    }

    // Is the complete dataset fresh? An entry that has only ever been filled by
    // scoped syncs is never fresh at dataset level, whatever its clock says —
    // there is no full gather for the TTL to be measured against.
    pub fn is_fresh(&self) -> bool {
        self.complete && self.fetched_at.elapsed() < self.ttl
    }

    // Whether a sync of the whole source has ever landed here.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    // Record that this entry holds hosts rather than a source: it exists
    // because a host- or group-scoped sync created it (a consumer asking for
    // one host on a cold cache), not because anything gathered the whole thing.
    pub fn mark_partial(&mut self) {
        self.complete = false;
    }

    // A sync of the WHOLE source has just landed here: it was gathered now, and
    // it is complete.
    //
    // The replace path says both of those by building a fresh CacheEntry. The
    // merge path patches the entry in place, so nothing else says either — and
    // until this existed, nothing did: a merge-mode source kept the `fetched_at`
    // of its very first sync forever. Its age grew without bound and it read as
    // stale from one TTL after boot, however faithfully it had been syncing
    // since.
    pub fn mark_gathered(&mut self) {
        self.fetched_at = Instant::now();
        self.complete = true;
    }

    // Age of the complete dataset in seconds
    pub fn age_seconds(&self) -> u64 {
        self.fetched_at.elapsed().as_secs()
    }

    // Is a specific host fresh? Accepts a TTL override per host
    pub fn is_host_fresh(&self, hostname: &str, ttl_override: Option<u64>) -> bool {
        let ttl = match ttl_override {
            Some(secs) => Duration::from_secs(secs),
            None => self.ttl, // if no override, use the global TTL
        };

        match self.host_timestamps.get(hostname) {
            Some(timestamp) => timestamp.elapsed() < ttl,
            None => false, // host does not exist in cache
        }
    }

    // Age of a specific host in seconds
    pub fn host_age_seconds(&self, hostname: &str) -> Option<u64> {
        self.host_timestamps
            .get(hostname)
            .map(|ts| ts.elapsed().as_secs())
    }

    // Updates a single host: its vars and its timestamp
    // &mut self = MUTABLE reference — we can modify the instance
    // This is the first time we see &mut: until now everything was &self (read-only)
    //
    // Arc::make_mut = clone-on-write: if nobody else holds this Arc, it hands
    // out a &mut to the existing Dataset (no copy at all); if a concurrent
    // reader still holds a clone, it copies the Dataset first so the reader
    // keeps seeing an immutable snapshot. Either way mutation is safe and
    // readers are never blocked or torn.
    pub fn update_host(&mut self, hostname: String, vars: HostVars) {
        self.update_host_aged(hostname, vars, 0);
    }

    // Same as update_host, but the host's timestamp is backdated by
    // `age_seconds` instead of stamped "now".
    //
    // This is what a host-scoped federated sync needs: the central fetched the
    // data from the edge, but the edge gathered it some seconds (or hours) ago.
    // Stamping now() would report a host as newly born every time a central
    // pulled it, which is exactly the lie CacheEntry::restore exists to avoid on
    // the full-dataset path. Same reasoning, same fallback: if the clock cannot
    // represent the backdated instant the data is older than the machine's
    // uptime, so `now` is the closest representable answer and it is still no
    // fresher than the truth by more than the uptime.
    pub fn update_host_aged(&mut self, hostname: String, vars: HostVars, age_seconds: u64) {
        self.invalidate_serialized();
        let dataset = Arc::make_mut(&mut self.dataset);
        dataset.hostvars.insert(hostname.clone(), vars);

        let now = Instant::now();
        let timestamp = now
            .checked_sub(Duration::from_secs(age_seconds))
            .unwrap_or(now);
        Arc::make_mut(&mut self.host_timestamps).insert(hostname, timestamp);
    }

    // Capture a host's complete state before an operation that would drop it.
    // Returns None when the host is not in this entry — there is nothing to keep.
    pub fn capture_host(&self, hostname: &str) -> Option<RetainedHost> {
        let vars = self.dataset.hostvars.get(hostname)?.clone();

        let groups = self
            .dataset
            .groups
            .iter()
            .filter(|(_, group)| group.hosts.iter().any(|host| host == hostname))
            .map(|(name, _)| name.clone())
            .collect();

        Some(RetainedHost {
            hostname: hostname.to_string(),
            vars,
            // A host with no timestamp is one nothing ever stamped; treating it
            // as gathered "now" would be the one direction that lies in the
            // host's favour, so it counts as brand new only when it truly is.
            age_seconds: self.host_age_seconds(hostname).unwrap_or(0),
            groups,
        })
    }

    // Put a captured host back: its variables and true age, then every group it
    // belonged to. A group the new dataset no longer carries is recreated,
    // because a group that vanished did so exactly when every host in it failed
    // to answer — dropping it would take the retained hosts with it.
    pub fn restore_host(&mut self, retained: RetainedHost) {
        let RetainedHost {
            hostname,
            vars,
            age_seconds,
            groups,
        } = retained;

        self.update_host_aged(hostname.clone(), vars, age_seconds);

        let dataset = Arc::make_mut(&mut self.dataset);
        for name in groups {
            let group = dataset.groups.entry(name).or_default();
            if !group.hosts.iter().any(|host| host == &hostname) {
                group.hosts.push(hostname.clone());
            }
        }
    }

    // Additive merge for derived data: adds or replaces only the keys that
    // come, leaving every other key on that host untouched.
    //
    // Two differences from merge_dataset, both deliberate:
    //
    // - it patches keys, not whole hosts, so two enrichers writing different
    //   keys onto the same host cannot lose each other's work. Each writes
    //   only what it owns and never carries a snapshot of the rest;
    // - it does not stamp host_timestamps. Those record when a host was last
    //   *gathered*, and a read decides whether to refresh from them. Derived
    //   data has nothing to say about that, and saying it would suppress
    //   refreshes the consumer asked for.
    //
    // Hosts absent from the target are skipped rather than inserted:
    // enrichment describes hosts, it does not introduce them.
    pub fn merge_hostvar_fields(&mut self, partial: Dataset) {
        self.invalidate_serialized();
        let dataset = Arc::make_mut(&mut self.dataset);

        for (hostname, vars) in partial.hostvars {
            if let Some(existing) = dataset.hostvars.get_mut(&hostname) {
                for (key, value) in vars {
                    existing.insert(key, value);
                }
            }
        }
    }

    // Merge: patches the hosts that come, the rest is left alone.
    // Also processes remove_hosts if they come.
    //
    // For data a connector GATHERED, so the hosts it carries were collected now
    // and their timestamps say so.
    pub fn merge_dataset(&mut self, partial: Dataset) {
        self.merge_into(partial, true);
    }

    // The same merge, for what a script enricher returns.
    //
    // The one difference is that it does not restamp a host that is already
    // here. `host_timestamps` record when a host was last GATHERED, and a read
    // consults them to decide whether to refresh before answering. Enrichment
    // derives from data already in the cache — it gathers nothing — so stamping
    // made an enriched host look freshly collected and could suppress a refresh
    // the consumer had explicitly asked for.
    //
    // The declarative merge was fixed in 0.10.0 by writing through
    // merge_hostvar_fields; the script path kept the bug because it went
    // through merge_dataset, which exists for gathered data.
    //
    // A host the enricher INTRODUCES is stamped: every host needs a timestamp
    // (one without is absent from /status and never fresh), and "now" is when
    // it became known.
    pub fn merge_enrichment(&mut self, partial: Dataset) {
        self.merge_into(partial, false);
    }

    fn merge_into(&mut self, partial: Dataset, restamp_existing: bool) {
        self.invalidate_serialized();
        let now = Instant::now();
        let dataset = Arc::make_mut(&mut self.dataset);
        let timestamps = Arc::make_mut(&mut self.host_timestamps);

        // Merge hostvars
        for (hostname, vars) in partial.hostvars {
            dataset.hostvars.insert(hostname.clone(), vars);
            // Keyed on the timestamp map rather than on hostvars, so the
            // invariant it protects — every host has one — holds even for an
            // entry that somehow arrived without it.
            if restamp_existing || !timestamps.contains_key(&hostname) {
                timestamps.insert(hostname, now);
            }
        }

        // Merge groups
        for (group_name, group) in partial.groups {
            dataset.groups.insert(group_name, group);
        }

        // Remove hosts
        for hostname in &partial.remove_hosts {
            dataset.hostvars.remove(hostname);
            timestamps.remove(hostname);
            // Remove the host from all groups
            for group in dataset.groups.values_mut() {
                group.hosts.retain(|h| h != hostname);
            }
        }
    }

    // Deletes a host from the cache
    pub fn remove_host(&mut self, hostname: &str) {
        self.invalidate_serialized();
        let dataset = Arc::make_mut(&mut self.dataset);
        dataset.hostvars.remove(hostname);
        Arc::make_mut(&mut self.host_timestamps).remove(hostname);
        for group in dataset.groups.values_mut() {
            group.hosts.retain(|h| h != hostname);
        }
    }

    pub fn update_group(&mut self, group_name: &str, partial_dataset: Dataset) {
        self.invalidate_serialized();
        let dataset = Arc::make_mut(&mut self.dataset);
        let timestamps = Arc::make_mut(&mut self.host_timestamps);

        // Only update the hosts that belong to the group
        if let Some(group) = dataset.groups.get(group_name) {
            let now = Instant::now();
            for hostname in &group.hosts {
                if let Some(vars) = partial_dataset.hostvars.get(hostname) {
                    dataset.hostvars.insert(hostname.clone(), vars.clone());
                    timestamps.insert(hostname.clone(), now);
                }
            }
        }

        // Update the group's vars if they come
        if let Some(new_group) = partial_dataset.groups.get(group_name)
            && let Some(existing_group) = dataset.groups.get_mut(group_name)
        {
            existing_group.vars = new_group.vars.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn empty_dataset() -> Dataset {
        Dataset {
            hostvars: HashMap::new(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        }
    }

    fn host_vars(name: &str, role: &str) -> (String, HostVars) {
        (
            name.to_string(),
            [("role".to_string(), serde_json::json!(role))]
                .into_iter()
                .collect(),
        )
    }

    fn dataset_with_hosts() -> Dataset {
        let mut hostvars = HashMap::new();
        hostvars.insert(
            "motoko.section9.net".to_string(),
            [("role".to_string(), serde_json::json!("commander"))]
                .into_iter()
                .collect(),
        );
        hostvars.insert(
            "batou.section9.net".to_string(),
            [("role".to_string(), serde_json::json!("assault"))]
                .into_iter()
                .collect(),
        );
        Dataset {
            hostvars,
            groups: HashMap::new(),
            remove_hosts: vec![],
        }
    }

    #[test]
    fn new_entry_is_fresh() {
        let entry = CacheEntry::new(empty_dataset(), 3600);
        assert!(entry.is_fresh());
    }

    #[test]
    fn update_host_aged_backdates_the_timestamp() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 600);
        let vars: HostVars = [("role".to_string(), serde_json::json!("sniper"))]
            .into_iter()
            .collect();

        entry.update_host_aged("saito.section9.net".to_string(), vars, 300);

        assert_eq!(entry.host_age_seconds("saito.section9.net"), Some(300));
        // still fresh: the TTL is longer than the age it came with
        assert!(entry.is_host_fresh("saito.section9.net", None));
        // and stale under a TTL shorter than that age
        assert!(!entry.is_host_fresh("saito.section9.net", Some(60)));
    }

    #[test]
    fn update_host_aged_with_zero_age_matches_update_host() {
        let mut entry = CacheEntry::new(empty_dataset(), 600);
        entry.update_host_aged("a.example".to_string(), HashMap::new(), 0);
        assert_eq!(entry.host_age_seconds("a.example"), Some(0));
    }

    #[test]
    fn update_host_aged_leaves_the_other_hosts_alone() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 600);
        entry.update_host_aged("motoko.section9.net".to_string(), HashMap::new(), 300);

        assert_eq!(entry.host_age_seconds("motoko.section9.net"), Some(300));
        assert_eq!(entry.host_age_seconds("batou.section9.net"), Some(0));
    }

    #[test]
    fn a_new_entry_is_a_complete_one() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert!(entry.is_complete());
        assert!(entry.is_fresh());
    }

    // The dataset-level TTL measures a full gather, and a partial entry has
    // none to measure — so no clock makes it fresh.
    #[test]
    fn a_partial_entry_is_never_fresh_however_new() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        entry.mark_partial();

        assert!(!entry.is_complete());
        assert!(!entry.is_fresh());
        // Its hosts are another matter: those really were gathered
        assert!(entry.is_host_fresh("motoko.section9.net", None));
    }

    #[test]
    fn a_full_sync_makes_a_partial_entry_complete_again() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        entry.mark_partial();

        entry.mark_gathered();

        assert!(entry.is_complete());
        assert!(entry.is_fresh());
    }

    // The other half: a gather restarts the dataset clock. Without it an entry
    // that is only ever patched in place ages forever from its first sync.
    //
    // The fixture is backdated by seconds rather than hours on purpose: Instant
    // is monotonic from boot, so subtracting more than the machine's uptime is
    // not representable and CacheEntry::restore falls back (see its comment).
    #[test]
    fn mark_gathered_restarts_the_dataset_clock() {
        let mut entry = CacheEntry::restore(dataset_with_hosts(), 1, 5, HashMap::new());
        assert_eq!(entry.age_seconds(), 5);
        assert!(!entry.is_fresh());

        entry.mark_gathered();

        assert_eq!(entry.age_seconds(), 0);
        assert!(entry.is_fresh());
    }

    // Host-level timestamps answer a different question — when was this host
    // gathered — and the merge that carries the data is what stamps those.
    #[test]
    fn mark_gathered_leaves_the_host_timestamps_alone() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        let (name, vars) = host_vars("motoko.section9.net", "commander");
        entry.update_host_aged(name, vars, 900);

        entry.mark_gathered();

        assert!(entry.host_age_seconds("motoko.section9.net").unwrap_or(0) >= 900);
    }

    #[test]
    fn expired_entry_is_not_fresh() {
        let entry = CacheEntry::new(empty_dataset(), 0);
        thread::sleep(std::time::Duration::from_millis(10));
        assert!(!entry.is_fresh());
    }

    #[test]
    fn age_starts_at_zero() {
        let entry = CacheEntry::new(empty_dataset(), 3600);
        assert_eq!(entry.age_seconds(), 0);
    }

    #[test]
    fn ttl_is_set_correctly() {
        let entry = CacheEntry::new(empty_dataset(), 300);
        assert_eq!(entry.ttl, std::time::Duration::from_secs(300));
    }

    #[test]
    fn host_timestamps_created_on_new() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert_eq!(entry.host_timestamps.len(), 2);
        assert!(entry.host_timestamps.contains_key("motoko.section9.net"));
    }

    #[test]
    fn host_is_fresh_with_global_ttl() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert!(entry.is_host_fresh("motoko.section9.net", None));
    }

    #[test]
    fn host_is_stale_with_override_ttl() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        thread::sleep(std::time::Duration::from_millis(10));
        // Override of 0 seconds = always expired
        assert!(!entry.is_host_fresh("motoko.section9.net", Some(0)));
    }

    #[test]
    fn unknown_host_is_not_fresh() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert!(!entry.is_host_fresh("togusa.section9.net", None));
    }

    #[test]
    fn update_host_refreshes_timestamp() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        let original_ts = entry.host_timestamps["motoko.section9.net"];

        thread::sleep(std::time::Duration::from_millis(10));

        let new_vars = [("role".to_string(), serde_json::json!("upgraded"))]
            .into_iter()
            .collect();
        entry.update_host("motoko.section9.net".to_string(), new_vars);

        // The timestamp must have changed
        assert!(entry.host_timestamps["motoko.section9.net"] > original_ts);
        // The vars must be updated
        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["role"],
            "upgraded"
        );
    }

    #[test]
    fn host_age_returns_none_for_unknown() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert!(entry.host_age_seconds("togusa.section9.net").is_none());
    }

    // The property that makes several enrichers on one target safe: each
    // writes only the key it owns, and neither erases the other's.
    #[test]
    fn merge_hostvar_fields_keeps_keys_it_was_not_given() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);

        let from_first = Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [(
                    "infinibox".to_string(),
                    serde_json::json!({"volume": "vol-a"}),
                )]
                .into_iter()
                .collect(),
            )]
            .into_iter()
            .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        };
        let from_second = Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [("avamar".to_string(), serde_json::json!({"client": "c-1"}))]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        };

        entry.merge_hostvar_fields(from_first);
        entry.merge_hostvar_fields(from_second);

        let vars = &entry.dataset.hostvars["motoko.section9.net"];
        // both derived keys survived, and so did what the connector gathered
        assert_eq!(vars["infinibox"]["volume"], "vol-a");
        assert_eq!(vars["avamar"]["client"], "c-1");
        assert_eq!(vars["role"], "commander");
    }

    // Derived data must not testify to when a host was last gathered: those
    // timestamps decide whether a read refreshes.
    #[test]
    fn merge_hostvar_fields_does_not_refresh_host_age() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        // backdate the host so a reset would be unmistakable
        entry.update_host_aged(
            "motoko.section9.net".to_string(),
            [("role".to_string(), serde_json::json!("commander"))]
                .into_iter()
                .collect(),
            900,
        );
        let before = entry.host_age_seconds("motoko.section9.net");
        assert!(before.unwrap_or(0) >= 900);

        entry.merge_hostvar_fields(Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [(
                    "infinibox".to_string(),
                    serde_json::json!({"volume": "vol-a"}),
                )]
                .into_iter()
                .collect(),
            )]
            .into_iter()
            .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        });

        assert_eq!(before, entry.host_age_seconds("motoko.section9.net"));
    }

    // Enrichment describes hosts, it does not introduce them.
    #[test]
    fn merge_hostvar_fields_ignores_unknown_hosts() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        let before = entry.dataset.hostvars.len();

        entry.merge_hostvar_fields(Dataset {
            hostvars: [(
                "ghost.section9.net".to_string(),
                [(
                    "infinibox".to_string(),
                    serde_json::json!({"volume": "vol-z"}),
                )]
                .into_iter()
                .collect(),
            )]
            .into_iter()
            .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        });

        assert_eq!(entry.dataset.hostvars.len(), before);
        assert!(!entry.dataset.hostvars.contains_key("ghost.section9.net"));
    }

    #[test]
    fn merge_dataset_adds_new_hosts() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert_eq!(entry.dataset.hostvars.len(), 2);

        let partial = Dataset {
            hostvars: [(
                "togusa.section9.net".to_string(),
                [("role".to_string(), serde_json::json!("detective"))]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        };

        entry.merge_dataset(partial);
        assert_eq!(entry.dataset.hostvars.len(), 3);
        assert_eq!(
            entry.dataset.hostvars["togusa.section9.net"]["role"],
            "detective"
        );
    }

    #[test]
    fn merge_dataset_updates_existing_hosts() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);

        let partial = Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [("role".to_string(), serde_json::json!("major"))]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        };

        entry.merge_dataset(partial);
        assert_eq!(entry.dataset.hostvars.len(), 2);
        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["role"],
            "major"
        );
    }

    // Derived data must not testify to when a host was last gathered: those
    // timestamps decide whether a read refreshes. Same rule the declarative
    // merge follows — the script path used to break it through merge_dataset.
    #[test]
    fn merge_enrichment_does_not_refresh_an_existing_hosts_age() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        let (name, vars) = host_vars("motoko.section9.net", "commander");
        entry.update_host_aged(name, vars, 900);
        let before = entry.host_age_seconds("motoko.section9.net");

        entry.merge_enrichment(Dataset {
            hostvars: [(
                "motoko.section9.net".to_string(),
                [("infinibox".to_string(), serde_json::json!("vol-a"))]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        });

        assert_eq!(before, entry.host_age_seconds("motoko.section9.net"));
        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["infinibox"],
            "vol-a"
        );
        // Still stale under a TTL shorter than its real age, so a refresh the
        // consumer asked for still happens
        assert!(!entry.is_host_fresh("motoko.section9.net", Some(600)));
    }

    // Every host needs a timestamp: one without is absent from /status and
    // never counts as fresh.
    #[test]
    fn merge_enrichment_stamps_a_host_it_introduces() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);

        entry.merge_enrichment(Dataset {
            hostvars: [host_vars("saito.section9.net", "sniper")]
                .into_iter()
                .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        });

        assert_eq!(entry.host_age_seconds("saito.section9.net"), Some(0));
        assert!(entry.is_host_fresh("saito.section9.net", None));
    }

    #[test]
    fn merge_enrichment_still_removes_hosts() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);

        entry.merge_enrichment(Dataset {
            hostvars: HashMap::new(),
            groups: HashMap::new(),
            remove_hosts: vec!["batou.section9.net".to_string()],
        });

        assert!(!entry.dataset.hostvars.contains_key("batou.section9.net"));
        assert!(!entry.host_timestamps.contains_key("batou.section9.net"));
    }

    // The other side of the distinction: a gather DOES say when it happened.
    #[test]
    fn merge_dataset_refreshes_an_existing_hosts_age() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        let (name, vars) = host_vars("motoko.section9.net", "commander");
        entry.update_host_aged(name, vars, 900);

        entry.merge_dataset(Dataset {
            hostvars: [host_vars("motoko.section9.net", "major")]
                .into_iter()
                .collect(),
            groups: HashMap::new(),
            remove_hosts: vec![],
        });

        assert_eq!(entry.host_age_seconds("motoko.section9.net"), Some(0));
    }

    #[test]
    fn merge_dataset_removes_hosts() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        assert_eq!(entry.dataset.hostvars.len(), 2);

        let partial = Dataset {
            hostvars: HashMap::new(),
            groups: HashMap::new(),
            remove_hosts: vec!["batou.section9.net".to_string()],
        };

        entry.merge_dataset(partial);
        assert_eq!(entry.dataset.hostvars.len(), 1);
        assert!(!entry.dataset.hostvars.contains_key("batou.section9.net"));
        assert!(!entry.host_timestamps.contains_key("batou.section9.net"));
    }

    #[test]
    fn serialized_json_reuses_the_same_buffer() {
        let entry = CacheEntry::new(dataset_with_hosts(), 3600);
        let first_ptr = entry.serialized_json().unwrap().bytes.as_ptr();
        // Second call and a clone of the entry both share the same buffer
        assert_eq!(entry.serialized_json().unwrap().bytes.as_ptr(), first_ptr);
        let clone = entry.clone();
        assert_eq!(clone.serialized_json().unwrap().bytes.as_ptr(), first_ptr);
    }

    #[test]
    fn serialized_json_invalidated_by_mutation() {
        let mut entry = CacheEntry::new(dataset_with_hosts(), 3600);
        let before = entry.serialized_json().unwrap().etag.clone();

        entry.update_host("togusa.section9.net".to_string(), HashMap::new());

        let after = entry.serialized_json().unwrap().etag.clone();
        assert_ne!(before, after, "a mutation must produce a new ETag");
        // And the new bytes actually contain the new host
        let json: serde_json::Value =
            serde_json::from_slice(&entry.serialized_json().unwrap().bytes).unwrap();
        assert!(json["hostvars"]["togusa.section9.net"].is_object());
    }

    fn dataset_with_groups() -> Dataset {
        use crate::domain::dataset::Group;

        let mut dataset = dataset_with_hosts();
        dataset.groups.insert(
            "section9".to_string(),
            Group {
                hosts: vec![
                    "motoko.section9.net".to_string(),
                    "batou.section9.net".to_string(),
                ],
                children: vec![],
                vars: None,
            },
        );
        dataset.groups.insert(
            "commanders".to_string(),
            Group {
                hosts: vec!["motoko.section9.net".to_string()],
                children: vec![],
                vars: None,
            },
        );
        dataset
    }

    #[test]
    fn capture_takes_the_hosts_vars_age_and_groups() {
        let mut entry = CacheEntry::new(dataset_with_groups(), 3600);
        let (name, vars) = host_vars("motoko.section9.net", "commander");
        entry.update_host_aged(name, vars, 900);

        let captured = entry.capture_host("motoko.section9.net").expect("captured");
        let mut groups = captured.groups.clone();
        groups.sort();
        assert_eq!(groups, vec!["commanders", "section9"]);
        assert!(captured.age_seconds >= 900);
        assert_eq!(captured.vars["role"], "commander");
    }

    #[test]
    fn capturing_an_unknown_host_returns_nothing() {
        let entry = CacheEntry::new(dataset_with_groups(), 3600);
        assert!(entry.capture_host("togusa.section9.net").is_none());
    }

    // The whole point: a restored host is back in its groups, not just in
    // hostvars, so an inventory rendered from `groups` still contains it.
    #[test]
    fn restore_puts_the_host_back_in_its_groups() {
        let previous = CacheEntry::new(dataset_with_groups(), 3600);
        let captured = previous
            .capture_host("motoko.section9.net")
            .expect("captured");

        // A fresh gather that reached nobody but batou
        let mut gathered = dataset_with_groups();
        gathered.hostvars.remove("motoko.section9.net");
        for group in gathered.groups.values_mut() {
            group.hosts.retain(|host| host != "motoko.section9.net");
        }
        let mut entry = CacheEntry::new(gathered, 3600);

        entry.restore_host(captured);

        assert_eq!(
            entry.dataset.hostvars["motoko.section9.net"]["role"],
            "commander"
        );
        assert!(
            entry.dataset.groups["section9"]
                .hosts
                .contains(&"motoko.section9.net".to_string())
        );
        assert_eq!(
            entry.dataset.groups["commanders"].hosts,
            vec!["motoko.section9.net"]
        );
    }

    // A group vanishes exactly when every host in it failed to answer, so
    // dropping it would take the retained hosts with it.
    #[test]
    fn restore_recreates_a_group_the_new_dataset_lost() {
        let previous = CacheEntry::new(dataset_with_groups(), 3600);
        let captured = previous
            .capture_host("motoko.section9.net")
            .expect("captured");

        let mut entry = CacheEntry::new(empty_dataset(), 3600);
        entry.restore_host(captured);

        assert_eq!(
            entry.dataset.groups["commanders"].hosts,
            vec!["motoko.section9.net"]
        );
    }

    #[test]
    fn restore_does_not_list_a_host_twice_in_a_group() {
        let previous = CacheEntry::new(dataset_with_groups(), 3600);
        let captured = previous
            .capture_host("motoko.section9.net")
            .expect("captured");

        // The new dataset still lists the host in the group even though it has
        // no hostvars for it
        let mut entry = CacheEntry::new(dataset_with_groups(), 3600);
        entry.restore_host(captured);

        assert_eq!(entry.dataset.groups["commanders"].hosts.len(), 1);
    }

    #[test]
    fn restore_keeps_the_age_the_host_already_had() {
        let mut previous = CacheEntry::new(dataset_with_groups(), 3600);
        let (name, vars) = host_vars("motoko.section9.net", "commander");
        previous.update_host_aged(name, vars, 900);
        let captured = previous
            .capture_host("motoko.section9.net")
            .expect("captured");

        let mut entry = CacheEntry::new(empty_dataset(), 3600);
        entry.restore_host(captured);

        assert!(entry.host_age_seconds("motoko.section9.net").unwrap_or(0) >= 900);
    }

    #[test]
    fn remove_host_deletes_from_groups() {
        use crate::domain::dataset::Group;

        let mut entry = CacheEntry::new(
            Dataset {
                hostvars: [(
                    "motoko.section9.net".to_string(),
                    [("role".to_string(), serde_json::json!("commander"))]
                        .into_iter()
                        .collect(),
                )]
                .into_iter()
                .collect(),
                groups: [(
                    "section9".to_string(),
                    Group {
                        hosts: vec!["motoko.section9.net".to_string()],
                        children: vec![],
                        vars: None,
                    },
                )]
                .into_iter()
                .collect(),
                remove_hosts: vec![],
            },
            3600,
        );

        entry.remove_host("motoko.section9.net");
        assert!(!entry.dataset.hostvars.contains_key("motoko.section9.net"));
        assert!(entry.dataset.groups["section9"].hosts.is_empty());
    }
}
