//! Minimal JSON encoding.
//!
//! GitMesh exposes machine-readable results for scripting (`gitmesh status --json`).
//! Rather than pulling in a serialisation dependency, this module implements exactly
//! what GitMesh needs: building a JSON document. There is no parser, because GitMesh
//! never reads JSON.
//!
//! The encoder is written carefully around the two things that make hand-rolled JSON
//! wrong: string escaping and number formatting.

use std::fmt::Write as _;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Build an object from key/value pairs.
    pub fn object(pairs: impl IntoIterator<Item = (impl Into<String>, Json)>) -> Json {
        Json::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Build an array.
    pub fn array(items: impl IntoIterator<Item = Json>) -> Json {
        Json::Array(items.into_iter().collect())
    }

    /// `null` for `None`, the value otherwise.
    pub fn opt<T: Into<Json>>(value: Option<T>) -> Json {
        match value {
            Some(v) => v.into(),
            None => Json::Null,
        }
    }

    /// Add or replace a field of an object (no-op on other values).
    ///
    /// Used where a base document is built once and one or two fields are added
    /// conditionally, which keeps the caller from rebuilding the whole object.
    pub fn with_field(mut self, key: impl Into<String>, value: Json) -> Json {
        if let Json::Object(fields) = &mut self {
            let key = key.into();
            match fields.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, existing)) => *existing = value,
                None => fields.push((key, value)),
            }
        }
        self
    }

    /// Compact, valid JSON text.
    pub fn compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// Pretty-printed JSON text (two-space indent).
    pub fn to_pretty_string(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Json::UInt(u) => {
                let _ = write!(out, "{u}");
            }
            Json::Float(f) => {
                if f.is_finite() {
                    let _ = write!(out, "{f}");
                } else {
                    out.push_str("null");
                }
            }
            Json::String(s) => write_escaped(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty(&self, out: &mut String, indent: usize) {
        let pad = "  ".repeat(indent);
        let inner_pad = "  ".repeat(indent + 1);
        match self {
            Json::Array(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    out.push_str(&inner_pad);
                    item.write_pretty(out, indent + 1);
                }
                out.push('\n');
                out.push_str(&pad);
                out.push(']');
            }
            Json::Object(pairs) if !pairs.is_empty() => {
                out.push_str("{\n");
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    out.push_str(&inner_pad);
                    write_escaped(key, out);
                    out.push_str(": ");
                    value.write_pretty(out, indent + 1);
                }
                out.push('\n');
                out.push_str(&pad);
                out.push('}');
            }
            other => other.write(out),
        }
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Json::String(value.to_string())
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Json::String(value)
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Json::Bool(value)
    }
}

impl From<usize> for Json {
    fn from(value: usize) -> Self {
        Json::UInt(value as u64)
    }
}

impl From<u32> for Json {
    fn from(value: u32) -> Self {
        Json::UInt(value as u64)
    }
}

impl From<i64> for Json {
    fn from(value: i64) -> Self {
        Json::Int(value)
    }
}

impl From<Vec<Json>> for Json {
    fn from(value: Vec<Json>) -> Self {
        Json::Array(value)
    }
}

impl std::fmt::Display for Json {
    /// Compact, valid JSON text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        self.write(&mut out);
        f.write_str(&out)
    }
}

fn write_escaped(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_control_characters_and_quotes() {
        let value = Json::String("a\"b\\c\nd\u{1}".into());
        assert_eq!(value.compact(), "\"a\\\"b\\\\c\\nd\\u0001\"");
    }

    #[test]
    fn builds_nested_documents() {
        let doc = Json::object([
            ("name", Json::from("demo")),
            ("count", Json::from(2usize)),
            (
                "repos",
                Json::array([Json::object([("id", Json::from("root"))])]),
            ),
            ("missing", Json::opt::<&str>(None)),
        ]);
        assert_eq!(
            doc.compact(),
            "{\"name\":\"demo\",\"count\":2,\"repos\":[{\"id\":\"root\"}],\"missing\":null}"
        );
    }

    #[test]
    fn pretty_printing_is_parseable_looking() {
        let doc = Json::object([("a", Json::array([Json::from(1i64), Json::from(2i64)]))]);
        let pretty = doc.to_pretty_string();
        assert!(pretty.contains("\"a\": ["));
        assert!(pretty.lines().count() > 2);
    }

    #[test]
    fn non_finite_floats_become_null() {
        assert_eq!(Json::Float(f64::NAN).compact(), "null");
    }

    #[test]
    fn empty_containers_are_compact() {
        assert_eq!(Json::array(Vec::<Json>::new()).compact(), "[]");
        assert_eq!(Json::object(Vec::<(&str, Json)>::new()).compact(), "{}");
    }
}
