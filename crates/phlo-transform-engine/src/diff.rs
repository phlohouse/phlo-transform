//! Warehouse-side data diff.
//!
//! Diffing is model-aware: it reuses declared keys, compares candidate and base
//! relations with warehouse-side SQL, and returns structured summaries. Large
//! tables are never downloaded to the Rust process.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use phlo_transform_core::{ColumnTolerance, DataType, Relation};

use crate::adapter::Adapter;
use crate::error::{AdapterError, EngineError};
use crate::util::now_rfc3339;

/// How a diff compares data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStrategy {
    Keyed,
    Aggregate,
    Full,
    Sampled,
    /// Compare at partition granularity.
    Partition {
        columns: Vec<String>,
    },
}

/// Declarative diff policy.
#[derive(Clone, Debug, Default)]
pub struct DiffPolicy {
    pub max_added_rows: Option<i64>,
    pub max_removed_rows: Option<i64>,
    pub max_modified_rows: Option<i64>,
    pub max_changed_fraction: Option<f64>,
    pub require_keyed_diff: bool,
    pub require_full_diff: bool,
    pub tolerances: BTreeMap<String, ColumnTolerance>,
}

/// A single policy evaluation result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyResult {
    pub policy: String,
    pub passed: bool,
    pub detail: String,
}

/// A request to diff two relations.
#[derive(Clone, Debug)]
pub struct DiffRequest {
    pub model: String,
    pub candidate_relation: Relation,
    pub base_relation: Relation,
    pub candidate_ref: Option<String>,
    pub base_ref: Option<String>,
    pub candidate_version: Option<String>,
    pub base_version: Option<String>,
    /// Stable key columns reused from `@key`/incremental configuration.
    pub key_columns: Vec<String>,
    /// Columns to compare value-wise.
    pub columns: Vec<String>,
    pub strategy: DiffStrategy,
    pub policy: DiffPolicy,
    pub sample_fraction: Option<f64>,
    /// Declared column renames (`candidate name` → `base name`) — turns a
    /// removed+added schema pair into a reviewable rename.
    pub renames: std::collections::BTreeMap<String, String>,
}

/// Row-level summary.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RowSummary {
    pub base_rows: i64,
    pub candidate_rows: i64,
    pub delta: i64,
    pub added: i64,
    pub removed: i64,
    pub modified: i64,
    pub unchanged: i64,
}

/// A schema change included in the diff.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaChange {
    pub column: String,
    pub kind: String,
    pub detail: String,
    pub safety: String,
}

/// A structured diff report.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffReport {
    pub diff_id: String,
    pub model: String,
    pub candidate_ref: Option<String>,
    pub base_ref: Option<String>,
    pub candidate_version: Option<String>,
    pub base_version: Option<String>,
    pub candidate_relation: String,
    pub base_relation: String,
    pub strategy: DiffStrategy,
    pub coverage: String,
    pub key_columns: Vec<String>,
    pub row_summary: RowSummary,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub column_changes: BTreeMap<String, i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schema_changes: Vec<SchemaChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions_added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions_removed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions_changed: Vec<String>,
    pub policy_results: Vec<PolicyResult>,
    pub passed: bool,
    pub started_at: String,
    pub finished_at: String,
}

