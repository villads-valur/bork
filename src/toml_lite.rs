//! Minimal hand-rolled TOML reader shared across config files.
//!
//! Supports a flat schema only:
//! - `key = "value"` (string, optional quotes)
//! - `key = true` / `key = false` (or quoted equivalents)
//! - `key = 300` (integer; quoted form also accepted)
//! - `key = ["a", "b"]` (string array, optional quotes per element)
//! - `# comment` (line or trailing inline)
//!
//! No sections, no nested tables. This matches the existing project config
//! shape and keeps us off the `toml` crate dependency.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Bool(bool),
    Int(i64),
    List(Vec<String>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::String(s) => match s.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_int().and_then(|n| u64::try_from(n).ok())
    }

    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }
}

pub type Table = HashMap<String, Value>;

/// Parse a TOML-lite document. Unknown shapes are silently skipped so partial
/// configs do not fail; callers decide which keys they care about.
pub fn parse(contents: &str) -> Table {
    let mut table = Table::new();

    for raw in contents.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = parse_value(raw_value.trim());
        table.insert(key.to_string(), value);
    }

    table
}

/// Strip an inline `#` comment, but only when the `#` is not inside a quoted
/// string. The parser is intentionally simple: arrays should not contain `#`.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn parse_value(raw: &str) -> Value {
    if let Some(list) = parse_list(raw) {
        return Value::List(list);
    }
    let token = trim_token(raw);
    if token == "true" {
        return Value::Bool(true);
    }
    if token == "false" {
        return Value::Bool(false);
    }
    if let Ok(n) = token.parse::<i64>() {
        return Value::Int(n);
    }
    Value::String(token.to_string())
}

fn parse_list(raw: &str) -> Option<Vec<String>> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let mut items = Vec::new();
    for part in inner.split(',') {
        let token = trim_token(part);
        if token.is_empty() {
            continue;
        }
        items.push(token.to_string());
    }
    Some(items)
}

fn trim_token(value: &str) -> &str {
    value.trim().trim_matches('"').trim_matches('\'').trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_scalars() {
        let t = parse(r#"name = "bork""#);
        assert_eq!(t["name"].as_str(), Some("bork"));
    }

    #[test]
    fn parses_unquoted_strings() {
        let t = parse("name = bork");
        assert_eq!(t["name"].as_str(), Some("bork"));
    }

    #[test]
    fn parses_bools() {
        let t = parse("debug = true\nverbose = false");
        assert_eq!(t["debug"].as_bool(), Some(true));
        assert_eq!(t["verbose"].as_bool(), Some(false));
    }

    #[test]
    fn parses_quoted_bools() {
        let t = parse(r#"debug = "true""#);
        assert_eq!(t["debug"].as_bool(), Some(true));
    }

    #[test]
    fn parses_ints() {
        let t = parse("ttl = 300");
        assert_eq!(t["ttl"].as_int(), Some(300));
        assert_eq!(t["ttl"].as_u64(), Some(300));
    }

    #[test]
    fn parses_quoted_ints() {
        let t = parse(r#"ttl = "600""#);
        assert_eq!(t["ttl"].as_u64(), Some(600));
    }

    #[test]
    fn parses_arrays() {
        let t = parse(r#"agents = ["claude", "opencode"]"#);
        assert_eq!(
            t["agents"].as_list(),
            Some(&["claude".to_string(), "opencode".to_string()][..])
        );
    }

    #[test]
    fn parses_arrays_with_unquoted_items() {
        let t = parse("agents = [claude, opencode]");
        assert_eq!(
            t["agents"].as_list(),
            Some(&["claude".to_string(), "opencode".to_string()][..])
        );
    }

    #[test]
    fn empty_array() {
        let t = parse("agents = []");
        assert_eq!(t["agents"].as_list(), Some(&[][..]));
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let t = parse(
            r#"
# top comment
name = "bork"   # trailing

debug = true
"#,
        );
        assert_eq!(t["name"].as_str(), Some("bork"));
        assert_eq!(t["debug"].as_bool(), Some(true));
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        let t = parse(r#"prompt = "hello # world""#);
        assert_eq!(t["prompt"].as_str(), Some("hello # world"));
    }

    #[test]
    fn skips_lines_without_equals() {
        let t = parse("not a key value pair\nname = ok");
        assert_eq!(t.len(), 1);
        assert_eq!(t["name"].as_str(), Some("ok"));
    }

    #[test]
    fn last_value_wins_on_duplicate_keys() {
        let t = parse("name = first\nname = second");
        assert_eq!(t["name"].as_str(), Some("second"));
    }

    #[test]
    fn invalid_int_falls_back_to_string() {
        let t = parse("ttl = notanumber");
        assert_eq!(t["ttl"].as_int(), None);
        assert_eq!(t["ttl"].as_str(), Some("notanumber"));
    }
}
