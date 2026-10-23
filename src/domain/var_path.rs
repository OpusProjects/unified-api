use serde_json::{Map, Value};

use crate::domain::dataset::HostVars;

// Dotted paths into hostvars: `monitor.name` is the `name` key of the
// `monitor` object. Shared by `filter` (which reads one value) and by
// `fields` / `exclude_fields` (which shape what a response carries), on the
// dataset route, on views and on the builtin endpoints, so all three read a
// path the same way.
//
// Two rules keep this compatible with every request written before paths
// existed:
//
// - A name that is a top-level key, dots and all, is that key. Hostvar names
//   are identifiers in practice, but nothing forbids `a.b` as one, and a
//   request that worked yesterday must keep selecting what it selected.
// - A path walks into objects only, never into lists. `monitor.services.name`
//   would have to mean "the name of every service", which is a projection over
//   a list rather than a path, so it simply finds nothing: a filter on it sees
//   a missing var, and `fields` leaves nothing behind for it.

// The value at `path`, or None when any step is missing or not an object.
pub fn lookup<'a>(vars: &'a HostVars, path: &str) -> Option<&'a Value> {
    if let Some(value) = vars.get(path) {
        return Some(value);
    }
    let mut segments = path.split('.');
    let mut current = vars.get(segments.next()?)?;
    for segment in segments {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

// What `fields` and `exclude_fields` ask for, parsed once per request.
//
// `keep` is `fields`: when present, a host carries only those paths. `drop` is
// `exclude_fields`, applied after `keep`, and the only half that applies to
// group vars: a var left on a group reaches its members once the consumer
// resolves the inventory, so excluding it from hosts alone would let it back in
// through the group (the rule the builtin endpoints already follow).
#[derive(Debug, Clone, Default)]
pub struct VarSelect<'p> {
    keep: Option<Vec<&'p str>>,
    drop: Vec<&'p str>,
}

impl<'p> VarSelect<'p> {
    pub fn new(
        keep: Option<impl IntoIterator<Item = &'p str>>,
        drop: impl IntoIterator<Item = &'p str>,
    ) -> Self {
        VarSelect {
            keep: keep.map(|k| k.into_iter().collect()),
            drop: drop.into_iter().collect(),
        }
    }

    // Nothing to shape: callers borrow the vars through instead of copying.
    pub fn is_identity(&self) -> bool {
        self.keep.is_none() && self.drop.is_empty()
    }

    pub fn drops_nothing(&self) -> bool {
        self.drop.is_empty()
    }

    // A host's vars, shaped by both halves.
    pub fn host(&self, vars: &HostVars) -> HostVars {
        let mut out = match &self.keep {
            None => vars.clone(),
            Some(paths) => keep_paths(vars, paths),
        };
        drop_paths(&mut out, &self.drop);
        out
    }

    // A group's vars, shaped by `exclude_fields` only.
    pub fn group_vars(&self, vars: &mut HostVars) {
        drop_paths(vars, &self.drop);
    }
}