/// Compute a diff using warehouse-side queries.
pub async fn diff(
    adapter: Arc<dyn Adapter>,
    request: &DiffRequest,
) -> Result<DiffReport, EngineError> {
    let started_at = now_rfc3339();
    let candidate_sql = relation_sql(&request.candidate_relation, request);
    let base_sql = relation_sql(&request.base_relation, request);

    let base_rows = row_count(adapter.as_ref(), &base_sql).await?;
    let candidate_rows = row_count(adapter.as_ref(), &candidate_sql).await?;

    let mut summary = RowSummary {
        base_rows,
        candidate_rows,
        delta: candidate_rows - base_rows,
        ..Default::default()
    };
    let mut column_changes: BTreeMap<String, i64> = BTreeMap::new();
    let mut partitions_added = Vec::new();
    let mut partitions_removed = Vec::new();
    let mut partitions_changed = Vec::new();
    let coverage;

    match &request.strategy {
        DiffStrategy::Partition { columns } => {
            let base_metadata = adapter
                .partition_counts(&request.base_relation, columns)
                .await
                .ok()
                .flatten();
            let candidate_metadata = adapter
                .partition_counts(&request.candidate_relation, columns)
                .await
                .ok()
                .flatten();
            let source;
            match (base_metadata, candidate_metadata) {
                (Some(base), Some(candidate)) => {
                    let (added, removed, changed) = partition_delta(&base, &candidate);
                    partitions_added = added;
                    partitions_removed = removed;
                    partitions_changed = changed;
                    source = "partition metadata";
                }
                _ => {
                    let (added, removed, changed) =
                        partition_summary(adapter.as_ref(), &base_sql, &candidate_sql, columns)
                            .await?;
                    partitions_added = added;
                    partitions_removed = removed;
                    partitions_changed = changed;
                    source = "partition row counts";
                }
            }
            coverage = format!(
                "partition-aware ({source}, {} partitions changed)",
                partitions_changed.len()
            );
        }
        _ => {
            if request.key_columns.is_empty() {
                coverage = if matches!(request.strategy, DiffStrategy::Sampled) {
                    "sampled aggregate (row counts only)".to_string()
                } else {
                    "aggregate (row counts only)".to_string()
                };
            } else {
                let (added, removed, modified, unchanged) =
                    keyed_counts(adapter.as_ref(), request, &base_sql, &candidate_sql).await?;
                summary.added = added;
                summary.removed = removed;
                summary.modified = modified;
                summary.unchanged = unchanged;
                for column in &request.columns {
                    let changed = column_changed(
                        adapter.as_ref(),
                        request,
                        &base_sql,
                        &candidate_sql,
                        column,
                    )
                    .await?;
                    if changed > 0 {
                        column_changes.insert(column.clone(), changed);
                    }
                }
                coverage = match request.strategy {
                    DiffStrategy::Sampled => "sampled keyed".to_string(),
                    DiffStrategy::Full => "full keyed".to_string(),
                    _ => "keyed".to_string(),
                };
            }
        }
    }

    let schema_changes = schema_changes(adapter.as_ref(), request).await;

    let policy_results = evaluate_policy(&request.policy, request, &summary);
    let passed = policy_results.iter().all(|result| result.passed);

    Ok(DiffReport {
        diff_id: uuid::Uuid::new_v4().to_string(),
        model: request.model.clone(),
        candidate_ref: request.candidate_ref.clone(),
        base_ref: request.base_ref.clone(),
        candidate_version: request.candidate_version.clone(),
        base_version: request.base_version.clone(),
        candidate_relation: request.candidate_relation.display(),
        base_relation: request.base_relation.display(),
        strategy: request.strategy.clone(),
        coverage,
        key_columns: request.key_columns.clone(),
        row_summary: summary,
        column_changes,
        schema_changes,
        partitions_added,
        partitions_removed,
        partitions_changed,
        policy_results,
        passed,
        started_at,
        finished_at: now_rfc3339(),
    })
}

/// The SQL expression for a relation, applying sampling when requested.
fn relation_sql(relation: &Relation, request: &DiffRequest) -> String {
    match (&request.strategy, request.sample_fraction) {
        (DiffStrategy::Sampled, Some(fraction)) => format!(
            "{} TABLESAMPLE BERNOULLI ({})",
            relation.sql(),
            (fraction.clamp(0.0, 1.0) * 100.0)
        ),
        _ => relation.sql(),
    }
}

async fn row_count(adapter: &dyn Adapter, relation_sql: &str) -> Result<i64, EngineError> {
    let result = adapter
        .execute(&format!("SELECT count(*) FROM {relation_sql}"))
        .await
        .map_err(EngineError::Adapter)?;
    first_cell_i64(&result.rows)
}

