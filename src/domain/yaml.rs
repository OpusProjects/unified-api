use std::collections::HashMap;

use serde_json::{Map, Number, Value};
use serde_saphyr::granit_parser::{Event, Options, Parser, ScalarStyle, StrInput, Tag};

// YAML → JSON for data whose shape we do not know in advance: static
// inventories and their group_vars / host_vars files. Typed config goes
// through `serde_saphyr::from_str` instead, where the target type decides.
//
// Why not `serde_saphyr::from_str::<serde_json::Value>`: untyped, it infers
// scalar types by its own rules, and no option restores the ones 0.31 (which
// read YAML with serde_yaml_ng) applied. An unquoted `08001` or `0644` came
// back as a float (8001.0, 644.0) — a zip code or a file mode silently turned
// into a number. So this walks the parser's events and resolves each plain
// scalar the way serde_yaml_ng did, which is the YAML 1.2 core schema:
//   null   ~ null Null NULL, or nothing at all
//   bool   true True TRUE false False FALSE  (not yes/no/on/off)
//   int    decimal, 0x hex, 0o octal, 0b binary; no leading zeros
//   float  anything Rust reads as a finite f64; .inf and .nan become null,
//          as JSON has no such numbers
//   string everything else, and every quoted or block scalar
// Mapping keys are always strings: a scalar key's own text (`80:` is "80").
// `<<` is an ordinary key, as it was, and a duplicate key is an error.
// A local tag such as `!unsafe` is dropped and its value kept, except
// `!vault`: an encrypted value is an error, never data.

// Nesting deeper than this is an error rather than a stack overflow.
const MAX_DEPTH: usize = 128;
// Aliases copy their anchor's value, so a few lines can expand to billions of
// nodes. The copies may add up to this many nodes, or 100 per event read.
const MIN_ALIAS_BUDGET: usize = 1_000_000;

const CORE_TAG: &str = "tag:yaml.org,2002:";

pub fn to_json(doc: &str) -> Result<Value, String> {
    // Comments are not data; without this they arrive as events of their own.
    let mut options = Options::default();
    options.emit_comments = false;
    let mut reader = Reader {
        parser: Parser::new_from_str_with_options(doc, options),
        anchors: HashMap::new(),
        events: 0,
        copied: 0,
    };
    reader.stream()
}

struct Reader<'a> {
    parser: Parser<'a, StrInput<'a>>,
    // anchor id → (value, its node count, which is what copying it costs)
    anchors: HashMap<usize, (Value, usize)>,
    events: usize,
    copied: usize,
}

impl<'a> Reader<'a> {
    fn next(&mut self) -> Result<Event<'a>, String> {
        self.events += 1;
        match self.parser.next_event() {
            Some(Ok((event, _))) => Ok(event),
            Some(Err(e)) => Err(e.to_string()),
            None => Err("unexpected end of YAML".to_string()),
        }
    }

    // One document, or none (an empty file reads as null). A second document
    // is an error, as it was: which one would the caller want?
    fn stream(&mut self) -> Result<Value, String> {
        let mut value = Value::Null;
        let mut documents = 0;
        loop {
            match self.next()? {
                Event::StreamStart => {}
                Event::StreamEnd => return Ok(value),
                Event::DocumentStart(..) => {
                    documents += 1;
                    if documents > 1 {
                        return Err("more than one YAML document is not supported".to_string());
                    }
                    let event = self.next()?;
                    value = self.node(event, 0)?.0;
                }
                Event::DocumentEnd => {}
                _ => return Err("unexpected YAML outside a document".to_string()),
            }
        }
    }

    // Returns the value and how many nodes it holds.
    fn node(&mut self, event: Event<'a>, depth: usize) -> Result<(Value, usize), String> {
        if depth > MAX_DEPTH {
            return Err(format!("YAML nested more than {MAX_DEPTH} levels deep"));
        }
        let (value, size, anchor) = match event {
            Event::Alias(id) => {
                let (value, size) = self
                    .anchors
                    .get(&id)
                    .cloned()
                    .ok_or("YAML alias to an unknown anchor")?;
                self.copied += size;
                if self.copied > MIN_ALIAS_BUDGET.max(self.events * 100) {
                    return Err("YAML aliases expand to too many nodes".to_string());
                }
                return Ok((value, size));
            }
            Event::Scalar(text, style, anchor, tag) => {
                if tag.as_deref().is_some_and(is_vault) {
                    return Err(
                        "an ansible-vault encrypted value (!vault): unified-api cannot decrypt it"
                            .to_string(),
                    );
                }
                (scalar(&text, style, tag.as_deref()), 1, anchor)
            }
            Event::SequenceStart(_, anchor, _) => {
                let mut items = Vec::new();
                let mut size = 1;
                loop {
                    match self.next()? {
                        Event::SequenceEnd => break,
                        event => {
                            let (item, n) = self.node(event, depth + 1)?;
                            items.push(item);
                            size += n;
                        }
                    }
                }
                (Value::Array(items), size, anchor)
            }
            Event::MappingStart(_, anchor, _) => {
                let mut map = Map::new();
                let mut size = 1;
                loop {
                    let key = match self.next()? {
                        Event::MappingEnd => break,
                        Event::Scalar(text, ..) => text.into_owned(),
                        Event::Alias(id) => match self.anchors.get(&id) {
                            Some((Value::String(s), _)) => s.clone(),
                            Some((Value::Array(_) | Value::Object(_), _)) | None => {
                                return Err("YAML mapping keys must be scalars".to_string());
                            }
                            Some((other, _)) => other.to_string(),
                        },
                        _ => return Err("YAML mapping keys must be scalars".to_string()),
                    };
                    let event = self.next()?;
                    let (value, n) = self.node(event, depth + 1)?;
                    if map.insert(key.clone(), value).is_some() {
                        return Err(format!("duplicate YAML key '{key}'"));
                    }
                    size += n;
                }
                (Value::Object(map), size, anchor)
            }
            _ => return Err("unexpected YAML event".to_string()),
        };
        if anchor != 0 {
            self.anchors.insert(anchor, (value.clone(), size));
        }
        Ok((value, size))
    }
}

