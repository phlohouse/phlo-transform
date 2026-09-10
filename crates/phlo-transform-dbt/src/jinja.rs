//! A tolerant scanner for Jinja delimiters in dbt SQL.
//!
//! This is not a Jinja engine. It splits model text into literal SQL segments
//! and `{{ ... }}`, `{% ... %}` and `{# ... #}` constructs so the translator
//! can classify each construct without executing it. Unterminated or nested
//! constructs are surfaced to the caller as raw text to classify as a review
//! issue rather than silently dropped.

use crate::pylit::{self, Lit};

/// One piece of a dbt SQL file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Literal SQL text.
    Text(String),
    /// `{{ expr }}`; `inner` is the expression with whitespace-control
    /// markers removed.
    Expr { raw: String, inner: String },
    /// `{% stmt %}`; `inner` is the statement body trimmed of markers.
    Stmt { raw: String, inner: String },
    /// `{# comment #}`.
    Comment(String),
}

impl Segment {
    /// The text exactly as it appeared in the source.
    pub fn raw(&self) -> &str {
        match self {
            Segment::Text(text) | Segment::Comment(text) => text,
            Segment::Expr { raw, .. } | Segment::Stmt { raw, .. } => raw,
        }
    }
}

/// Split dbt SQL into literal and Jinja segments, in order.
pub fn scan(sql: &str) -> Vec<Segment> {
    let bytes = sql.as_bytes();
    let mut segments = Vec::new();
    let mut text_start = 0usize;
    let mut pos = 0usize;

    while pos + 1 < bytes.len() {
        if bytes[pos] != b'{' {
            pos += 1;
            continue;
        }
        let (kind, close) = match bytes[pos + 1] {
            b'{' => (Kind::Expr, "}}"),
            b'%' => (Kind::Stmt, "%}"),
            b'#' => (Kind::Comment, "#}"),
            _ => {
                pos += 1;
                continue;
            }
        };
        let Some(end) = find_close(sql, pos + 2, close) else {
            // Unterminated construct: treat the remainder as literal text so
            // the emitted file still fails loudly at check time.
            break;
        };
        if pos > text_start {
            segments.push(Segment::Text(sql[text_start..pos].to_string()));
        }
        let raw = &sql[pos..end];
        let inner = strip_markers(kind, &sql[pos + 2..end - 2]);
        segments.push(match kind {
            Kind::Expr => Segment::Expr {
                raw: raw.to_string(),
                inner,
            },
            Kind::Stmt => Segment::Stmt {
                raw: raw.to_string(),
                inner,
            },
            Kind::Comment => Segment::Comment(raw.to_string()),
        });
        pos = end;
        text_start = end;
    }
    if text_start < sql.len() {
        segments.push(Segment::Text(sql[text_start..].to_string()));
    }
    segments
}

#[derive(Clone, Copy)]
enum Kind {
    Expr,
    Stmt,
    Comment,
}

/// Find the index just past the next `close` delimiter, aware that `}}` may
/// appear inside a quoted string argument (for example `{{ ref('a}}b') }}`
/// is not legal dbt anyway, so a simple scan is sufficient).
fn find_close(sql: &str, from: usize, close: &str) -> Option<usize> {
    sql[from..]
        .find(close)
        .map(|offset| from + offset + close.len())
}

/// Remove whitespace-control markers: `{{- x -}}`, `{%+ x +%}`.
fn strip_markers(_kind: Kind, inner: &str) -> String {
    let inner = inner.trim_start();
    let inner = inner.strip_prefix(['-', '+']).unwrap_or(inner).trim_end();
    let inner = inner.strip_suffix(['-', '+']).unwrap_or(inner);
    inner.trim().to_string()
}

/// A parsed `name(args)` expression.
#[derive(Clone, Debug)]
pub struct Call {
    pub name: String,
    pub positional: Vec<Lit>,
    pub keyword: Vec<(String, Lit)>,
}

impl Call {
    pub fn kwarg(&self, name: &str) -> Option<&Lit> {
        self.keyword
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// Positional string argument at `index`.
    pub fn arg(&self, index: usize) -> Option<&str> {
        self.positional.get(index).and_then(Lit::as_str)
    }
}

/// Parse `name(arg, kw = value)`; returns `None` when the expression is not a
/// simple call (for example `this`, `x.y`, or a comparison).
pub fn parse_call(inner: &str) -> Option<Call> {
    let inner = inner.trim();
    let open = inner.find('(')?;
    let name = inner[..open].trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
    {
        return None;
    }
    if !inner.ends_with(')') {
        return None;
    }
    let (positional, keyword) = pylit::parse_args(&inner[open + 1..inner.len() - 1]).ok()?;
    Some(Call {
        name: name.to_string(),
        positional,
        keyword,
    })
}

/// The first word of a `{% ... %}` statement, e.g. `if`, `for`, `macro`.
pub fn stmt_keyword(inner: &str) -> &str {
    let end = inner
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(inner.len());
    &inner[..end]
}

/// Whether an `{% if ... %}` condition is exactly `is_incremental()` —
/// optionally with surrounding parentheses.
pub fn is_incremental_condition(inner: &str) -> bool {
    let condition = inner.trim().strip_prefix("if").unwrap_or("").trim();
    let mut text = condition.to_string();
    while text.starts_with('(') && text.ends_with(')') {
        text = text[1..text.len() - 1].trim().to_string();
    }
    matches!(text.as_str(), "is_incremental()" | "is_incremental")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_segments() {
        let segments = scan("select * from {{ ref('a') }} -- hi\n");
        assert_eq!(segments.len(), 3);
        assert!(matches!(&segments[0], Segment::Text(t) if t == "select * from "));
        match &segments[1] {
            Segment::Expr { inner, .. } => assert_eq!(inner, "ref('a')"),
            other => panic!("expected expr, got {other:?}"),
        }
    }

    #[test]
    fn handles_whitespace_control() {
        let segments = scan("{%- set x = 1 -%}select 1{{+ 'y' +}}");
        match &segments[0] {
            Segment::Stmt { inner, .. } => assert_eq!(inner, "set x = 1"),
            other => panic!("expected stmt, got {other:?}"),
        }
        match &segments[2] {
            Segment::Expr { inner, .. } => assert_eq!(inner, "'y'"),
            other => panic!("expected expr, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_block_is_left_as_text() {
        let segments = scan("select {{ broken");
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], Segment::Text(_)));
    }

    #[test]
    fn parses_call_args() {
        let call = parse_call("ref('staging', 'x', v = 2)").unwrap();
        assert_eq!(call.name, "ref");
        assert_eq!(call.arg(1), Some("x"));
        assert_eq!(call.kwarg("v"), Some(&Lit::Int(2)));
    }

    #[test]
    fn non_call_is_none() {
        assert!(parse_call("this").is_none());
        assert!(parse_call("x > 1").is_none());
    }

    #[test]
    fn incremental_condition() {
        assert!(is_incremental_condition("if is_incremental()"));
        assert!(is_incremental_condition("if (is_incremental())"));
        assert!(!is_incremental_condition("if is_incremental() and x"));
        assert!(!is_incremental_condition("if target.name == 'prod'"));
    }
}
