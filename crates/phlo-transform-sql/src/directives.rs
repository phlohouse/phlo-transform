//! Model header directive parsing.
//!
//! Directives are ordinary SQL line comments of the form `-- @name value`.
//! They are parsed independently of SQL syntax so that malformed metadata is
//! reported clearly and so that directives never become a programming
//! language. Phase 0 only gives meaning to `@id`; other directives are
//! recognised as unknown and reported as warnings rather than silently
//! ignored, which keeps typos visible without blocking forward compatibility.

use serde::Serialize;

/// Directives extracted from a model's SQL text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Directives {
    /// Raw value of a pinned `-- @id <name>` directive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_id: Option<String>,
    /// Non-fatal problems encountered while parsing directives.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<DirectiveIssue>,
}

/// A problem found while parsing a directive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DirectiveIssue {
    pub kind: DirectiveIssueKind,
    /// Directive name including the leading `@`.
    pub name: String,
    /// 1-based line number.
    pub line: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveIssueKind {
    /// A known directive was present but lacked a required value.
    MissingValue,
    /// The same `@id` directive was declared more than once.
    DuplicateId,
    /// The directive name is not recognised by this compiler version.
    UnknownDirective,
}

/// Extract directives from the raw SQL text.
pub fn parse_directives(sql: &str) -> Directives {
    let mut directives = Directives::default();

    for (index, line) in sql.lines().enumerate() {
        let line_number = index + 1;
        let Some((name, value)) = parse_line(line) else {
            continue;
        };

        match name.as_str() {
            "@id" => {
                let value = value.trim();
                if value.is_empty() {
                    directives.issues.push(DirectiveIssue {
                        kind: DirectiveIssueKind::MissingValue,
                        name,
                        line: line_number,
                    });
                } else if directives.pinned_id.is_some() {
                    directives.issues.push(DirectiveIssue {
                        kind: DirectiveIssueKind::DuplicateId,
                        name,
                        line: line_number,
                    });
                } else {
                    // First declaration wins, keeping identity stable.
                    directives.pinned_id = Some(value.to_string());
                }
            }
            _ => directives.issues.push(DirectiveIssue {
                kind: DirectiveIssueKind::UnknownDirective,
                name,
                line: line_number,
            }),
        }
    }

    directives
}

/// Parse a single line into `(name, value)` if it is a directive comment.
///
/// Recognises `-- @name`, `--@name`, and trailing whitespace. The value is
/// everything after the name.
fn parse_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let body = trimmed.strip_prefix("--")?.trim_start();
    let body = body.strip_prefix('@')?;

    let name_end = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(body.len());
    if name_end == 0 {
        return None;
    }

    let name = format!("@{}", &body[..name_end]);
    let value = body[name_end..].trim().to_string();
    Some((name, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pinned_id() {
        let directives = parse_directives("-- @id assay.raw\nselect 1");
        assert_eq!(directives.pinned_id.as_deref(), Some("assay.raw"));
        assert!(directives.issues.is_empty());
    }

    #[test]
    fn ignores_regular_comments_and_sql() {
        let directives = parse_directives("-- a comment\nselect 1 -- nothing\n");
        assert_eq!(directives.pinned_id, None);
        assert!(directives.issues.is_empty());
    }

    #[test]
    fn reports_missing_value() {
        let directives = parse_directives("-- @id\nselect 1");
        assert_eq!(directives.pinned_id, None);
        assert_eq!(directives.issues.len(), 1);
        assert_eq!(directives.issues[0].kind, DirectiveIssueKind::MissingValue);
        assert_eq!(directives.issues[0].line, 1);
    }

    #[test]
    fn reports_duplicate_id() {
        let directives = parse_directives("-- @id a.b\n-- @id a.c\nselect 1");
        assert_eq!(directives.pinned_id.as_deref(), Some("a.b"));
        assert_eq!(directives.issues[0].kind, DirectiveIssueKind::DuplicateId);
    }

    #[test]
    fn reports_unknown_directive() {
        let directives = parse_directives("-- @table\nselect 1");
        assert_eq!(
            directives.issues[0].kind,
            DirectiveIssueKind::UnknownDirective
        );
        assert_eq!(directives.issues[0].name, "@table");
    }

    #[test]
    fn requires_at_least_one_name_character() {
        assert!(parse_line("-- @ value").is_none());
        assert!(parse_line("-- @id value").is_some());
    }
}
