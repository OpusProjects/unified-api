use serde::Deserialize;
use std::collections::HashSet;

use super::dataset::Dataset;

// A view is a read-only composite over several sources: it presents them as one
// id, routes a per-host request to whichever member OWNS that host, and
// delegates an on-demand refresh to that member. It gathers nothing itself,
// which is why it has no connector, no schedule and no sync.
//
// What it buys is that a consumer stops needing to know the topology. With
// federation (`connector_type: remote`) the central needs no credentials into a
// datacenter, but the facts of a datacenter B host still live in a different
// source id than those of a datacenter A host — so every consumer learned the
// split, and relearns it whenever an edge is added. A view is one address
// for "the facts".
//
// `deny_unknown_fields` is where the config-wide policy (see config.rs)
// started: ownership is the routing table, and a typo in it (`grups:` instead
// of `groups:`) would otherwise deserialize into an empty pattern, which claims
// everything. A hard startup error beats a member that silently swallows the
// whole inventory.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub name: String,

    // Declared order is significant: the FIRST member that claims a host wins
    // it. Overlap is normal (one multi-homed host can legitimately sit in two
    // datacenter groups) and needs a rule that is stable rather than clever.
    pub members: Vec<ViewMember>,

    // The view's own freshness policy, which is also the gate for on-demand
    // refresh (`refresh_hosts` only gathers hosts older than it). None =
    // inherit the owning member's TTL, so the member keeps governing.
    //
    // Declaring one here is a deliberate consumer-facing policy: it means two
    // doors onto the same data with different refresh rules, which is fine as
    // long as somebody chose it. What it must never be is an accident — a
    // default TTL appearing out of nowhere would silently replace the
    // `ttl_seconds` that governs a datacenter today.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ViewMember {
    // Source id this member serves data from
    pub source: String,

    // Which hosts this member is responsible for
    pub owns: Ownership,
}

// Ownership is DECLARED, not inferred from what a member happens to have
// cached. Two reasons, both found the hard way:
//
// - Facts sources are often synced daily on purpose (the bulk is a floor,
//   freshness comes from on-demand), so a host provisioned this morning is not
//   in any cache until tomorrow — which is exactly the case on-demand refresh
//   exists for. Routing by cache membership would refuse to route it.
// - Some hosts never enter a cache at all (appliances that take no SSH). They
//   are permanently the edge's responsibility and permanently absent from its
//   data.
//
// The pattern resolves against ANOTHER source's dataset — normally the
// inventory source that both the view and the member's `hosts_from_source`
// already agree on. That is the only rule that is local, fresh, and identical
// for a local `ssh` member and a `remote` one: a remote member has no
// `hosts_from_source` at the central at all, so there is nothing else to ask.
//
// The cost is a duplicated truth (the edge says "I am datacenter_dc2" in its
// own config; the view repeats it). The no-drift version would have the edge
// advertise its scope over the API and the view read it.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    // Source whose cached dataset the groups below are resolved against
    pub source: String,

    // Groups of `source` whose members this view member owns
    #[serde(default)]
    pub groups: Vec<String>,

    // Individually named hosts. Unlike groups, these are claimed whether or not
    // they appear in `source` — naming a host IS the declaration, and a host
    // the inventory does not know about yet is precisely one somebody may need
    // to reach.
    #[serde(default)]
    pub hosts: Vec<String>,
}

impl Ownership {
    // No groups and no hosts = "everything the ownership source knows". Useful
    // as a LAST member ("whatever the others did not claim"), dangerous as a
    // first one, which is why order is documented as significant.
    pub fn is_catch_all(&self) -> bool {
        self.groups.is_empty() && self.hosts.is_empty()
    }

    // Does this member claim `hostname`?
    //
    // Deliberately cheap on the common shape (a couple of groups, one host
    // asked for): a linear scan of the group's member list beats building a
    // set of 400 hostnames to answer one question. The whole-set version below
    // is for the union read, which needs it anyway.
    pub fn claims(&self, dataset: &Dataset, hostname: &str) -> bool {
        if self.is_catch_all() {
            return dataset.hostvars.contains_key(hostname)
                || dataset
                    .groups
                    .values()
                    .any(|group| group.hosts.iter().any(|host| host == hostname));
        }

        if self.hosts.iter().any(|host| host == hostname) {
            return true;
        }

        self.groups.iter().any(|name| {
            dataset
                .groups
                .get(name)
                .is_some_and(|group| group.hosts.iter().any(|host| host == hostname))
        })
    }

    // Every host this member claims. Borrowed from the dataset and from the
    // config, so the caller has to keep holding the cache entry — which it
    // does, for the length of one request.
    pub fn claimed_hosts<'a>(&'a self, dataset: &'a Dataset) -> HashSet<&'a str> {
        let mut claimed: HashSet<&str> = HashSet::new();

        if self.is_catch_all() {
            claimed.extend(dataset.hostvars.keys().map(String::as_str));
            for group in dataset.groups.values() {
                claimed.extend(group.hosts.iter().map(String::as_str));
            }
            return claimed;
        }

        claimed.extend(self.hosts.iter().map(String::as_str));
        for name in &self.groups {
            if let Some(group) = dataset.groups.get(name) {
                claimed.extend(group.hosts.iter().map(String::as_str));
            }
        }
        claimed
    }
}

