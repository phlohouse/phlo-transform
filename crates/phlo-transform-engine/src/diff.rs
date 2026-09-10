//! Warehouse-side data diff.
//!
//! Diffing is model-aware: it reuses declared keys, compares candidate and base
//! relations with warehouse-side SQL, and returns structured summaries. Large
//! tables are never downloaded to the Rust process.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;

use phlo_transform_core::{ColumnTolerance, DataType, Relation};

use crate::adapter::Adapter;
use crate::error::EngineError;
use crate::util::now_rfc3339;

/// How a diff compares data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
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
}

/// Row-level summary.
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
pub struct SchemaChange {
    pub column: String,
    pub kind: String,
    pub detail: String,
    pub safety: String,
}

/// A structured diff report.
#[derive(Clone, Debug, Serialize)]
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
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub column_changes: BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub schema_changes: Vec<SchemaChange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub partitions_added: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub partitions_removed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
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
            let (added, removed, changed) =
                partition_summary(adapter.as_ref(), &base_sql, &candidate_sql, columns).await?;
            partitions_added = added;
            partitions_removed = removed;
            partitions_changed = changed;
            coverage = format!(
                "partition-aware ({} partitions changed)",
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
    Ok(first_cell_i64(&result.rows))
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
    let row = result.rows.first().cloned().unwrap_or_default();
    Ok((cell(&row, 0), cell(&row, 1), cell(&row, 2), cell(&row, 3)))
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
    Ok(first_cell_i64(&result.rows))
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

async fn schema_changes(adapter: &dyn Adapter, request: &DiffRequest) -> Vec<SchemaChange> {
    let Ok(candidate) = adapter.relation_columns(&request.candidate_relation).await else {
        return Vec::new();
    };
    let Ok(base) = adapter.relation_columns(&request.base_relation).await else {
        return Vec::new();
    };
    if candidate.is_empty() && base.is_empty() {
        return Vec::new();
    }

    let candidate_types: BTreeMap<String, DataType> = candidate
        .iter()
        .map(|column| {
            (
                column.name.clone(),
                DataType::parse_trino(&column.data_type),
            )
        })
        .collect();
    let base_types: BTreeMap<String, DataType> = base
        .iter()
        .map(|column| {
            (
                column.name.clone(),
                DataType::parse_trino(&column.data_type),
            )
        })
        .collect();

    let mut changes = Vec::new();
    for (name, base_type) in &base_types {
        match candidate_types.get(name) {
            None => changes.push(SchemaChange {
                column: name.clone(),
                kind: "removed".to_string(),
                detail: format!("{base_type} removed"),
                safety: "full_rebuild_required".to_string(),
            }),
            Some(candidate_type) if candidate_type != base_type => changes.push(SchemaChange {
                column: name.clone(),
                kind: "changed".to_string(),
                detail: format!("{base_type} -> {candidate_type}"),
                safety: if candidate_type.is_numeric() && base_type.is_numeric() {
                    "review".to_string()
                } else {
                    "error".to_string()
                },
            }),
            Some(_) => {}
        }
    }
    for (name, candidate_type) in &candidate_types {
        if !base_types.contains_key(name) {
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
    if let Some(max) = policy.max_added_rows {
        results.push(PolicyResult {
            policy: "max_added_rows".to_string(),
            passed: summary.added <= max,
            detail: format!("added {} (max {max})", summary.added),
        });
    }
    if let Some(max) = policy.max_removed_rows {
        results.push(PolicyResult {
            policy: "max_removed_rows".to_string(),
            passed: summary.removed <= max,
            detail: format!("removed {} (max {max})", summary.removed),
        });
    }
    if let Some(max) = policy.max_modified_rows {
        results.push(PolicyResult {
            policy: "max_modified_rows".to_string(),
            passed: summary.modified <= max,
            detail: format!("modified {} (max {max})", summary.modified),
        });
    }
    if let Some(max_fraction) = policy.max_changed_fraction {
        let base = summary.base_rows.max(1) as f64;
        let fraction = (summary.added + summary.removed + summary.modified) as f64 / base;
        results.push(PolicyResult {
            policy: "max_changed_fraction".to_string(),
            passed: fraction <= max_fraction,
            detail: format!("changed fraction {fraction:.4} (max {max_fraction})"),
        });
    }
    if policy.require_keyed_diff {
        results.push(PolicyResult {
            policy: "require_keyed_diff".to_string(),
            passed: !request.key_columns.is_empty(),
            detail: "a stable key is required for keyed diffing".to_string(),
        });
    }
    if policy.require_full_diff {
        results.push(PolicyResult {
            policy: "require_full_diff".to_string(),
            passed: matches!(request.strategy, DiffStrategy::Keyed | DiffStrategy::Full),
            detail: "full/keyed coverage is required".to_string(),
        });
    }
    results
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn first_cell_i64(rows: &[Vec<String>]) -> i64 {
    rows.first()
        .and_then(|row| row.first())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn cell(row: &[String], index: usize) -> i64 {
    row.get(index)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
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
