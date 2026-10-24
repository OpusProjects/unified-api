use std::cmp::Ordering;

use crate::domain::dataset::HostVars;
use crate::domain::var_path;

// A host filter: which hosts of an inventory a reader wants, decided by the
// host's NAME and by its hostvars. Shared by `GET /sources/{id}/dataset` (and
// views, which answer on the same route) and by the builtin endpoint
// transformers, so the two spell the same filter the same way.
//
// Two parts, both optional, ANDed together:
//   host   — comma-separated name patterns, `*` and `?` wildcards,
//            case-insensitive; a host matching ANY pattern is kept
//   filter — `;`-separated conditions on hostvars, ALL must hold; a
//            dotted key walks into objects (`monitor.status`): `key<op>value` with op one of
//              =  !=   exact (a var's text form, see `text`)
//              ~  !~   contains, case-insensitive
//              >  >=  <  <=   numeric when both sides are numbers,
//                             otherwise text (so ISO dates compare)
//            `key=` (empty value) matches a missing or null var, or an
//            empty string, list or map, and `key!=` any other; the other
//            operators need a value.
//
// `;` rather than `,` separates conditions because a value may well contain
// a comma; a query string carries one value per name, so a list has to be
// joined somehow, and a POST body may send a JSON list instead (the caller
// joins it before parsing).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostFilter {
    patterns: Vec<String>,
    conditions: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq)]
struct Condition {
    key: String,
    op: Op,
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Contains,
    NotContains,
    Gt,
    Ge,
    Lt,
    Le,
}

// Two-character operators first: `>=` must not be read as `>` followed by a
// value starting with `=`.
const OPERATORS: [(&str, Op); 8] = [
    ("!=", Op::Ne),
    ("!~", Op::NotContains),
    (">=", Op::Ge),
    ("<=", Op::Le),
    ("=", Op::Eq),
    ("~", Op::Contains),
    (">", Op::Gt),
    ("<", Op::Lt),
];

impl HostFilter {
    // Parse both parts. An empty string means "no constraint" for that part,
    // so `parse("", "")` is the filter that keeps everything.
    pub fn parse(host: &str, filter: &str) -> Result<HostFilter, String> {
        let conditions: Vec<&str> = filter.split(';').collect();
        HostFilter::from_conditions(host, &conditions)
    }

    // The same, with the conditions already apart — a JSON list, where each
    // entry is one condition and a `;` inside it is part of its value.
    pub fn from_conditions(host: &str, conditions: &[&str]) -> Result<HostFilter, String> {
        let patterns = host
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_lowercase)
            .collect();
        let conditions = conditions
            .iter()
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
            .map(parse_condition)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(HostFilter {
            patterns,
            conditions,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty() && self.conditions.is_empty()
    }

    pub fn matches(&self, host: &str, vars: &HostVars) -> bool {
        if !self.patterns.is_empty() {
            let name = host.to_lowercase();
            if !self.patterns.iter().any(|p| glob(p, &name)) {
                return false;
            }
        }
        self.conditions.iter().all(|c| c.holds(vars))
    }
}

fn parse_condition(raw: &str) -> Result<Condition, String> {
    // The key ends where the first operator character starts. Hostvar names
    // are identifiers in practice, so none of `=!~<>` belongs in one.
    let start = raw
        .find(['=', '!', '~', '<', '>'])
        .ok_or_else(|| format!("filter condition '{raw}' has no operator (= != ~ !~ > >= < <=)"))?;
    let key = raw[..start].trim();
    if key.is_empty() {
        return Err(format!(
            "filter condition '{raw}' has no key before the operator"
        ));
    }
    let rest = &raw[start..];
    let (token, op) = OPERATORS
        .iter()
        .find(|(token, _)| rest.starts_with(token))
        .ok_or_else(|| format!("filter condition '{raw}' has an unknown operator"))?;
    let value = rest[token.len()..].trim();
    // Only `=` and `!=` mean something against nothing (empty / not empty);
    // `os~` would match every host, which is never what was meant.
    if value.is_empty() && !matches!(op, Op::Eq | Op::Ne) {
        return Err(format!(
            "filter condition '{raw}' has no value after '{token}'"
        ));
    }
    Ok(Condition {
        key: key.to_string(),
        op: *op,
        value: value.to_string(),
    })
}

// A hostvar as text: strings as they are, numbers and booleans as JSON writes
// them (`true`, `42`), anything structured as compact JSON, and a missing or
// null var as the empty string.
fn text(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

// What `key=` matches: no var, null, or an empty string, list or map.
fn is_empty(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(s)) => s.is_empty(),
        Some(serde_json::Value::Array(a)) => a.is_empty(),
        Some(serde_json::Value::Object(o)) => o.is_empty(),
        Some(_) => false,
    }
}