impl View {
    pub fn member_ids(&self) -> Vec<&str> {
        self.members
            .iter()
            .map(|member| member.source.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dataset::Group;

    fn dataset() -> Dataset {
        serde_json::from_value(serde_json::json!({
            "hostvars": {
                "web01.dc1.example": {},
                "web02.dc1.example": {},
                "db01.dc2.example": {}
            },
            "groups": {
                "datacenter_dc1": {"hosts": ["web01.dc1.example", "web02.dc1.example"]},
                "datacenter_dc2": {"hosts": ["db01.dc2.example", "web02.dc1.example"]}
            }
        }))
        .unwrap()
    }

    fn view(yaml: &str) -> View {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    const TWO_DATACENTERS: &str = "\
name: All facts
members:
  - source: src-dc1
    owns:
      source: src-d42
      groups: [\"datacenter_dc1\"]
  - source: src-dc2
    owns:
      source: src-d42
      groups: [\"datacenter_dc2\"]
";

    #[test]
    fn a_group_pattern_claims_that_groups_members() {
        let view = view(TWO_DATACENTERS);
        let ds = dataset();
        assert!(view.members[0].owns.claims(&ds, "web01.dc1.example"));
        assert!(!view.members[0].owns.claims(&ds, "db01.dc2.example"));
        assert!(view.members[1].owns.claims(&ds, "db01.dc2.example"));
    }

    #[test]
    fn ttl_is_absent_unless_declared() {
        assert!(view(TWO_DATACENTERS).ttl_seconds.is_none());
        let with_ttl = view(&format!("{}ttl_seconds: 30\n", TWO_DATACENTERS));
        assert_eq!(with_ttl.ttl_seconds, Some(30));
    }

    #[test]
    fn a_host_in_two_groups_is_claimed_by_both_patterns() {
        // Resolution of the overlap is the caller's job (first member wins);
        // the pattern itself simply says yes to both.
        let view = view(TWO_DATACENTERS);
        let ds = dataset();
        assert!(view.members[0].owns.claims(&ds, "web02.dc1.example"));
        assert!(view.members[1].owns.claims(&ds, "web02.dc1.example"));
    }

    #[test]
    fn an_explicitly_named_host_is_claimed_even_when_the_dataset_lacks_it() {
        let owns: Ownership =
            serde_yaml_ng::from_str("source: src-d42\nhosts: [\"brandnew.dc2.example\"]\n")
                .unwrap();
        assert!(owns.claims(&dataset(), "brandnew.dc2.example"));
        assert!(
            owns.claimed_hosts(&dataset())
                .contains("brandnew.dc2.example")
        );
    }

    #[test]
    fn an_unknown_group_claims_nothing_rather_than_everything() {
        let owns: Ownership =
            serde_yaml_ng::from_str("source: src-d42\ngroups: [\"datacenter_ghost\"]\n").unwrap();
        assert!(!owns.claims(&dataset(), "web01.dc1.example"));
        assert!(owns.claimed_hosts(&dataset()).is_empty());
    }

    #[test]
    fn an_empty_pattern_is_a_catch_all() {
        let owns: Ownership = serde_yaml_ng::from_str("source: src-d42\n").unwrap();
        assert!(owns.is_catch_all());
        assert!(owns.claims(&dataset(), "web01.dc1.example"));
        assert!(!owns.claims(&dataset(), "nowhere.example"));
        assert_eq!(owns.claimed_hosts(&dataset()).len(), 3);
    }

    #[test]
    fn group_members_without_hostvars_are_still_claimed() {
        let mut ds = dataset();
        ds.groups.insert(
            "datacenter_dc1".to_string(),
            Group {
                hosts: vec!["appliance.dc1.example".to_string()],
                children: vec![],
                vars: None,
            },
        );
        let view = view(TWO_DATACENTERS);
        // The 28 appliances that take no SSH: claimed by the member, absent
        // from its data, and still routable.
        assert!(view.members[0].owns.claims(&ds, "appliance.dc1.example"));
    }

    #[test]
    fn claimed_hosts_is_the_union_of_groups_and_hosts() {
        let owns: Ownership = serde_yaml_ng::from_str(
            "source: src-d42\ngroups: [\"datacenter_dc1\"]\nhosts: [\"db01.dc2.example\"]\n",
        )
        .unwrap();
        let ds = dataset();
        let claimed = owns.claimed_hosts(&ds);
        assert_eq!(claimed.len(), 3);
        assert!(claimed.contains("db01.dc2.example"));
    }

    #[test]
    fn a_misspelled_pattern_key_fails_to_parse_instead_of_claiming_everything() {
        let err = serde_yaml_ng::from_str::<Ownership>("source: src-d42\ngrups: [\"a\"]\n")
            .expect_err("a typo in the routing table must not deserialize");
        assert!(err.to_string().contains("grups"), "error was: {}", err);
    }

    #[test]
    fn member_ids_lists_the_sources_in_declared_order() {
        assert_eq!(
            view(TWO_DATACENTERS).member_ids(),
            vec!["src-dc1", "src-dc2"]
        );
    }
}
