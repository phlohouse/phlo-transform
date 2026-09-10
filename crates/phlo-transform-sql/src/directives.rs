//! Model header directive parsing.
//!
//! Directives are ordinary SQL line comments of the form `-- @name value`.
//! They are parsed independently of SQL syntax so that malformed metadata is
//! reported clearly and so that directives never become a programming
//! language.
//!
//! Phase 0 gave meaning to `@id`. Phase 1 adds the declarative metadata needed
//! by the MVP build engine: `@view`, `@table`, `@materialized`, `@tags` and
//! `@owner`. Unknown directives are reported as warnings rather than silently
//! ignored, which keeps typos visible without blocking forward compatibility.

use serde::Serialize;

/// How a model is physically materialised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Materialization {
    View,
    Table,
    Incremental,
}

impl Materialization {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "view" => Some(Materialization::View),
            "table" => Some(Materialization::Table),
            "incremental" => Some(Materialization::Incremental),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Materialization::View => "view",
            Materialization::Table => "table",
            Materialization::Incremental => "incremental",
        }
    }
}

/// Declarative incremental intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum IncrementalStrategy {
    Append,
    Key {
        columns: Vec<String>,
    },
    Partition {
        columns: Vec<String>,
    },
    TimeWindow {
        column: String,
        overlap_seconds: Option<u64>,
    },
}

impl IncrementalStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            IncrementalStrategy::Append => "append",
            IncrementalStrategy::Key { .. } => "key",
            IncrementalStrategy::Partition { .. } => "partition",
            IncrementalStrategy::TimeWindow { .. } => "time-window",
        }
    }

    /// The identity/partition columns this strategy uses.
    pub fn columns(&self) -> Vec<String> {
        match self {
            IncrementalStrategy::Append => Vec::new(),
            IncrementalStrategy::Key { columns } | IncrementalStrategy::Partition { columns } => {
                columns.clone()
            }
            IncrementalStrategy::TimeWindow { column, .. } => vec![column.clone()],
        }
    }
}

impl std::fmt::Display for Materialization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Directives extracted from a model's SQL text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Directives {
    /// Raw value of a pinned `-- @id <name>` directive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_id: Option<String>,
    /// Requested materialisation, if declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialization: Option<Materialization>,
    /// `-- @tags a,b` values, in declaration order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `-- @owner <value>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// `-- @key <column>` identity columns.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// `-- @not-null a,b` columns asserted non-null.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_null: Vec<String>,
    /// `-- @incremental ...` intent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incremental: Option<IncrementalStrategy>,
    /// Non-fatal or fatal problems encountered while parsing directives.
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
    /// A directive carried a value that is not understood.
    InvalidValue,
    /// The same `@id` directive was declared more than once.
    DuplicateId,
    /// Conflicting materialisation directives (for example `@view` and
    /// `@table`).
    ConflictingMaterialization,
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
            "@id" => parse_id(&mut directives, name, value, line_number),
            "@view" => {
                set_materialization(&mut directives, Materialization::View, name, line_number)
            }
            "@table" => {
                set_materialization(&mut directives, Materialization::Table, name, line_number)
            }
            "@materialized" => {
                if value.trim().is_empty() {
                    missing_value(&mut directives, name, line_number);
                } else if let Some(materialization) = Materialization::parse(&value) {
                    set_materialization(&mut directives, materialization, name, line_number);
                } else {
                    directives.issues.push(DirectiveIssue {
                        kind: DirectiveIssueKind::InvalidValue,
                        name,
                        line: line_number,
                    });
                }
            }
            "@tags" => {
                let tags: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_string)
                    .collect();
                if tags.is_empty() {
                    missing_value(&mut directives, name, line_number);
                } else {
                    directives.tags.extend(tags);
                }
            }
            "@owner" => {
                let owner = value.trim();
                if owner.is_empty() {
                    missing_value(&mut directives, name, line_number);
                } else {
                    directives.owner = Some(owner.to_string());
                }
            }
            "@key" => {
                let columns = split_columns(&value);
                if columns.is_empty() {
                    missing_value(&mut directives, name, line_number);
                } else {
                    directives.keys.extend(columns);
                }
            }
            "@not-null" => {
                let columns = split_columns(&value);
                if columns.is_empty() {
                    missing_value(&mut directives, name, line_number);
                } else {
                    directives.not_null.extend(columns);
                }
            }
            "@incremental" => {
                parse_incremental(&mut directives, name, value, line_number);
            }
            _ => directives.issues.push(DirectiveIssue {
                kind: DirectiveIssueKind::UnknownDirective,
                name,
                line: line_number,
            }),
        }
    }

    directives.tags.sort();
    directives.tags.dedup();
    directives.keys.sort();
    directives.keys.dedup();
    directives.not_null.sort();
    directives.not_null.dedup();
    directives
}