impl Condition {
    fn holds(&self, vars: &HostVars) -> bool {
        // A dotted key walks into objects (`monitor.status`); a top-level key
        // of that exact name still wins. See domain::var_path.
        let var = var_path::lookup(vars, &self.key);
        if self.value.is_empty() {
            return (self.op == Op::Eq) == is_empty(var);
        }
        let actual = text(var);
        match self.op {
            Op::Eq => actual == self.value,
            Op::Ne => actual != self.value,
            Op::Contains => actual.to_lowercase().contains(&self.value.to_lowercase()),
            Op::NotContains => !actual.to_lowercase().contains(&self.value.to_lowercase()),
            // An ordering against nothing is false either way round: a host
            // with no value is neither older nor newer than a date.
            Op::Gt | Op::Ge | Op::Lt | Op::Le if actual.is_empty() => false,
            Op::Gt => compare(&actual, &self.value) == Ordering::Greater,
            Op::Ge => compare(&actual, &self.value) != Ordering::Less,
            Op::Lt => compare(&actual, &self.value) == Ordering::Less,
            Op::Le => compare(&actual, &self.value) != Ordering::Greater,
        }
    }
}

// Numbers compare as numbers (`9 < 10`), everything else as text, which is
// what makes ISO 8601 dates and times order correctly.
fn compare(actual: &str, wanted: &str) -> Ordering {
    // Finite numbers only: Rust also reads `inf` and `NaN` as f64, and a host
    // called that, or a var saying so, is text.
    let number = |s: &str| s.parse::<f64>().ok().filter(|f| f.is_finite());
    match (number(actual), number(wanted)) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        _ => actual.cmp(wanted),
    }
}

// Glob match with `*` (any run, including none) and `?` (one character), on
// chars rather than bytes so a non-ASCII host name cannot split a character.
// The classic two-pointer walk: remember the last `*` and where the text was
// then, and on a mismatch let that star swallow one more character.
fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: serde_json::Value) -> HostVars {
        serde_json::from_value(pairs).unwrap()
    }

    #[test]
    fn host_patterns_are_globs_any_of_them_case_insensitive() {
        let f = HostFilter::parse("aa1*, *.DC4.pqe ,web0?", "").unwrap();
        let v = HostVars::new();
        assert!(f.matches("aa1pan01", &v));
        assert!(f.matches("db01.dc4.pqe", &v));
        assert!(f.matches("web07", &v));
        assert!(!f.matches("web107", &v));
        assert!(!f.matches("db01.aa1.pqe", &v));
    }

    #[test]
    fn conditions_are_anded_and_typed_by_their_text_form() {
        let v = vars(serde_json::json!({
            "power": "poweredOff", "mem_mb": 16384, "patched": false,
            "last": "2026-06-30", "owner": null, "os": "Oracle Linux 8"
        }));
        let ok = |expr: &str| HostFilter::parse("", expr).unwrap().matches("h", &v);
        assert!(ok("power=poweredOff;mem_mb>=16384"));
        assert!(!ok("power=poweredOff;mem_mb>16384"));
        assert!(ok("patched=false"));
        assert!(ok("mem_mb>9000"), "numbers compare as numbers, not text");
        assert!(ok("last<2026-07-01"), "ISO dates compare as text");
        assert!(ok("os~linux") && ok("os!~windows"));
        assert!(
            ok("owner=") && ok("missing="),
            "null and missing read as empty"
        );
        assert!(ok("power!=") && !ok("owner!="));
        let empty = vars(serde_json::json!({"tags": [], "meta": {}, "name": ""}));
        let on_empty = |expr: &str| HostFilter::parse("", expr).unwrap().matches("h", &empty);
        assert!(on_empty("tags=") && on_empty("meta=") && on_empty("name="));
        assert!(!on_empty("tags!="));
        assert!(ok("power>inf"), "inf is text, and 'p' sorts after 'i'");
        assert!(
            !ok("missing<2026-01-01"),
            "an ordering against nothing is false"
        );
    }

    #[test]
    fn malformed_conditions_are_errors_naming_them() {
        assert!(
            HostFilter::parse("", "power")
                .unwrap_err()
                .contains("no operator")
        );
        assert!(HostFilter::parse("", "=x").unwrap_err().contains("no key"));
        assert!(
            HostFilter::parse("", "os~")
                .unwrap_err()
                .contains("no value")
        );
        assert!(
            HostFilter::parse("", "n>")
                .unwrap_err()
                .contains("no value")
        );
        assert!(
            HostFilter::parse("", "a=1;;").is_ok(),
            "empty pieces are ignored"
        );
        assert!(HostFilter::parse("", "").unwrap().is_empty());
    }

    #[test]
    fn glob_backtracks() {
        assert!(glob("a*b*c", "axxbyyc"));
        assert!(glob("*", ""));
        assert!(!glob("a*b", "acd"));
        assert!(glob("ä?c", "äbc"));
    }
}
