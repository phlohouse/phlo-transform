//! Warehouse-side data diff.
//!
//! Diffing is model-aware: it reuses declared keys, compares candidate and base
//! relations with warehouse-side SQL, and returns structured summaries. Large
//! tables are never downloaded to the Rust process.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;

use phlo_transform_core::{Relation, SchemaChangeSafety};

use crate::adapter::Adapter;
use crate::error::EngineError;
use crate::util::now_rfc3339;

/// How a diff compares data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStrategy {
    Keyed,
    Aggregate,
    Full,
    Sampled,
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
    let base_rows = row_count(adapter.as_ref(), &request.base_relation).await?;
    let candidate_rows = row_count(adapter.as_ref(), &request.candidate_relation).await?;

    let mut summary = RowSummary {
        base_rows,
        candidate_rows,
        delta: candidate_rows - base_rows,
        ..Default::default()
    };
    let mut column_changes: BTreeMap<String, i64> = BTreeMap::new();
    let coverage;

    if request.key_columns.is_empty() {
        // Without a stable key only aggregate/row-count comparison is possible.
        coverage = "aggregate (row counts only)".to_string();
    } else {
        let (added, removed, modified, unchanged) = keyed_counts(adapter.as_ref(), request).await?;
        summary.added = added;
        summary.removed = removed;
        summary.modified = modified;
        summary.unchanged = unchanged;
        for column in &request.columns {
            let changed = column_changed(adapter.as_ref(), request, column).await?;
            if changed > 0 {
                column_changes.insert(column.clone(), changed);
            }
        }
        coverage = match request.strategy {
            DiffStrategy::Sampled => "sampled keyed (deterministic seed)".to_string(),
            DiffStrategy::Full => "full keyed".to_string(),
            _ => "keyed".to_string(),
        };
    }

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
        strategy: request.strategy,
        coverage,
        key_columns: request.key_columns.clone(),
        row_summary: summary,
        column_changes,
        schema_changes: Vec::new(),
        policy_results,
        passed,
        started_at,
        finished_at: now_rfc3339(),
    })
}

/// Attach schema changes (from the Phase 2 classifier) to a report.
pub fn attach_schema_changes(report: &mut DiffReport, safety: SchemaChangeSafety) {
    report.schema_changes.push(SchemaChange {
        column: "*".to_string(),
        kind: "schema".to_string(),
        detail: "see compiler schema classification".to_string(),
        safety: format!("{safety:?}").to_lowercase(),
    });
}

async fn row_count(adapter: &dyn Adapter, relation: &Relation) -> Result<i64, EngineError> {
    let result = adapter
        .execute(&format!("SELECT count(*) FROM {}", relation.sql()))
        .await
        .map_err(EngineError::Adapter)?;
    Ok(first_cell_i64(&result.rows))
}

async fn keyed_counts(
    adapter: &dyn Adapter,
    request: &DiffRequest,
) -> Result<(i64, i64, i64, i64), EngineError> {
    let keys = &request.key_columns;
    let join: Vec<String> = keys
        .iter()
        .map(|key| format!("b.{} IS NOT DISTINCT FROM c.{}", quote(key), quote(key)))
        .collect();
    let change_conditions: Vec<String> = request
        .columns
        .iter()
        .map(|column| format!("b.{} IS DISTINCT FROM c.{}", quote(column), quote(column)))
        .collect();
    let any_changed = if change_conditions.is_empty() {
        "false".to_string()
    } else {
        change_conditions.join(" OR ")
    };
    let first_key = quote(&keys[0]);
    let sql = format!(
        "SELECT \
           count(*) FILTER (WHERE b.{first_key} IS NULL) AS added, \
           count(*) FILTER (WHERE c.{first_key} IS NULL) AS removed, \
           count(*) FILTER (WHERE b.{first_key} IS NOT NULL AND c.{first_key} IS NOT NULL AND ({any_changed})) AS modified, \
           count(*) FILTER (WHERE b.{first_key} IS NOT NULL AND c.{first_key} IS NOT NULL AND NOT ({any_changed})) AS unchanged \
         FROM ({candidate}) c \
         FULL OUTER JOIN ({base}) b ON {join}",
        candidate = request.candidate_relation.sql(),
        base = request.base_relation.sql(),
        join = join.join(" AND "),
    );
    let result = adapter.execute(&sql).await.map_err(EngineError::Adapter)?;
    let row = result.rows.first().cloned().unwrap_or_default();
    Ok((cell(&row, 0), cell(&row, 1), cell(&row, 2), cell(&row, 3)))
}

async fn column_changed(
    adapter: &dyn Adapter,
    request: &DiffRequest,
    column: &str,
) -> Result<i64, EngineError> {
    let keys = &request.key_columns;
    let join: Vec<String> = keys
        .iter()
        .map(|key| format!("b.{} = c.{}", quote(key), quote(key)))
        .collect();
    let sql = format!(
        "SELECT count(*) FROM ({candidate}) c JOIN ({base}) b ON {join} \
         WHERE b.{column} IS DISTINCT FROM c.{column}",
        candidate = request.candidate_relation.sql(),
        base = request.base_relation.sql(),
        join = join.join(" AND "),
        column = quote(column),
    );
    let result = adapter.execute(&sql).await.map_err(EngineError::Adapter)?;
    Ok(first_cell_i64(&result.rows))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(added: i64, removed: i64, modified: i64) -> RowSummary {
        RowSummary {
            base_rows: 100,
            candidate_rows: 100 + added - removed,
            delta: added - removed,
            added,
            removed,
            modified,
            unchanged: 90,
        }
    }

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
        };
        let results = evaluate_policy(&policy, &request(true), &summary(2, 0, 2));
        assert!(results.iter().all(|result| result.passed));

        let results = evaluate_policy(&policy, &request(false), &summary(2, 0, 2));
        assert!(results.iter().any(|result| !result.passed));
    }

    #[test]
    fn removed_rows_are_gated() {
        let policy = DiffPolicy {
            max_removed_rows: Some(0),
            ..Default::default()
        };
        let results = evaluate_policy(&policy, &request(true), &summary(0, 3, 0));
        assert!(!results[0].passed);
    }
}
