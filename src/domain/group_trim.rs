use std::collections::{HashMap, HashSet};

// `groups=matching`: the groups a page of hosts actually belongs to.
//
// A dataset read that selects a few hosts used to carry every group of the
// source with its full member list — on a 1000-host source that is most of the
// response, whatever the selection was. Matching keeps:
//
// - every group with at least one host on the page, its `hosts` cut to those;
// - every group with a kept group among its children, so a page still says
//   which parents its groups sit under (walked to a fixpoint: a grandparent is
//   kept through its kept child);
//
// and cuts each kept group's `children` to groups that were kept. Group vars
// are the caller's business: this only decides membership.
//
// Generic over how the caller holds names, so a source (owned `String`s) and
// a view (borrowed `&str`s) share one implementation.
pub fn matching<'a, H, C>(
    groups: impl IntoIterator<Item = (&'a str, &'a [H], &'a [C])>,
    page: &HashSet<&str>,
) -> HashMap<&'a str, Trimmed<'a>>
where
    H: AsRef<str> + 'a,
    C: AsRef<str> + 'a,
{
    let all: Vec<(&'a str, &'a [H], &'a [C])> = groups.into_iter().collect();

    let mut kept: HashMap<&'a str, Trimmed<'a>> = HashMap::new();
    for (name, hosts, _) in &all {
        let on_page: Vec<&'a str> = hosts
            .iter()
            .map(AsRef::as_ref)
            .filter(|h| page.contains(h))
            .collect();
        if !on_page.is_empty() {
            kept.insert(
                name,
                Trimmed {
                    hosts: on_page,
                    children: Vec::new(),
                },
            );
        }
    }

    loop {
        let mut grew = false;
        for (name, _, children) in &all {
            if !kept.contains_key(name) && children.iter().any(|c| kept.contains_key(c.as_ref())) {
                kept.insert(
                    name,
                    Trimmed {
                        hosts: Vec::new(),
                        children: Vec::new(),
                    },
                );
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let names: HashSet<&'a str> = kept.keys().copied().collect();
    for (name, _, children) in &all {
        if let Some(trimmed) = kept.get_mut(name) {
            trimmed.children = children
                .iter()
                .map(AsRef::as_ref)
                .filter(|c| names.contains(c))
                .collect();
        }
    }
    kept
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trimmed<'a> {
    pub hosts: Vec<&'a str>,
    pub children: Vec<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keeps_groups_of_the_page_and_their_parents_and_trims_both_lists() {
        let web_hosts = owned(&["web1", "web2", "web3"]);
        let db_hosts = owned(&["db1"]);
        let none: Vec<String> = Vec::new();
        let dc_children = owned(&["web", "db"]);
        let all_children = owned(&["site_a"]);
        let other_hosts = owned(&["x"]);
        let groups = vec![
            ("web", web_hosts.as_slice(), none.as_slice()),
            ("db", db_hosts.as_slice(), none.as_slice()),
            ("site_a", none.as_slice(), dc_children.as_slice()),
            ("everything", none.as_slice(), all_children.as_slice()),
            ("unrelated", other_hosts.as_slice(), none.as_slice()),
        ];
        let page: HashSet<&str> = ["web2"].into_iter().collect();
        let out = matching(groups, &page);

        assert_eq!(
            out["web"],
            Trimmed {
                hosts: vec!["web2"],
                children: vec![]
            }
        );
        // the parent stays, naming only the child that stayed
        assert_eq!(
            out["site_a"],
            Trimmed {
                hosts: vec![],
                children: vec!["web"]
            }
        );
        // and so does the grandparent, through it
        assert_eq!(
            out["everything"],
            Trimmed {
                hosts: vec![],
                children: vec!["site_a"]
            }
        );
        assert!(!out.contains_key("db"));
        assert!(!out.contains_key("unrelated"));
    }

    #[test]
    fn an_empty_page_keeps_no_groups() {
        let hosts = owned(&["a"]);
        let none: Vec<String> = Vec::new();
        let out = matching([("g", hosts.as_slice(), none.as_slice())], &HashSet::new());
        assert!(out.is_empty());
    }

    #[test]
    fn works_on_borrowed_names_too() {
        let hosts: Vec<&str> = vec!["a", "b"];
        let none: Vec<&str> = vec![];
        let page: HashSet<&str> = ["b"].into_iter().collect();
        let out = matching([("g", hosts.as_slice(), none.as_slice())], &page);
        assert_eq!(out["g"].hosts, vec!["b"]);
    }
}
