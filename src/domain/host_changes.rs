use std::collections::HashSet;

// Which hosts a sync brought into a source and which it took out: the first
// question when an inventory changes ("why is db-07 gone?"), which the host
// count alone cannot answer — 1234 before and 1234 after can still be one
// host swapped for another.
//
// The counts are exact; the names stop at NAMES_KEPT per side. The history
// keeps fifty runs per source in memory, and one sync of a source whose
// connector broke can take thousands of hosts out at once: the count says how
// bad it is, the first names say where to look.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostChanges {
    /// How many hosts the sync added
    pub added_count: usize,
    /// The first of them, sorted by name
    pub added: Vec<String>,
    /// How many hosts the sync removed
    pub removed_count: usize,
    /// The first of them, sorted by name
    pub removed: Vec<String>,
}

// How many names each side keeps. Enough to read in a history row, small
// enough that fifty rows per source stay cheap.
pub const NAMES_KEPT: usize = 20;

impl HostChanges {
    // Compares the host names a source held before a sync with the ones it
    // holds after. Names rather than the datasets themselves: the caller
    // must not keep the old dataset alive across the write, or the cache's
    // copy-on-write would copy the whole of it to apply the sync.
    pub fn between(before: &HashSet<String>, after: &HashSet<String>) -> Self {
        let (added_count, added) = missing_from(after, before);
        let (removed_count, removed) = missing_from(before, after);
        Self {
            added_count,
            added,
            removed_count,
            removed,
        }
    }

    // Whether the sync changed which hosts the source holds. A sync that only
    // updated variables did not, and its history row says nothing about it.
    pub fn is_empty(&self) -> bool {
        self.added_count == 0 && self.removed_count == 0
    }
}

// The hosts of `from` that `other` does not have: how many, and the first
// NAMES_KEPT of them by name.
fn missing_from(from: &HashSet<String>, other: &HashSet<String>) -> (usize, Vec<String>) {
    let mut names: Vec<&String> = from.difference(other).collect();
    let count = names.len();
    // Sorted before the cut, so the names kept are always the same ones for
    // the same change — a HashMap's order would pick a different twenty on
    // every run.
    names.sort_unstable();
    let kept = names.into_iter().take(NAMES_KEPT).cloned().collect();
    (count, kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn names_the_hosts_added_and_removed() {
        let changes = HostChanges::between(&hosts(&["a", "b", "c"]), &hosts(&["b", "c", "d"]));
        assert_eq!(changes.added_count, 1);
        assert_eq!(changes.added, vec!["d"]);
        assert_eq!(changes.removed_count, 1);
        assert_eq!(changes.removed, vec!["a"]);
        assert!(!changes.is_empty());
    }

    #[test]
    fn the_same_hosts_are_no_change() {
        let changes = HostChanges::between(&hosts(&["a", "b"]), &hosts(&["b", "a"]));
        assert!(changes.is_empty());
        assert_eq!(changes, HostChanges::default());
    }

    // The count stays exact past the cut, and the names kept are the first
    // ones by name, whatever order the map holds them in.
    #[test]
    fn keeps_the_first_names_and_the_whole_count() {
        let before: Vec<String> = (0..50).map(|i| format!("host-{:02}", i)).collect();
        let before: Vec<&str> = before.iter().map(String::as_str).collect();
        let changes = HostChanges::between(&hosts(&before), &hosts(&[]));
        assert_eq!(changes.removed_count, 50);
        assert_eq!(changes.removed.len(), NAMES_KEPT);
        assert_eq!(changes.removed[0], "host-00");
        assert_eq!(changes.removed[NAMES_KEPT - 1], "host-19");
        assert_eq!(changes.added_count, 0);
        assert!(changes.added.is_empty());
    }
}
