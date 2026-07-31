use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use serde::Serialize;

use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::HostVars;
use crate::domain::source::Source;
use crate::domain::view::{Ownership, View, ViewMember};
use crate::ports::cache::CachePort;

// Reading a view: resolve which member owns a host, and present the members'
// data as one dataset.
//
// Everything here is a pure function of a SNAPSHOT taken once per request. A
// view read touches three or more cache entries (each member's data, plus the
// inventory source each member's ownership resolves against) and they must not
// be re-read half way through: a sync landing in between would let one host be
// routed to a member and another to nobody, in the same response.

// One member of the view, with everything a read needs about it already looked
// up. `CacheEntry` clones are Arc bumps, not copies of the dataset.
pub struct MemberSnapshot<'a> {
    pub source_id: &'a str,
    // None when the member id is not in sources.yaml. Config validation rejects
    // that, so it means the config changed under a running process.
    pub source: Option<&'a Source>,
    // The member's data. None = it has never synced.
    pub entry: Option<CacheEntry>,
    // The dataset the ownership pattern resolves against. None = that source
    // has never synced, which is why `ownership_cached` is reported in /status:
    // it is the difference between "no member claims this host" and "the
    // routing table has not loaded yet".
    pub ownership: Option<CacheEntry>,
    pub owns: &'a Ownership,
}

impl<'a> MemberSnapshot<'a> {
    pub fn claims(&self, hostname: &str) -> bool {
        match &self.ownership {
            Some(entry) => self.owns.claims(&entry.dataset, hostname),
            // Without the inventory the group patterns cannot be expanded, but
            // an explicitly named host is a claim in its own right and stays
            // routable.
            None => self.owns.hosts.iter().any(|host| host == hostname),
        }
    }

    pub fn claimed_hosts(&self) -> HashSet<&str> {
        match &self.ownership {
            Some(entry) => self.owns.claimed_hosts(&entry.dataset),
            None => self.owns.hosts.iter().map(String::as_str).collect(),
        }
    }

    pub fn ownership_cached(&self) -> bool {
        self.ownership.is_some()
    }

    // The TTL that applies to this member's hosts under this view: the view's
    // declared one if it has any, else the member's own. Falls back to the
    // configured `ttl_seconds` when the member has no cache entry yet, so a
    // refresh of a never-synced member still has a threshold to compare against.
    pub fn default_ttl(&self, view: &View) -> u64 {
        view.ttl_seconds
            .or_else(|| self.entry.as_ref().map(|entry| entry.ttl.as_secs()))
            .or_else(|| self.source.map(|source| source.ttl_seconds))
            .unwrap_or(0)
    }

    // The member's group-level TTL overrides, resolved once per request rather
    // than per host (see Source::group_ttl_by_host).
    pub fn group_ttls(&self) -> HashMap<&str, u64> {
        match (self.source, self.entry.as_ref()) {
            (Some(source), Some(entry)) => source.group_ttl_by_host(&entry.dataset),
            _ => HashMap::new(),
        }
    }

    // A host's TTL under this view. A view TTL replaces the DEFAULT, not the
    // member's explicit per-host and per-group overrides — same rule as
    // everywhere else in the codebase, "an override beats the default", so a
    // view cannot accidentally cancel the 5-minute TTL somebody put on a
    // critical host.
    pub fn effective_ttl(
        &self,
        view: &View,
        hostname: &str,
        group_ttls: &HashMap<&str, u64>,
    ) -> u64 {
        let fallback = self.default_ttl(view);
        match self.source {
            Some(source) => source.effective_ttl(hostname, group_ttls, fallback),
            None => fallback,
        }
    }

    // Does this member actually hold data for the host it owns? False for the
    // appliances that never take SSH, and for anything provisioned since the
    // member's last gather.
    fn has(&self, hostname: &str) -> bool {
        self.entry
            .as_ref()
            .is_some_and(|entry| entry.dataset.hostvars.contains_key(hostname))
    }
}

pub struct ViewSnapshot<'a> {
    pub view_id: &'a str,
    pub view: &'a View,
    pub members: Vec<MemberSnapshot<'a>>,
    // hostname → index of the member that owns it, built at most once per
    // snapshot. See `routing()` for why this exists rather than resolving each
    // host against each member on demand.
    //
    // OnceLock, so a snapshot that never routes anything (reporting a member's
    // age and TTL needs no ownership at all) pays nothing. Owned Strings rather
    // than borrows: the hostnames live inside the `ownership` cache entries this
    // very struct owns, and a struct cannot borrow from itself.
    routing: OnceLock<HashMap<String, usize>>,
}