// Ansible's inline encrypted value. Dropping the tag like any other would
// hand the ciphertext on as if it were the value.
fn is_vault(tag: &Tag) -> bool {
    tag.handle() != CORE_TAG && tag.suffix() == "vault"
}

fn scalar(text: &str, style: ScalarStyle, tag: Option<&Tag>) -> Value {
    if let Some(tag) = tag
        && tag.handle() == CORE_TAG
        && tag.suffix() == "str"
    {
        return Value::String(text.to_string());
    }
    if style != ScalarStyle::Plain {
        return Value::String(text.to_string());
    }
    if matches!(text, "" | "~" | "null" | "Null" | "NULL") {
        return Value::Null;
    }
    if let Some(b) = match text {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    } {
        return Value::Bool(b);
    }
    if let Some(n) = int(text) {
        return Value::Number(n);
    }
    if !digits_but_not_number(text)
        && let Some(f) = float(text)
    {
        // `from_f64` is None only for a non-finite value: .inf and .nan
        return Number::from_f64(f).map_or(Value::Null, Value::Number);
    }
    Value::String(text.to_string())
}

// Leading zeros followed by digits are a string in YAML 1.2 (`007`, `08001`).
fn digits_but_not_number(text: &str) -> bool {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    digits.len() > 1 && digits.starts_with('0') && digits[1..].bytes().all(|b| b.is_ascii_digit())
}

fn int(text: &str) -> Option<Number> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    // A sign may come only once, and only before any radix prefix.
    if unsigned.starts_with(['+', '-']) {
        return None;
    }
    let (radix, digits) = if let Some(rest) = unsigned.strip_prefix("0x") {
        (16, rest)
    } else if let Some(rest) = unsigned.strip_prefix("0o") {
        (8, rest)
    } else if let Some(rest) = unsigned.strip_prefix("0b") {
        (2, rest)
    } else if digits_but_not_number(text) {
        return None;
    } else {
        (10, unsigned)
    };
    // `from_str_radix` would accept a sign of its own here (`0x-1`).
    if digits.starts_with(['+', '-']) {
        return None;
    }
    let magnitude = u64::from_str_radix(digits, radix).ok()?;
    if !negative {
        return Some(Number::from(magnitude));
    }
    // -(2^63) is the one magnitude above i64::MAX that still fits negated.
    0i64.checked_sub_unsigned(magnitude).map(Number::from)
}