/// Split a comma/whitespace separated column list into trimmed, non-empty names.
fn split_columns(value: &str) -> Vec<String> {
    value
        .split(',')
        .flat_map(|part| part.split_whitespace())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_incremental(directives: &mut Directives, name: String, value: String, line: usize) {
    let value = value.trim();
    if value.is_empty() {
        missing_value(directives, name, line);
        return;
    }

    let strategy = if let Some((key, columns)) = value.split_once('=') {
        let columns = split_columns(columns);
        match key.trim().to_ascii_lowercase().as_str() {
            "key" if !columns.is_empty() => Some(IncrementalStrategy::Key { columns }),
            "partition" if !columns.is_empty() => Some(IncrementalStrategy::Partition { columns }),
            "window" if !columns.is_empty() => Some(IncrementalStrategy::TimeWindow {
                column: columns[0].clone(),
                overlap_seconds: None,
            }),
            _ => None,
        }
    } else {
        match value.split_whitespace().next().map(str::to_ascii_lowercase) {
            Some(word) if word == "append" => Some(IncrementalStrategy::Append),
            _ => None,
        }
    };

    match strategy {
        Some(strategy) => {
            if let IncrementalStrategy::Key { columns } = &strategy {
                // A key declaration also implies identity semantics.
                directives.keys.extend(columns.iter().cloned());
                directives.keys.sort();
                directives.keys.dedup();
            }
            directives.incremental = Some(strategy);
            set_materialization(directives, Materialization::Incremental, name, line);
        }
        None => directives.issues.push(DirectiveIssue {
            kind: DirectiveIssueKind::InvalidValue,
            name,
            line,
        }),
    }
}

fn parse_id(directives: &mut Directives, name: String, value: String, line: usize) {
    let value = value.trim();
    if value.is_empty() {
        missing_value(directives, name, line);
    } else if directives.pinned_id.is_some() {
        directives.issues.push(DirectiveIssue {
            kind: DirectiveIssueKind::DuplicateId,
            name,
            line,
        });
    } else {
        // First declaration wins, keeping identity stable.
        directives.pinned_id = Some(value.to_string());
    }
}

fn set_materialization(
    directives: &mut Directives,
    materialization: Materialization,
    name: String,
    line: usize,
) {
    match directives.materialization {
        Some(existing) if existing != materialization => {
            directives.issues.push(DirectiveIssue {
                kind: DirectiveIssueKind::ConflictingMaterialization,
                name,
                line,
            });
        }
        Some(_) => {}
        None => directives.materialization = Some(materialization),
    }
}

fn missing_value(directives: &mut Directives, name: String, line: usize) {
    directives.issues.push(DirectiveIssue {
        kind: DirectiveIssueKind::MissingValue,
        name,
        line,
    });
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
        let directives = parse_directives("-- @resource heavy\nselect 1");
        assert_eq!(
            directives.issues[0].kind,
            DirectiveIssueKind::UnknownDirective
        );
        assert_eq!(directives.issues[0].name, "@resource");
    }

    #[test]
    fn requires_at_least_one_name_character() {
        assert!(parse_line("-- @ value").is_none());
        assert!(parse_line("-- @id value").is_some());
    }

    #[test]
    fn parses_materialization_short_forms() {
        assert_eq!(
            parse_directives("-- @view\nselect 1").materialization,
            Some(Materialization::View)
        );
        assert_eq!(
            parse_directives("-- @table\nselect 1").materialization,
            Some(Materialization::Table)
        );
    }

    #[test]
    fn parses_materialization_long_form() {
        assert_eq!(
            parse_directives("-- @materialized table\nselect 1").materialization,
            Some(Materialization::Table)
        );
        let directives = parse_directives("-- @materialized nonsense\nselect 1");
        assert_eq!(directives.materialization, None);
        assert_eq!(directives.issues[0].kind, DirectiveIssueKind::InvalidValue);
    }

    #[test]
    fn reports_conflicting_materialization() {
        let directives = parse_directives("-- @view\n-- @table\nselect 1");
        assert_eq!(directives.materialization, Some(Materialization::View));
        assert_eq!(
            directives.issues[0].kind,
            DirectiveIssueKind::ConflictingMaterialization
        );
    }

    #[test]
    fn parses_tags_and_owner() {
        let directives =
            parse_directives("-- @tags qc, gold\n-- @owner analytical-development\nselect 1");
        assert_eq!(directives.tags, vec!["gold", "qc"]);
        assert_eq!(directives.owner.as_deref(), Some("analytical-development"));
    }

    #[test]
    fn parses_key_and_not_null() {
        let directives =
            parse_directives("-- @key experiment_id\n-- @not-null sample_id,result\nselect 1");
        assert_eq!(directives.keys, vec!["experiment_id"]);
        assert_eq!(directives.not_null, vec!["result", "sample_id"]);
    }

    #[test]
    fn reports_key_without_value() {
        let directives = parse_directives("-- @key\nselect 1");
        assert_eq!(directives.issues[0].kind, DirectiveIssueKind::MissingValue);
    }

    #[test]
    fn parses_incremental_strategies() {
        assert_eq!(
            parse_directives("-- @incremental append\nselect 1").incremental,
            Some(IncrementalStrategy::Append)
        );
        assert_eq!(
            parse_directives("-- @incremental key=a,b\nselect 1").incremental,
            Some(IncrementalStrategy::Key {
                columns: vec!["a".to_string(), "b".to_string()]
            })
        );
        assert_eq!(
            parse_directives("-- @incremental partition=run_date\nselect 1").incremental,
            Some(IncrementalStrategy::Partition {
                columns: vec!["run_date".to_string()]
            })
        );
        assert_eq!(
            parse_directives("-- @incremental window=updated_at\nselect 1").incremental,
            Some(IncrementalStrategy::TimeWindow {
                column: "updated_at".to_string(),
                overlap_seconds: None
            })
        );
    }

    #[test]
    fn incremental_key_implies_identity() {
        let directives = parse_directives("-- @incremental key=experiment_id\nselect 1");
        assert_eq!(
            directives.materialization,
            Some(Materialization::Incremental)
        );
        assert_eq!(directives.keys, vec!["experiment_id"]);
    }

    #[test]
    fn invalid_incremental_is_reported() {
        let directives = parse_directives("-- @incremental nonsense\nselect 1");
        assert_eq!(directives.issues[0].kind, DirectiveIssueKind::InvalidValue);
    }
}