// Build a fresh map holding only `paths`. A whole top-level key is copied as
// it is; a deeper path is copied into objects of the same names, created as
// needed, so `monitor.name,monitor.status` comes back as one `monitor`
// object with those two keys.
fn keep_paths(vars: &HostVars, paths: &[&str]) -> HostVars {
    let mut out = HostVars::new();
    for path in paths {
        if let Some(value) = vars.get(*path) {
            out.insert((*path).to_string(), value.clone());
            continue;
        }
        let Some(value) = lookup(vars, path) else {
            continue;
        };
        let segments: Vec<&str> = path.split('.').collect();
        let (root, rest) = segments
            .split_first()
            .expect("lookup found a non-empty path");
        // The whole root is already here (kept by an earlier entry): it holds
        // this path too, so there is nothing to add.
        if out.get(*root).is_some_and(|v| !v.is_object()) {
            continue;
        }
        let root_obj = out
            .entry((*root).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        insert_path(root_obj, rest, value.clone());
    }
    out
}

fn insert_path(target: &mut Value, segments: &[&str], value: Value) {
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut current = target;
    for segment in parents {
        let Some(object) = current.as_object_mut() else {
            return;
        };
        current = object
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if let Some(object) = current.as_object_mut() {
        object.insert((*last).to_string(), value);
    }
}

// Remove `paths` in place. A top-level key goes whole; a deeper path removes
// the last key from its parent object and leaves the rest of that object.
fn drop_paths(vars: &mut HostVars, paths: &[&str]) {
    for path in paths {
        if vars.remove(*path).is_some() {
            continue;
        }
        let segments: Vec<&str> = path.split('.').collect();
        let Some((root, rest)) = segments.split_first() else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        if let Some(value) = vars.get_mut(*root) {
            remove_path(value, rest);
        }
    }
}

// Remove the key at `segments` below `value`, if every step is an object.
fn remove_path(value: &mut Value, segments: &[&str]) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    match segments {
        [] => {}
        [last] => {
            object.remove(*last);
        }
        [next, rest @ ..] => {
            if let Some(child) = object.get_mut(*next) {
                remove_path(child, rest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(value: Value) -> HostVars {
        serde_json::from_value(value).unwrap()
    }

    fn sample() -> HostVars {
        vars(json!({
            "os": "linux",
            "a.b": "literal",
            "monitor": {
                "name": "motoko",
                "status": "UP",
                "services": [{"name": "Ping"}],
                "deep": {"x": 1, "y": 2}
            }
        }))
    }

    #[test]
    fn lookup_walks_objects_and_prefers_a_literal_key() {
        let v = sample();
        assert_eq!(lookup(&v, "os"), Some(&json!("linux")));
        assert_eq!(lookup(&v, "monitor.name"), Some(&json!("motoko")));
        assert_eq!(lookup(&v, "monitor.deep.y"), Some(&json!(2)));
        // a top-level key named with a dot is that key, not a path
        assert_eq!(lookup(&v, "a.b"), Some(&json!("literal")));
        assert_eq!(lookup(&v, "monitor.missing"), None);
        // never into lists
        assert_eq!(lookup(&v, "monitor.services.name"), None);
        // nor through a scalar
        assert_eq!(lookup(&v, "os.anything"), None);
    }

    #[test]
    fn keep_builds_only_the_named_paths_under_their_roots() {
        let v = sample();
        let sel = VarSelect::new(Some(["monitor.name", "monitor.deep.x", "os"]), []);
        assert_eq!(
            serde_json::to_value(sel.host(&v)).unwrap(),
            json!({"os": "linux", "monitor": {"name": "motoko", "deep": {"x": 1}}})
        );
        // a path that finds nothing adds nothing, not an empty object
        let sel = VarSelect::new(Some(["monitor.nope", "missing.path"]), []);
        assert_eq!(serde_json::to_value(sel.host(&v)).unwrap(), json!({}));
    }

    #[test]
    fn keeping_a_whole_root_and_a_path_under_it_keeps_the_whole_root() {
        let v = sample();
        for order in [["monitor", "monitor.name"], ["monitor.name", "monitor"]] {
            let sel = VarSelect::new(Some(order), []);
            assert_eq!(sel.host(&v)["monitor"], v["monitor"], "order {order:?}");
        }
    }

    #[test]
    fn drop_removes_a_nested_key_and_leaves_its_siblings() {
        let v = sample();
        let sel = VarSelect::new(
            None::<[&str; 0]>,
            ["monitor.services", "monitor.deep.x", "os"],
        );
        assert_eq!(
            serde_json::to_value(sel.host(&v)).unwrap(),
            json!({"a.b": "literal", "monitor": {"name": "motoko", "status": "UP", "deep": {"y": 2}}})
        );
    }

    #[test]
    fn drop_applies_after_keep() {
        let v = sample();
        let sel = VarSelect::new(Some(["monitor"]), ["monitor.services", "monitor.deep"]);
        assert_eq!(
            serde_json::to_value(sel.host(&v)).unwrap(),
            json!({"monitor": {"name": "motoko", "status": "UP"}})
        );
    }

    #[test]
    fn top_level_names_behave_exactly_as_before_paths() {
        let v = sample();
        let sel = VarSelect::new(Some(["os", "a.b"]), ["a.b"]);
        assert_eq!(
            serde_json::to_value(sel.host(&v)).unwrap(),
            json!({"os": "linux"})
        );
        assert!(VarSelect::new(None::<[&str; 0]>, []).is_identity());
    }
}