// Take the snapshot. Cheap on purpose — a few cache reads and no host
// resolution — because the refresh path takes one to route the request and
// another to serve the data the refresh just wrote.
pub fn snapshot<'a>(
    cache: &dyn CachePort,
    sources: &'a HashMap<String, Source>,
    view_id: &'a str,
    view: &'a View,
) -> ViewSnapshot<'a> {
    let members = view
        .members
        .iter()
        .map(|member: &'a ViewMember| MemberSnapshot {
            source_id: member.source.as_str(),
            source: sources.get(&member.source),
            entry: cache.get(&member.source),
            ownership: cache.get(&member.owns.source),
            owns: &member.owns,
        })
        .collect();

    ViewSnapshot {
        view_id,
        view,
        members,
        routing: OnceLock::new(),
    }
}

// A hostname the request asked for that no member claims. Its own type because
// it is the one routing failure that must never degrade into empty data: an
// unclaimed host is a gap in the view's config (or an inventory that has not
// loaded), and answering `{}` would turn that into something nobody
// investigates.
#[derive(Debug)]
pub struct UnclaimedHosts(pub Vec<String>);

impl<'a> ViewSnapshot<'a> {
    // The routing table: every claimed host mapped to the member that owns it.
    //
    // Resolved once per snapshot rather than per question. Asking each member
    // in turn whether it claims a host means a linear scan of that member's
    // ownership group — a thousand hostnames for a datacenter — and a whole-view
    // read asks it once per host, then again for every host it serves. On a
    // 2000-host view that was ~6 ms of string comparison per call and quadratic
    // in the inventory, on the read path a consumer polls.
    //
    // Built in declared order with `or_insert`, so the FIRST member to claim a
    // host keeps it — the same rule the per-member scan implemented, and the one
    // a multi-homed host in two datacenter groups depends on.
    fn routing(&self) -> &HashMap<String, usize> {
        self.routing.get_or_init(|| {
            let mut routing: HashMap<String, usize> = HashMap::new();
            for (index, member) in self.members.iter().enumerate() {
                for hostname in member.claimed_hosts() {
                    routing.entry(hostname.to_string()).or_insert(index);
                }
            }
            routing
        })
    }

    // Above this many hosts in one question, indexing the whole inventory pays
    // for itself. Below it, asking the members directly is cheaper than building
    // a table of thousands of entries to answer a handful of lookups — and the
    // commonest view read names exactly one host. The gap between the two costs
    // is wide, so the precise value does not matter.
    const ROUTING_TABLE_WORTH_IT: usize = 16;

    // Build the table if the caller is about to route enough hosts to justify it.
    fn prepare_routing(&self, hosts_in_question: usize) {
        if hosts_in_question > Self::ROUTING_TABLE_WORTH_IT {
            let _ = self.routing();
        }
    }