fn change_predicate(column: &str, tolerance: Option<&ColumnTolerance>) -> String {
    let column = quote(column);
    let exact = format!("b.{column} IS DISTINCT FROM c.{column}");
    let Some(tolerance) = tolerance else {
        return exact;
    };
    let mut within = vec![format!("b.{column} = c.{column}")];
    let difference = format!("abs(b.{column} - c.{column})");
    if let Some(absolute) = tolerance.absolute {
        within.push(format!("{difference} <= {absolute}"));
    }
    if let Some(relative) = tolerance.relative {
        within.push(format!(
            "{difference} <= {relative} * greatest(abs(b.{column}), abs(c.{column}))"
        ));
    }
    format!(
        "((b.{column} IS NULL) <> (c.{column} IS NULL)) \
         OR (b.{column} IS NOT NULL AND c.{column} IS NOT NULL AND NOT ({}))",
        within.join(" OR ")
    )
}

async fn keyed_counts(
    adapter: &dyn Adapter,
    request: &DiffRequest,
    base_sql: &str,
    candidate_sql: &str,
) -> Result<(i64, i64, i64, i64), EngineError> {
    let keys = &request.key_columns;
    let join: Vec<String> = keys
        .iter()
        .map(|key| format!("b.{} IS NOT DISTINCT FROM c.{}", quote(key), quote(key)))
        .collect();
    let conditions: Vec<String> = request
        .columns
        .iter()
        .map(|column| change_predicate(column, request.policy.tolerances.get(column)))
        .collect();
    let any_changed = if conditions.is_empty() {
        "false".to_string()
    } else {
        conditions.join(" OR ")
    };
    let first_key = quote(&keys[0]);
    let sql = format!(
        "SELECT \
           count(*) FILTER (WHERE b.{first_key} IS NULL) AS added, \
           count(*) FILTER (WHERE c.{first_key} IS NULL) AS removed, \
           count(*) FILTER (WHERE b.{first_key} IS NOT NULL AND c.{first_key} IS NOT NULL AND ({any_changed})) AS modified, \
           count(*) FILTER (WHERE b.{first_key} IS NOT NULL AND c.{first_key} IS NOT NULL AND NOT ({any_changed})) AS unchanged \
         FROM ({candidate_sql}) c \
         FULL OUTER JOIN ({base_sql}) b ON {}",
        join.join(" AND "),
    );
    let result = adapter.execute(&sql).await.map_err(EngineError::Adapter)?;
    let row = result.rows.first().ok_or_else(|| {
        EngineError::Adapter(AdapterError::new(
            "MALFORMED_RESULT",
            "count query returned no rows",
        ))
    })?;
    Ok((cell(row, 0)?, cell(row, 1)?, cell(row, 2)?, cell(row, 3)?))
}

async fn column_changed(
    adapter: &dyn Adapter,
    request: &DiffRequest,
    base_sql: &str,
    candidate_sql: &str,
    column: &str,
) -> Result<i64, EngineError> {
    let keys = &request.key_columns;
    let join: Vec<String> = keys
        .iter()
        .map(|key| format!("b.{} = c.{}", quote(key), quote(key)))
        .collect();
    let predicate = change_predicate(column, request.policy.tolerances.get(column));
    let sql = format!(
        "SELECT count(*) FROM ({candidate_sql}) c JOIN ({base_sql}) b ON {} WHERE {predicate}",
        join.join(" AND "),
    );
    let result = adapter.execute(&sql).await.map_err(EngineError::Adapter)?;
    first_cell_i64(&result.rows)
}