fn float(text: &str) -> Option<f64> {
    let unsigned = text.strip_prefix('+').unwrap_or(text);
    if unsigned.starts_with(['+', '-']) && text.starts_with('+') {
        return None;
    }
    match unsigned {
        ".inf" | ".Inf" | ".INF" => return Some(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => return Some(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => return Some(f64::NAN),
        _ => {}
    }
    // Rust also reads `inf`, `NaN` and `1e999` (as infinity); YAML does not,
    // so a non-finite result here means the text stays a string.
    unsigned.parse::<f64>().ok().filter(|f| f.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_scalars_resolve_as_0_31_did() {
        let doc = concat!(
            "a: yes\nb: no\nc: on\nd: y\ne: true\nf: False\n",
            "g: 0644\nh: 08001\ni: 00\nj: -012\nk: 0\nl: -0\nm: 42\nn: -7\no: +3\n",
            "p: 0x1F\nq: 0o17\nr: 0b101\ns: -0x10\n",
            "t: 1.5\nu: 1e3\nv: .inf\nw: -.inf\nx: .nan\ny: inf\nz: NaN\n",
            "na: ~\nnb: null\nnc:\nnd: Null\n",
            "date: 2026-10-23\nver: 1.2.3\nport: \"80\"\nquoted: 'true'\nblock: |\n  42\n",
            "big: 18446744073709551615\nmin: -9223372036854775808\n",
        );
        assert_eq!(
            to_json(doc).unwrap(),
            json!({
                "a": "yes", "b": "no", "c": "on", "d": "y", "e": true, "f": false,
                "g": "0644", "h": "08001", "i": "00", "j": "-012", "k": 0, "l": 0,
                "m": 42, "n": -7, "o": 3,
                "p": 31, "q": 15, "r": 5, "s": -16,
                "t": 1.5, "u": 1000.0, "v": null, "w": null, "x": null, "y": "inf", "z": "NaN",
                "na": null, "nb": null, "nc": null, "nd": null,
                "date": "2026-10-23", "ver": "1.2.3", "port": "80", "quoted": "true",
                "block": "42\n",
                "big": 18446744073709551615u64, "min": -9223372036854775808i64,
            })
        );
    }

    #[test]
    fn keys_are_text_merge_keys_are_ordinary_and_tags_are_dropped() {
        let doc =
            "base: &b {x: 1}\nhost: {<<: *b, y: 2}\n80: http\ntrue: t\nsafe: !unsafe '{{ x }}'\n";
        assert_eq!(
            to_json(doc).unwrap(),
            json!({
                "base": {"x": 1}, "host": {"<<": {"x": 1}, "y": 2},
                "80": "http", "true": "t", "safe": "{{ x }}",
            })
        );
        assert_eq!(to_json("a: !!str 42\n").unwrap(), json!({"a": "42"}));
        let vaulted = "pw: !vault |\n  $ANSIBLE_VAULT;1.1;AES256\n  6162\n";
        assert!(to_json(vaulted).unwrap_err().contains("ansible-vault"));
    }

    #[test]
    fn documents_and_structure() {
        assert_eq!(to_json("").unwrap(), Value::Null);
        assert_eq!(to_json("# only a comment\n").unwrap(), Value::Null);
        assert_eq!(
            to_json("a: 1 # one\n# between\nb: [2, # two\n  3]\n").unwrap(),
            json!({"a": 1, "b": [2, 3]})
        );
        assert_eq!(
            to_json("---\n[1, [2, {a: b}]]\n").unwrap(),
            json!([1, [2, {"a": "b"}]])
        );
        assert!(
            to_json("a: 1\n---\nb: 2\n")
                .unwrap_err()
                .contains("more than one")
        );
        assert!(
            to_json("a: 1\na: 2\n")
                .unwrap_err()
                .contains("duplicate YAML key 'a'")
        );
        assert!(
            to_json("? [a]\n: 1\n")
                .unwrap_err()
                .contains("must be scalars")
        );
        assert!(to_json("a: [1\n").is_err());
    }

    #[test]
    fn errors_name_a_position_not_the_text() {
        let err = to_json("password: hunter2\n  bad: [\n").unwrap_err();
        assert!(err.contains("line"), "{err}");
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn deep_nesting_and_alias_bombs_are_refused() {
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(to_json(&deep).is_err());
        let mut bomb = String::from("a: &a [x, x, x, x, x, x, x, x, x, x]\n");
        for (i, name) in ["b", "c", "d", "e", "f", "g", "h"].iter().enumerate() {
            let prev = ["a", "b", "c", "d", "e", "f", "g"][i];
            let refs = vec![format!("*{prev}"); 10].join(", ");
            bomb.push_str(&format!("{name}: &{name} [{refs}]\n"));
        }
        assert!(to_json(&bomb).unwrap_err().contains("too many nodes"));
    }
}