    // Which member owns this host. First claim wins, so overlap resolves by
    // declared order.
    pub fn owner_of<'s>(&'s self, hostname: &str) -> Option<&'s MemberSnapshot<'a>> {
        self.owner_index(hostname).map(|index| &self.members[index])
    }

    // Public so the HTTP layer routes through the same answer the reads do,
    // instead of keeping a second copy of "which member claims this".
    //
    // Uses the table once it exists; before that, asks the members directly.
    // The two agree by construction — the table is built from `claimed_hosts`,
    // which is the set form of the `claims` predicate this falls back to, in the
    // same declared order with the same first-claim-wins rule.
    pub fn owner_index(&self, hostname: &str) -> Option<usize> {
        match self.routing.get() {
            Some(routing) => routing.get(hostname).copied(),
            None => self
                .members
                .iter()
                .position(|member| member.claims(hostname)),
        }
    }

    // Every host the view can serve: claimed by some member AND present in
    // that member's data. Sorted, so pages and listings are stable.
    //
    // Hosts claimed by a member that does not have them are deliberately absent
    // here — the same answer a member gives for a host it has not gathered —
    // while still being routable for a refresh, which is what `owner_of` is for.
    pub fn hosts(&self) -> Vec<&str> {
        // One pass over the routing table, which has already resolved each host
        // to its owner and deduplicated them. This routes the whole inventory,
        // so the table is always worth building here. The owner may be a member
        // that has no data for the host; it then belongs to that member and is
        // absent here, rather than silently served by a later one. Ownership is
        // the contract.
        let mut hosts: Vec<&str> = self
            .routing()
            .iter()
            .filter(|(hostname, index)| self.members[**index].has(hostname))
            .map(|(hostname, _)| hostname.as_str())
            .collect();

        hosts.sort_unstable();
        hosts
    }

    // Resolve the ?host= / ?group= filter, exactly as the source routes do —
    // sorted, deduplicated, and an unmatched filter is an empty selection
    // rather than an error.
    //
    // The one addition is the refusal: a NAMED host that no member claims is an
    // error, because it means the request cannot be routed at all.
    pub fn select(
        &self,
        host: Option<&str>,
        group: Option<&str>,
    ) -> Result<Vec<&str>, UnclaimedHosts> {
        let mut hosts: Vec<&str> = match (host, group) {
            (Some(host), _) => {
                let mut selected: Vec<&str> = Vec::new();
                let mut unclaimed: Vec<String> = Vec::new();

                for name in host.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    match self.owner_of(name) {
                        Some(owner) => {
                            // Borrow the hostname from the cache, not from the
                            // query string, so the selection outlives the request
                            // parameters.
                            if let Some((key, _)) = owner
                                .entry
                                .as_ref()
                                .and_then(|entry| entry.dataset.hostvars.get_key_value(name))
                            {
                                selected.push(key.as_str());
                            }
                        }
                        None => unclaimed.push(name.to_string()),
                    }
                }

                if !unclaimed.is_empty() {
                    return Err(UnclaimedHosts(unclaimed));
                }
                selected
            }
            (None, Some(group)) => self
                .members
                .iter()
                .filter_map(|member| member.entry.as_ref())
                .filter_map(|entry| entry.dataset.groups.get(group))
                .flat_map(|g| g.hosts.iter().map(String::as_str))
                .collect(),
            (None, None) => self.hosts(),
        };

        hosts.sort_unstable();
        hosts.dedup();
        Ok(hosts)
    }

    // Group the requested hosts by the member that owns them, in declared
    // order. What the refresh path needs: one delegated gather per member
    // rather than one per host.
    #[allow(clippy::type_complexity)]
    pub fn route<'s>(
        &'s self,
        hosts: &[String],
    ) -> Result<Vec<(&'s MemberSnapshot<'a>, Vec<String>)>, UnclaimedHosts> {
        self.prepare_routing(hosts.len());

        let mut by_member: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        let mut unclaimed: Vec<String> = Vec::new();

        for hostname in hosts {
            match self.owner_index(hostname) {
                Some(index) => by_member.entry(index).or_default().push(hostname.clone()),
                None => unclaimed.push(hostname.clone()),
            }
        }

        if !unclaimed.is_empty() {
            return Err(UnclaimedHosts(unclaimed));
        }

        Ok(by_member
            .into_iter()
            .map(|(index, hosts)| (&self.members[index], hosts))
            .collect())
    }

    // The selected hosts' variables, each taken from the member that owns it.
    // Borrowed straight out of the cache entries the snapshot holds: no clone
    // of the facts, which on a federated pair is 17 MB.
    pub fn hostvars(&self, selection: &[&str]) -> HashMap<&str, &HostVars> {
        self.prepare_routing(selection.len());
        selection
            .iter()
            .filter_map(|hostname| {
                let owner = self.owner_of(hostname)?;
                // The key is taken from the cache entry, not from `selection`,
                // so everything this returns borrows from the snapshot alone.
                let (key, vars) = owner
                    .entry
                    .as_ref()?
                    .dataset
                    .hostvars
                    .get_key_value(*hostname)?;
                Some((key.as_str(), vars))
            })
            .collect()
    }

    // One group namespace for the whole view: same-named groups union their
    // hosts and children, and group vars collide by the same first-member-wins
    // rule as ownership.
    //
    // Membership is NOT filtered to what each member owns: a group is a
    // statement about the topology, and the datacenter A member listing the
    // datacenter B hosts is true whether or not the view serves their facts from
    // there.
    pub fn groups(&self) -> BTreeMap<&str, MergedGroup<'_>> {
        let mut merged: BTreeMap<&str, MergedGroup> = BTreeMap::new();

        for member in &self.members {
            let Some(entry) = member.entry.as_ref() else {
                continue;
            };
            for (name, group) in &entry.dataset.groups {
                let target = merged.entry(name.as_str()).or_default();
                target.hosts.extend(group.hosts.iter().map(String::as_str));
                target
                    .children
                    .extend(group.children.iter().map(String::as_str));
                if target.vars.is_none() {
                    target.vars = group.vars.as_ref();
                }
            }
        }

        for group in merged.values_mut() {
            group.hosts.sort_unstable();
            group.hosts.dedup();
            group.children.sort_unstable();
            group.children.dedup();
        }

        merged
    }

    // The view as one dataset, in the exact shape a source's /dataset returns —
    // which is what makes migrating a consumer a one-word change to an id.
    pub fn dataset(&self, selection: &[&str]) -> MergedDataset<'_> {
        MergedDataset {
            hostvars: self.hostvars(selection),
            groups: self.groups(),
            remove_hosts: &[],
        }
    }

    // Age of the view: the age of its STALEST cached member, because a view is
    // only as current as the least current thing it serves. A member that has
    // never synced contributes nothing to the age (there is no age) but does
    // make the view not fresh.
    pub fn age_seconds(&self) -> u64 {
        self.members
            .iter()
            .filter_map(|member| member.entry.as_ref())
            .map(|entry| entry.age_seconds())
            .max()
            .unwrap_or(0)
    }

    pub fn is_fresh(&self) -> bool {
        !self.members.is_empty()
            && self
                .members
                .iter()
                .all(|member| member.entry.as_ref().is_some_and(|entry| entry.is_fresh()))
    }

    // The TTL to report for the view as a whole: the declared one, or the
    // loosest member TTL — the loosest rather than the tightest because it is
    // the one that has to elapse before every member is stale.
    pub fn ttl_seconds(&self) -> u64 {
        self.view.ttl_seconds.unwrap_or_else(|| {
            self.members
                .iter()
                .map(|member| member.default_ttl(self.view))
                .max()
                .unwrap_or(0)
        })
    }
}