async fn partition_summary(
    adapter: &dyn Adapter,
    base_sql: &str,
    candidate_sql: &str,
    columns: &[String],
) -> Result<(Vec<String>, Vec<String>, Vec<String>), EngineError> {
    let casts: Vec<String> = columns
        .iter()
        .map(|column| format!("CAST({} AS varchar)", quote(column)))
        .collect();
    let key = if casts.len() == 1 {
        casts[0].clone()
    } else {
        format!("concat_ws('|', {})", casts.join(", "))
    };

    let added = adapter
        .execute(&format!(
            "SELECT {key} FROM ({candidate_sql}) EXCEPT SELECT {key} FROM ({base_sql}) ORDER BY 1"
        ))
        .await
        .map_err(EngineError::Adapter)?;
    let removed = adapter
        .execute(&format!(
            "SELECT {key} FROM ({base_sql}) EXCEPT SELECT {key} FROM ({candidate_sql}) ORDER BY 1"
        ))
        .await
        .map_err(EngineError::Adapter)?;
    let changed = adapter
        .execute(&format!(
            "SELECT c.__key FROM \
               (SELECT {key} AS __key, count(*) AS n FROM ({candidate_sql}) GROUP BY 1) c \
             JOIN \
               (SELECT {key} AS __key, count(*) AS n FROM ({base_sql}) GROUP BY 1) b \
             ON c.__key = b.__key WHERE c.n <> b.n ORDER BY 1"
        ))
        .await
        .map_err(EngineError::Adapter)?;

    Ok((
        column_values(&added.rows),
        column_values(&removed.rows),
        column_values(&changed.rows),
    ))
}

fn column_values(rows: &[Vec<String>]) -> Vec<String> {
    rows.iter().filter_map(|row| row.first().cloned()).collect()
}

fn partition_delta(
    base: &[(String, i64)],
    candidate: &[(String, i64)],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let base_map: BTreeMap<&str, i64> = base
        .iter()
        .map(|(key, count)| (key.as_str(), *count))
        .collect();
    let candidate_map: BTreeMap<&str, i64> = candidate
        .iter()
        .map(|(key, count)| (key.as_str(), *count))
        .collect();
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for (key, count) in &candidate_map {
        match base_map.get(key) {
            None => added.push((*key).to_string()),
            Some(base_count) if base_count != count => changed.push((*key).to_string()),
            Some(_) => {}
        }
    }
    for key in base_map.keys() {
        if !candidate_map.contains_key(key) {
            removed.push((*key).to_string());
        }
    }
    added.sort();
    removed.sort();
    changed.sort();
    (added, removed, changed)
}

async fn schema_changes(adapter: &dyn Adapter, request: &DiffRequest) -> Vec<SchemaChange> {
    compare_columns(
        adapter,
        &request.candidate_relation,
        &request.base_relation,
        &request.renames,
    )
    .await
}

/// Column-level comparison of two relations: added/removed columns, type
/// changes and nullability changes. Empty when either side's columns cannot
/// be read.
pub(crate) async fn compare_columns(
    adapter: &dyn Adapter,
    candidate: &phlo_transform_core::Relation,
    base: &phlo_transform_core::Relation,
    renames: &BTreeMap<String, String>,
) -> Vec<SchemaChange> {
    let Ok(candidate_columns) = adapter.relation_columns(candidate).await else {
        return Vec::new();
    };
    let Ok(base_columns) = adapter.relation_columns(base).await else {
        return Vec::new();
    };
    compare_column_lists(&candidate_columns, &base_columns, renames)
}

/// Column-level comparison of two column lists — usable when only one side
/// exists (pass an empty list for the absent side). `renames` maps a
/// candidate column name to the base column it was renamed from, so a
/// remove+add pair that is a declared rename reports as `renamed` instead of
/// a breaking removal.
pub(crate) fn compare_column_lists(
    candidate_columns: &[crate::adapter::ColumnInfo],
    base_columns: &[crate::adapter::ColumnInfo],
    renames: &BTreeMap<String, String>,
) -> Vec<SchemaChange> {
    if candidate_columns.is_empty() && base_columns.is_empty() {
        return Vec::new();
    }

    let candidate_types: BTreeMap<String, (DataType, bool)> = candidate_columns
        .iter()
        .map(|column| {
            (
                column.name.clone(),
                (DataType::parse_trino(&column.data_type), column.nullable),
            )
        })
        .collect();
    let base_types: BTreeMap<String, (DataType, bool)> = base_columns
        .iter()
        .map(|column| {
            (
                column.name.clone(),
                (DataType::parse_trino(&column.data_type), column.nullable),
            )
        })
        .collect();

    let mut changes = Vec::new();
    // Renames that actually resolve: the new name exists only on the
    // candidate side and the old name exists on the base side. A stale
    // declaration (`new` absent, `old` absent, or `new` already a base
    // column) explains nothing and must not suppress a real addition or
    // removal. Renames are one-to-one: an `old` name claimed by two new
    // columns is ambiguous — resolve none of them so the removal and both
    // additions report normally.
    let mut claims: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (new, old) in renames {
        claims.entry(old.as_str()).or_default().push(new.as_str());
    }
    let ambiguous: std::collections::BTreeSet<&str> = claims
        .iter()
        .filter(|(_, news)| news.len() > 1)
        .map(|(old, _)| *old)
        .collect();
    let resolved: std::collections::BTreeSet<&str> = renames
        .iter()
        .filter(|(new, old)| {
            !ambiguous.contains(old.as_str())
                && candidate_types.contains_key(new.as_str())
                && !base_types.contains_key(new.as_str())
                && base_types.contains_key(old.as_str())
        })
        .map(|(new, _)| new.as_str())
        .collect();
    for (old, news) in &claims {
        if ambiguous.contains(old) && base_types.contains_key(*old) {
            changes.push(SchemaChange {
                column: (*old).to_string(),
                kind: "rename_ambiguous".to_string(),
                detail: format!(
                    "declared renames for `{old}` are ambiguous: {}",
                    news.join(", ")
                ),
                safety: "review".to_string(),
            });
        }
    }
    // Base columns renamed to a new candidate name — `old -> new`.
    let renamed_away: BTreeMap<&str, &str> = renames
        .iter()
        .filter(|(new, _)| resolved.contains(new.as_str()))
        .map(|(new, old)| (old.as_str(), new.as_str()))
        .collect();
    for (name, (base_type, base_nullable)) in &base_types {
        match candidate_types.get(name) {
            None => {
                if let Some(new_name) = renamed_away.get(name.as_str()) {
                    let (new_type, _) = &candidate_types[*new_name];
                    let type_note = if new_type != base_type {
                        format!(" (type {base_type} -> {new_type})")
                    } else {
                        String::new()
                    };
                    changes.push(SchemaChange {
                        column: (*new_name).to_string(),
                        kind: "renamed".to_string(),
                        detail: format!("`{name}` renamed to `{new_name}`{type_note}"),
                        // Declaring a rename proves intent, not
                        // compatibility — consumers selecting the old name
                        // break, so it gates like a removal.
                        safety: "full_rebuild_required".to_string(),
                    });
                } else {
                    changes.push(SchemaChange {
                        column: name.clone(),
                        kind: "removed".to_string(),
                        detail: format!("{base_type} removed"),
                        safety: "full_rebuild_required".to_string(),
                    });
                }
            }
            Some((candidate_type, candidate_nullable)) => {
                // Type and nullability are independent changes — a column
                // that moved on both reports both.
                if candidate_type != base_type {
                    changes.push(SchemaChange {
                        column: name.clone(),
                        kind: "changed".to_string(),
                        detail: format!("{base_type} -> {candidate_type}"),
                        safety: if candidate_type.is_numeric() && base_type.is_numeric() {
                            "review".to_string()
                        } else {
                            "error".to_string()
                        },
                    });
                }
                if candidate_nullable != base_nullable {
                    changes.push(SchemaChange {
                        column: name.clone(),
                        kind: "nullability".to_string(),
                        detail: if *candidate_nullable {
                            "not null -> nullable".to_string()
                        } else {
                            "nullable -> not null".to_string()
                        },
                        // Downstream direction: losing the NOT NULL
                        // guarantee breaks consumers that relied on it;
                        // gaining it can only reject producer data.
                        safety: if *candidate_nullable {
                            "error".to_string()
                        } else {
                            "review".to_string()
                        },
                    });
                }
            }
        }
    }
    for (name, (candidate_type, _)) in &candidate_types {
        if !base_types.contains_key(name) && !resolved.contains(name.as_str()) {
            changes.push(SchemaChange {
                column: name.clone(),
                kind: "added".to_string(),
                detail: format!("{candidate_type} added"),
                safety: "safe".to_string(),
            });
        }
    }
    changes.sort_by(|left, right| left.column.cmp(&right.column));
    changes
}