// Serializes exactly like domain::dataset::Group (the same three keys, always
// present), so a consumer cannot tell a view's groups from a source's.
#[derive(Serialize, Default)]
pub struct MergedGroup<'a> {
    pub hosts: Vec<&'a str>,
    pub children: Vec<&'a str>,
    pub vars: Option<&'a HostVars>,
}

// Serializes exactly like domain::dataset::Dataset. `remove_hosts` is always
// empty: it is an instruction to a cache writer, and a view writes nothing.
#[derive(Serialize)]
pub struct MergedDataset<'a> {
    pub hostvars: HashMap<&'a str, &'a HostVars>,
    pub groups: BTreeMap<&'a str, MergedGroup<'a>>,
    pub remove_hosts: &'static [String],
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::cache::memory::MemoryCache;
    use crate::domain::dataset::Dataset;

    fn dataset(json: serde_json::Value) -> Dataset {
        serde_json::from_value(json).unwrap()
    }

    // src-d42 is the inventory both members resolve ownership against;
    // src-dc1 and src-dc2 are the facts sources the view serves.
    fn fixture() -> (MemoryCache, HashMap<String, Source>, View) {
        let cache = MemoryCache::new();

        cache.set(
            "src-d42",
            CacheEntry::new(
                dataset(serde_json::json!({
                    "hostvars": {
                        "web01.dc1": {}, "web02.dc1": {},
                        "db01.dc2": {}, "appliance.dc2": {}
                    },
                    "groups": {
                        "datacenter_dc1": {"hosts": ["web01.dc1", "web02.dc1"]},
                        "datacenter_dc2": {"hosts": ["db01.dc2", "appliance.dc2"]}
                    }
                })),
                7200,
            ),
        );

        cache.set(
            "src-dc1",
            CacheEntry::new(
                dataset(serde_json::json!({
                    "hostvars": {"web01.dc1": {"os": "ol8"}, "web02.dc1": {"os": "ol9"}},
                    "groups": {"ol8": {"hosts": ["web01.dc1"]}, "shared": {"hosts": ["web01.dc1"]}}
                })),
                86400,
            ),
        );

        // db01 only; appliance.dc2 is owned by this member and never gathered
        cache.set(
            "src-dc2",
            CacheEntry::new(
                dataset(serde_json::json!({
                    "hostvars": {"db01.dc2": {"os": "rhel9"}},
                    "groups": {"shared": {"hosts": ["db01.dc2"]}}
                })),
                30,
            ),
        );

        let sources: HashMap<String, Source> = serde_yaml_ng::from_str(
            "src-d42:\n  name: d42\n  project_id: p\n  script_path: x\n  ttl_seconds: 7200\n\
             src-dc1:\n  name: dc1\n  project_id: p\n  script_path: x\n  ttl_seconds: 86400\n\
             src-dc2:\n  name: dc2\n  project_id: p\n  script_path: x\n  ttl_seconds: 30\n",
        )
        .unwrap();

        let view: View = serde_yaml_ng::from_str(
            "name: All facts\nmembers:\n\
             \x20 - source: src-dc1\n    owns:\n      source: src-d42\n      groups: [\"datacenter_dc1\"]\n\
             \x20 - source: src-dc2\n    owns:\n      source: src-d42\n      groups: [\"datacenter_dc2\"]\n",
        )
        .unwrap();

        (cache, sources, view)
    }

    fn snap<'a>(
        cache: &MemoryCache,
        sources: &'a HashMap<String, Source>,
        view: &'a View,
    ) -> ViewSnapshot<'a> {
        snapshot(cache, sources, "view-all", view)
    }

    #[test]
    fn a_host_routes_to_the_member_that_claims_it() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        assert_eq!(s.owner_of("web01.dc1").unwrap().source_id, "src-dc1");
        assert_eq!(s.owner_of("db01.dc2").unwrap().source_id, "src-dc2");
        assert!(s.owner_of("nobody.example").is_none());
    }

    // There are two ways to answer "who owns this host": the routing table, and
    // asking each member directly for a question too small to justify building
    // it. They must never disagree — a host served by one and refreshed against
    // the other would be a bug in both directions at once.
    #[test]
    fn the_routing_table_and_the_direct_scan_agree() {
        let (cache, sources, mut view) = fixture();
        // A multi-homed host in both datacenter groups, so declared order is
        // load bearing for this comparison too.
        cache.update("src-d42", &mut |entry| {
            entry.merge_dataset(dataset(serde_json::json!({
                "groups": {"datacenter_dc2": {"hosts": ["db01.dc2", "web01.dc1"]}}
            })));
        });

        for members_reversed in [false, true] {
            if members_reversed {
                view.members.reverse();
            }

            let direct = snap(&cache, &sources, &view);
            let indexed = snap(&cache, &sources, &view);
            indexed.routing(); // force the table on one of them

            for host in [
                "web01.dc1",
                "web02.dc1",
                "db01.dc2",
                "appliance.dc2",
                "nobody.example",
            ] {
                assert_eq!(
                    direct.owner_index(host),
                    indexed.owner_index(host),
                    "the two routing paths disagree about '{}' (reversed: {})",
                    host,
                    members_reversed
                );
            }
        }
    }

    // The fast path exists so a single-host read does not index the whole
    // inventory; this pins that it really is not built for one host.
    #[test]
    fn a_single_host_question_does_not_build_the_table() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);

        let routed = s.route(&["db01.dc2".to_string()]).unwrap();
        assert_eq!(routed[0].0.source_id, "src-dc2");
        assert!(s.routing.get().is_none());

        // ...while a whole-view read does build it
        let _ = s.hosts();
        assert!(s.routing.get().is_some());
    }

    #[test]
    fn a_claimed_host_with_no_data_still_routes() {
        // The appliance case: permanently the edge's responsibility, permanently
        // absent from its dataset. Routing it is what lets a refresh reach it.
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        assert_eq!(s.owner_of("appliance.dc2").unwrap().source_id, "src-dc2");
        assert!(!s.hosts().contains(&"appliance.dc2"));
    }

    #[test]
    fn the_union_is_every_owned_host_that_has_data() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        assert_eq!(s.hosts(), vec!["db01.dc2", "web01.dc1", "web02.dc1"]);
    }

    #[test]
    fn hostvars_come_from_the_owning_member() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        let selection = s.hosts();
        let vars = s.hostvars(&selection);
        assert_eq!(vars["web01.dc1"]["os"], "ol8");
        assert_eq!(vars["db01.dc2"]["os"], "rhel9");
    }

    #[test]
    fn same_named_groups_union_their_hosts() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        let groups = s.groups();
        assert_eq!(groups["shared"].hosts, vec!["db01.dc2", "web01.dc1"]);
        assert_eq!(groups["ol8"].hosts, vec!["web01.dc1"]);
    }

    #[test]
    fn naming_a_host_nobody_claims_is_refused_rather_than_empty() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        let err = s
            .select(Some("web01.dc1,nobody.example"), None)
            .expect_err("an unroutable host must not answer with empty data");
        assert_eq!(err.0, vec!["nobody.example"]);
    }

    #[test]
    fn a_group_filter_matching_nothing_is_an_empty_selection() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        assert!(s.select(None, Some("ghosts")).unwrap().is_empty());
    }

    #[test]
    fn a_group_filter_spans_members() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        assert_eq!(
            s.select(None, Some("shared")).unwrap(),
            vec!["db01.dc2", "web01.dc1"]
        );
    }

    #[test]
    fn routing_groups_hosts_by_member() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        let routed = s
            .route(&[
                "db01.dc2".to_string(),
                "web01.dc1".to_string(),
                "web02.dc1".to_string(),
            ])
            .unwrap();
        assert_eq!(routed.len(), 2);
        assert_eq!(routed[0].0.source_id, "src-dc1");
        assert_eq!(routed[0].1, vec!["web01.dc1", "web02.dc1"]);
        assert_eq!(routed[1].0.source_id, "src-dc2");
    }

    #[test]
    fn overlap_resolves_by_declared_order() {
        let (cache, sources, mut view) = fixture();
        // Put the same host in both datacenter groups, the multi-homed haproxy
        cache.update("src-d42", &mut |entry| {
            entry.merge_dataset(dataset(serde_json::json!({
                "groups": {"datacenter_dc2": {"hosts": ["db01.dc2", "web01.dc1"]}}
            })));
        });
        let s = snap(&cache, &sources, &view);
        assert_eq!(s.owner_of("web01.dc1").unwrap().source_id, "src-dc1");

        view.members.reverse();
        let s = snap(&cache, &sources, &view);
        assert_eq!(s.owner_of("web01.dc1").unwrap().source_id, "src-dc2");
    }

    #[test]
    fn a_declared_view_ttl_replaces_the_members_default() {
        let (cache, sources, mut view) = fixture();
        let s = snap(&cache, &sources, &view);
        // Inherited: the dc2 member's own 30s
        let dc2 = s.owner_of("db01.dc2").unwrap();
        assert_eq!(dc2.default_ttl(s.view), 30);

        view.ttl_seconds = Some(120);
        let s = snap(&cache, &sources, &view);
        assert_eq!(s.owner_of("db01.dc2").unwrap().default_ttl(s.view), 120);
        assert_eq!(s.ttl_seconds(), 120);
    }

    #[test]
    fn a_member_ttl_override_still_beats_a_view_ttl() {
        let (cache, mut sources, mut view) = fixture();
        view.ttl_seconds = Some(120);
        sources
            .get_mut("src-dc2")
            .unwrap()
            .ttl_overrides
            .hosts
            .insert("db01.dc2".to_string(), 5);

        let s = snap(&cache, &sources, &view);
        let dc2 = s.owner_of("db01.dc2").unwrap();
        assert_eq!(dc2.effective_ttl(s.view, "db01.dc2", &dc2.group_ttls()), 5);
    }

    #[test]
    fn the_view_reports_the_stalest_member() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        // Nothing is old enough to have aged a second, but every member is
        // cached and inside its TTL
        assert_eq!(s.age_seconds(), 0);
        assert!(s.is_fresh());
    }

    #[test]
    fn an_uncached_member_makes_the_view_not_fresh() {
        let (cache, sources, view) = fixture();
        cache.remove("src-dc2");
        let s = snap(&cache, &sources, &view);
        assert!(!s.is_fresh());
        // and its hosts leave the union, while staying routable for a refresh
        assert_eq!(s.hosts(), vec!["web01.dc1", "web02.dc1"]);
        assert_eq!(s.owner_of("db01.dc2").unwrap().source_id, "src-dc2");
    }

    #[test]
    fn without_the_ownership_source_only_explicit_hosts_route() {
        let (cache, sources, mut view) = fixture();
        view.members[1].owns.hosts = vec!["appliance.dc2".to_string()];
        cache.remove("src-d42");

        let s = snap(&cache, &sources, &view);
        assert!(!s.members[0].ownership_cached());
        // The group patterns cannot be expanded...
        assert!(s.owner_of("web01.dc1").is_none());
        // ...but a named host is a claim of its own
        assert_eq!(s.owner_of("appliance.dc2").unwrap().source_id, "src-dc2");
    }

    #[test]
    fn the_merged_dataset_has_the_same_shape_as_a_source() {
        let (cache, sources, view) = fixture();
        let s = snap(&cache, &sources, &view);
        let selection = s.hosts();
        let json = serde_json::to_value(s.dataset(&selection)).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["groups", "hostvars", "remove_hosts"]);
        let group = &json["groups"]["shared"];
        let group_keys: Vec<&str> = group
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(group_keys, vec!["children", "hosts", "vars"]);
    }
}