fn evaluate_policy(
    policy: &DiffPolicy,
    request: &DiffRequest,
    summary: &RowSummary,
) -> Vec<PolicyResult> {
    let mut results = Vec::new();
    // Row-level counts only exist when the keyed path ran — aggregate or
    // partition coverage leaves added/removed/modified unmeasured, and a
    // threshold that was never measured must fail rather than pass on a
    // vacuous zero.
    let measured = !request.key_columns.is_empty()
        && !matches!(request.strategy, DiffStrategy::Partition { .. });
    let row_policy =
        |name: &'static str, metric: &'static str, value: i64, max: i64| PolicyResult {
            policy: name.to_string(),
            passed: measured && value <= max,
            detail: if measured {
                format!("{metric} {value} (max {max})")
            } else {
                format!("{metric} cannot be measured without keyed row counts")
            },
        };
    if let Some(max) = policy.max_added_rows {
        results.push(row_policy("max_added_rows", "added", summary.added, max));
    }
    if let Some(max) = policy.max_removed_rows {
        results.push(row_policy(
            "max_removed_rows",
            "removed",
            summary.removed,
            max,
        ));
    }
    if let Some(max) = policy.max_modified_rows {
        results.push(row_policy(
            "max_modified_rows",
            "modified",
            summary.modified,
            max,
        ));
    }
    if let Some(max_fraction) = policy.max_changed_fraction {
        let base = summary.base_rows.max(1) as f64;
        let fraction = (summary.added + summary.removed + summary.modified) as f64 / base;
        results.push(PolicyResult {
            policy: "max_changed_fraction".to_string(),
            passed: measured && fraction <= max_fraction,
            detail: if measured {
                format!("changed fraction {fraction:.4} (max {max_fraction})")
            } else {
                "changed fraction cannot be measured without keyed row counts".to_string()
            },
        });
    }
    if policy.require_keyed_diff {
        results.push(PolicyResult {
            policy: "require_keyed_diff".to_string(),
            passed: measured,
            detail: "a stable key is required for keyed diffing".to_string(),
        });
    }
    if policy.require_full_diff {
        results.push(PolicyResult {
            policy: "require_full_diff".to_string(),
            passed: !request.key_columns.is_empty()
                && matches!(request.strategy, DiffStrategy::Keyed | DiffStrategy::Full),
            detail: "full/keyed coverage is required".to_string(),
        });
    }
    results
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn first_cell_i64(rows: &[Vec<String>]) -> Result<i64, EngineError> {
    let value = rows.first().and_then(|row| row.first()).ok_or_else(|| {
        EngineError::Adapter(AdapterError::new(
            "MALFORMED_RESULT",
            "count query returned no rows",
        ))
    })?;
    value.parse().map_err(|_| {
        EngineError::Adapter(AdapterError::new(
            "MALFORMED_RESULT",
            format!("count query returned non-numeric value `{value}`"),
        ))
    })
}

fn cell(row: &[String], index: usize) -> Result<i64, EngineError> {
    let value = row.get(index).ok_or_else(|| {
        EngineError::Adapter(AdapterError::new(
            "MALFORMED_RESULT",
            format!("count query returned no column {index}"),
        ))
    })?;
    value.parse().map_err(|_| {
        EngineError::Adapter(AdapterError::new(
            "MALFORMED_RESULT",
            format!("count query returned non-numeric value `{value}`"),
        ))
    })
}

/// Attach a schema-safety classification (kept for compatibility).
pub fn attach_schema_changes(
    report: &mut DiffReport,
    safety: phlo_transform_core::SchemaChangeSafety,
) {
    report.schema_changes.push(SchemaChange {
        column: "*".to_string(),
        kind: "schema".to_string(),
        detail: "see compiler schema classification".to_string(),
        safety: format!("{safety:?}").to_lowercase(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(keyed: bool) -> DiffRequest {
        DiffRequest {
            model: "assay.results".to_string(),
            candidate_relation: Relation {
                catalog: None,
                schema: "cand".to_string(),
                table: "results".to_string(),
            },
            base_relation: Relation {
                catalog: None,
                schema: "base".to_string(),
                table: "results".to_string(),
            },
            candidate_ref: Some("feature".to_string()),
            base_ref: Some("main".to_string()),
            candidate_version: None,
            base_version: None,
            key_columns: if keyed {
                vec!["id".to_string()]
            } else {
                Vec::new()
            },
            columns: vec!["value".to_string()],
            strategy: DiffStrategy::Keyed,
            policy: DiffPolicy::default(),
            sample_fraction: None,
            renames: Default::default(),
        }
    }

    #[test]
    fn policy_thresholds_pass_and_fail() {
        let policy = DiffPolicy {
            max_added_rows: Some(10),
            max_removed_rows: Some(0),
            max_modified_rows: Some(5),
            max_changed_fraction: Some(0.05),
            require_keyed_diff: true,
            require_full_diff: false,
            tolerances: BTreeMap::new(),
        };
        let summary = RowSummary {
            base_rows: 100,
            candidate_rows: 100,
            added: 2,
            removed: 0,
            modified: 2,
            unchanged: 90,
            ..Default::default()
        };
        assert!(evaluate_policy(&policy, &request(true), &summary)
            .iter()
            .all(|result| result.passed));
        assert!(evaluate_policy(&policy, &request(false), &summary)
            .iter()
            .any(|result| !result.passed));
    }

    #[test]
    fn keyless_policies_fail_closed() {
        // Without a stable key, added/removed/modified are never measured —
        // a threshold evaluated against unmeasured zeros must fail, not
        // pass vacuously.
        let policy = DiffPolicy {
            max_added_rows: Some(10),
            max_changed_fraction: Some(0.5),
            require_full_diff: true,
            ..Default::default()
        };
        let summary = RowSummary::default();
        let mut keyless = request(false);
        keyless.strategy = DiffStrategy::Full;
        let results = evaluate_policy(&policy, &keyless, &summary);
        assert_eq!(results.len(), 3);
        for result in &results {
            assert!(!result.passed, "{} should fail", result.policy);
        }
        // With keys and a keyed/full strategy the same policies measure.
        let keyed = request(true);
        let results = evaluate_policy(&policy, &keyed, &summary);
        assert!(results.iter().all(|result| result.passed));
    }

    #[test]
    fn partition_coverage_cannot_satisfy_row_thresholds() {
        // A partition diff reports changed partitions, not row counts — the
        // same fail-closed rule applies.
        let policy = DiffPolicy {
            max_added_rows: Some(0),
            ..Default::default()
        };
        let mut partitioned = request(true);
        partitioned.strategy = DiffStrategy::Partition {
            columns: vec!["d".to_string()],
        };
        let results = evaluate_policy(&policy, &partitioned, &RowSummary::default());
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
    }

    #[test]
    fn column_compare_reports_type_and_nullability_together() {
        // A column that changed type AND nullability reports both changes.
        let candidate = [crate::adapter::ColumnInfo {
            name: "id".to_string(),
            data_type: "bigint".to_string(),
            nullable: true,
        }];
        let base = [crate::adapter::ColumnInfo {
            name: "id".to_string(),
            data_type: "varchar".to_string(),
            nullable: false,
        }];
        let changes = compare_column_lists(&candidate, &base, &BTreeMap::new());
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].kind, "changed");
        assert_eq!(changes[1].kind, "nullability");
        assert_eq!(changes[1].detail, "not null -> nullable");
        // Losing a NOT NULL guarantee breaks downstream consumers.
        assert_eq!(changes[1].safety, "error");
    }

    #[test]
    fn declared_rename_reports_renamed_not_removed_and_added() {
        let info = |name: &str| crate::adapter::ColumnInfo {
            name: name.to_string(),
            data_type: "bigint".to_string(),
            nullable: true,
        };
        let candidate = [info("id"), info("new_col")];
        let base = [info("id"), info("old_col")];
        let mut renames = BTreeMap::new();
        renames.insert("new_col".to_string(), "old_col".to_string());
        let changes = compare_column_lists(&candidate, &base, &renames);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "renamed");
        assert_eq!(changes[0].column, "new_col");
        // A declared rename proves intent, not compatibility — consumers
        // selecting the old name break, so it is a breaking change.
        assert_eq!(changes[0].safety, "full_rebuild_required");

        // Without the declaration the same pair is a breaking removal.
        let changes = compare_column_lists(&candidate, &base, &BTreeMap::new());
        assert!(changes.iter().any(|change| change.kind == "removed"));
        assert!(changes.iter().any(|change| change.kind == "added"));
    }

    #[test]
    fn ambiguous_rename_resolves_nothing() {
        // Two new columns claiming the same old name: resolve neither — the
        // removal stays breaking and both additions report, plus an
        // ambiguity diagnostic explains why.
        let info = |name: &str| crate::adapter::ColumnInfo {
            name: name.to_string(),
            data_type: "bigint".to_string(),
            nullable: true,
        };
        let candidate = [info("id"), info("new_a"), info("new_b")];
        let base = [info("id"), info("old_col")];
        let mut renames = BTreeMap::new();
        renames.insert("new_a".to_string(), "old_col".to_string());
        renames.insert("new_b".to_string(), "old_col".to_string());
        let changes = compare_column_lists(&candidate, &base, &renames);
        let removed = changes
            .iter()
            .find(|change| change.column == "old_col" && change.kind == "removed")
            .expect("old_col removal");
        assert_eq!(removed.safety, "full_rebuild_required");
        for new in ["new_a", "new_b"] {
            let added = changes
                .iter()
                .find(|change| change.column == new && change.kind == "added")
                .unwrap_or_else(|| panic!("{new} must report added"));
            assert_eq!(added.safety, "safe");
        }
        assert!(changes
            .iter()
            .any(|change| change.kind == "rename_ambiguous" && change.column == "old_col"));
    }

    #[test]
    fn stale_rename_neither_hides_addition_nor_removal() {
        let info = |name: &str| crate::adapter::ColumnInfo {
            name: name.to_string(),
            data_type: "bigint".to_string(),
            nullable: true,
        };
        // `old_col` was never a base column — the declaration explains
        // nothing, and the genuinely-new `new_col` still reports added.
        let candidate = [info("id"), info("new_col")];
        let base = [info("id")];
        let mut renames = BTreeMap::new();
        renames.insert("new_col".to_string(), "old_col".to_string());
        let changes = compare_column_lists(&candidate, &base, &renames);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "added");
        assert_eq!(changes[0].column, "new_col");

        // `new_col` already existed on the base side — the declaration is
        // ambiguous, so `old_col`'s disappearance stays a breaking removal.
        let candidate = [info("id"), info("new_col")];
        let base = [info("id"), info("new_col"), info("old_col")];
        let changes = compare_column_lists(&candidate, &base, &renames);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "removed");
        assert_eq!(changes[0].column, "old_col");
    }

    #[test]
    fn tolerance_predicate_uses_numeric_bounds() {
        let tolerance = ColumnTolerance {
            absolute: Some(0.001),
            relative: Some(0.01),
        };
        let predicate = change_predicate("value", Some(&tolerance));
        assert!(predicate.contains("abs("));
        assert!(predicate.contains("greatest("));
    }

    #[test]
    fn sampling_wraps_the_relation() {
        let mut request = request(true);
        request.strategy = DiffStrategy::Sampled;
        request.sample_fraction = Some(0.1);
        let sql = relation_sql(&request.candidate_relation, &request);
        assert!(sql.contains("TABLESAMPLE BERNOULLI"));
    }
}
