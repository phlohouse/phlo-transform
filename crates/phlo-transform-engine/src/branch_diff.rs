//! Structured branch comparison.
//!
//! A branch diff compares the materialised datasets of two references — a
//! candidate (usually a development/CI branch) against a base (usually
//! `main`). It is model-aware: dataset status is decided from recorded
//! materialisations plus live relation-existence checks, schema changes are
//! column-level, and row counts come from warehouse-side `count(*)` queries.
//!
//! Dataset status rules (a Nessie branch inherits base tables, so a relation
//! visible on both sides with no candidate record is `unchanged` — the data
//! physically *is* the base's):
//!
//! ```text
//! candidate only                 -> added
//! base only                      -> removed
//! neither                        -> absent
//! both, recorded versions differ -> changed
//! both, otherwise                -> unchanged
//! ```
//!
//! The report carries per-dataset `upstream` dependencies and version hashes
//! as lineage hooks for graph-level diffing.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use phlo_transform_core::{Compilation, Relation};

use crate::adapter::Adapter;
use crate::diff::{
    compare_column_lists, diff, DiffReport, DiffRequest, DiffStrategy, SchemaChange,
};
use crate::error::{AdapterError, EngineError};
use crate::state::{MaterializedRecord, SeedRecord, StateStore};
use crate::util::{cmp_rfc3339, now_rfc3339};

/// Empty rename map shared by datasets whose model declares no renames.
static EMPTY_RENAMES: BTreeMap<String, String> = BTreeMap::new();

/// What a dataset is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetKind {
    Model,
    Seed,
}

/// How a dataset differs between the base and the candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetStatus {
    /// Materialised on the candidate only.
    Added,
    /// Materialised on the base only.
    Removed,
    /// Present on both, recorded versions differ.
    Changed,
    /// Present on both with equal (or inherited) content.
    Unchanged,
    /// In the workspace but materialised on neither side.
    Absent,
}

/// One dataset's branch comparison.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatasetDiff {
    pub dataset: String,
    pub kind: DatasetKind,
    pub status: DatasetStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_relation: Option<String>,
    /// Upstream model dependencies — the lineage hook for graph diffs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstream: Vec<String>,
}

/// Column-level changes for one model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelSchemaDiff {
    pub model: String,
    pub changes: Vec<SchemaChange>,
}

/// Contract-level changes for one model — what promotion would apply to
/// the base environment's recorded contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelContractDiff {
    pub model: String,
    pub changes: Vec<crate::contracts::ContractChange>,
}

/// The lineage impact of one breaking change — which downstream models and
/// tests consume the thing that breaks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BreakingImpact {
    pub model: String,
    /// The broken column; `None` for model-level breaks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// What changed, in the schema/contract diff's own words.
    pub change: String,
    /// Where the break came from: `schema` or `contract`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub downstream_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<String>,
}

/// Row-count comparison for one dataset.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRowDiff {
    pub dataset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<i64>,
}

/// A request to compare two references.
#[derive(Clone, Debug)]
pub struct BranchDiffRequest {
    /// The candidate reference (usually the development branch).
    pub candidate_ref: String,
    /// The base reference (usually `main`).
    pub base_ref: String,
    /// Catalog applied to compiled targets on the candidate when no recorded
    /// materialisation target exists. `None` keeps the compiled catalog.
    pub candidate_catalog: Option<String>,
    /// Same for the base side.
    pub base_catalog: Option<String>,
    /// Run keyed value diffs on changed models that declare keys. Without
    /// this the report carries row counts only.
    pub deep: bool,
    /// Fallback schema for seed targets that have no recorded load.
    pub default_schema: Option<String>,
    /// The commit the candidate reference resolved to when the diff ran —
    /// binds this evidence to an exact branch head. `None` leaves the
    /// report name-bound only, which promotion cannot audit.
    pub candidate_hash: Option<String>,
    /// Same for the base side.
    pub base_hash: Option<String>,
}

/// A structured branch comparison.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchDiffReport {
    pub candidate_ref: String,
    pub base_ref: String,
    /// The commits the refs resolved to when the diff ran — `None` when the
    /// diff ran without a Nessie client (name-bound evidence only).
    #[serde(default)]
    pub candidate_hash: Option<String>,
    #[serde(default)]
    pub base_hash: Option<String>,
    /// Every dataset in the union of the workspace and recorded
    /// materialisations, sorted by name.
    pub datasets: Vec<DatasetDiff>,
    /// Column-level changes for datasets present on at least one side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schema_changes: Vec<ModelSchemaDiff>,
    /// Contract changes each model would promote onto the base
    /// environment's recorded contract — declared-rename aware.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract_changes: Vec<ModelContractDiff>,
    /// Lineage impact for every breaking schema or contract change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impacts: Vec<BreakingImpact>,
    /// Row counts for datasets present on at least one side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<ModelRowDiff>,
    /// Deep per-model data diffs for changed models (only when `deep`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<DiffReport>,
    /// Whether value-level diffs ran (`diff --full`). A shallow report
    /// compared schema and row counts only — no data-diff policies were
    /// evaluated, so it cannot satisfy a required data-diff audit.
    #[serde(default)]
    pub deep: bool,
    /// All deep diffs passed their policies (vacuously true without `deep`).
    pub passed: bool,
    pub started_at: String,
    pub finished_at: String,
}

/// Compare a candidate reference against a base reference.
pub async fn branch_diff(
    adapter: Arc<dyn Adapter>,
    state: Option<&dyn StateStore>,
    compilation: &Compilation,
    request: &BranchDiffRequest,
) -> Result<BranchDiffReport, EngineError> {
    let started_at = now_rfc3339();

    let candidate_records: BTreeMap<String, MaterializedRecord> = match state {
        Some(state) => materialized_for_environment(state, &request.candidate_ref)?,
        None => BTreeMap::new(),
    };
    let base_records: BTreeMap<String, MaterializedRecord> = match state {
        Some(state) => materialized_for_environment(state, &request.base_ref)?,
        None => BTreeMap::new(),
    };
    let candidate_seeds: BTreeMap<String, SeedRecord> = match state {
        Some(state) => seeds_for_environment(state, &request.candidate_ref)?,
        None => BTreeMap::new(),
    };
    let base_seeds: BTreeMap<String, SeedRecord> = match state {
        Some(state) => seeds_for_environment(state, &request.base_ref)?,
        None => BTreeMap::new(),
    };

    // The union of workspace models, workspace seeds and recorded
    // materialisations — a model deleted from the workspace still shows up
    // via its records.
    let mut names: BTreeSet<String> = compilation
        .models
        .iter()
        .map(|model| model.id.logical_name())
        .collect();
    names.extend(compilation.seeds.iter().map(|seed| seed.name.clone()));
    names.extend(candidate_records.keys().cloned());
    names.extend(base_records.keys().cloned());
    names.extend(candidate_seeds.keys().cloned());
    names.extend(base_seeds.keys().cloned());

    let mut datasets = Vec::new();
    // Resolved relations + existence per dataset for the later passes.
    let mut relations: BTreeMap<String, Resolved> = BTreeMap::new();

    for name in &names {
        let model = compilation.model_by_name(name);
        let compiled_seed = compilation.seeds.iter().find(|seed| &seed.name == name);
        let is_seed = model.is_none()
            && (compiled_seed.is_some()
                || candidate_seeds.contains_key(name)
                || base_seeds.contains_key(name));

        let candidate_version = candidate_records
            .get(name)
            .map(|record| record.version.hash.clone())
            .or_else(|| {
                candidate_seeds
                    .get(name)
                    .map(|record| record.content_hash.clone())
            });
        let base_version = base_records
            .get(name)
            .map(|record| record.version.hash.clone())
            .or_else(|| {
                base_seeds
                    .get(name)
                    .map(|record| record.content_hash.clone())
            });

        let candidate_relation = candidate_records
            .get(name)
            .and_then(|record| Relation::parse(&record.target).ok())
            .or_else(|| {
                candidate_seeds
                    .get(name)
                    .and_then(|record| Relation::parse(&record.target).ok())
            })
            .or_else(|| {
                model.map(|model| retarget(&model.target, request.candidate_catalog.as_deref()))
            })
            .or_else(|| {
                compiled_seed.map(|seed| {
                    seed.relation(
                        request.candidate_catalog.as_deref(),
                        request.default_schema.as_deref().unwrap_or("public"),
                    )
                })
            });
        let base_relation = base_records
            .get(name)
            .and_then(|record| Relation::parse(&record.target).ok())
            .or_else(|| {
                base_seeds
                    .get(name)
                    .and_then(|record| Relation::parse(&record.target).ok())
            })
            .or_else(|| model.map(|model| retarget(&model.target, request.base_catalog.as_deref())))
            .or_else(|| {
                compiled_seed.map(|seed| {
                    seed.relation(
                        request.base_catalog.as_deref(),
                        request.default_schema.as_deref().unwrap_or("public"),
                    )
                })
            });

        let candidate_exists = exists(adapter.as_ref(), candidate_relation.as_ref()).await;
        let base_exists = exists(adapter.as_ref(), base_relation.as_ref()).await;

        let status = match (candidate_exists, base_exists) {
            (true, false) => DatasetStatus::Added,
            (false, true) => DatasetStatus::Removed,
            (false, false) => DatasetStatus::Absent,
            (true, true) => match (&candidate_version, &base_version) {
                (Some(candidate), Some(base)) => {
                    if candidate != base {
                        DatasetStatus::Changed
                    } else {
                        DatasetStatus::Unchanged
                    }
                }
                // The candidate wrote this dataset but the base's version is
                // unknown — it cannot be proven unchanged.
                (Some(_), None) => DatasetStatus::Changed,
                // No recorded version on the candidate: either the branch
                // inherited the base's physical table (same snapshot — data
                // is genuinely the base's) or the state store was not shared.
                // The adapter's snapshot/source state decides; when neither
                // side reports one there is no evidence of change.
                (None, _) => {
                    match (
                        snapshot(adapter.as_ref(), candidate_relation.as_ref()).await,
                        snapshot(adapter.as_ref(), base_relation.as_ref()).await,
                    ) {
                        (Some(candidate), Some(base)) if candidate != base => {
                            DatasetStatus::Changed
                        }
                        _ => DatasetStatus::Unchanged,
                    }
                }
            },
        };

        datasets.push(DatasetDiff {
            dataset: name.clone(),
            kind: if is_seed {
                DatasetKind::Seed
            } else {
                DatasetKind::Model
            },
            status,
            candidate_version,
            base_version,
            candidate_relation: candidate_relation.as_ref().map(Relation::display),
            base_relation: base_relation.as_ref().map(Relation::display),
            upstream: model
                .map(|model| {
                    model
                        .model_dependencies()
                        .map(|id| id.logical_name())
                        .collect()
                })
                .unwrap_or_default(),
        });
        relations.insert(
            name.clone(),
            Resolved {
                candidate: candidate_relation,
                candidate_exists,
                base: base_relation,
                base_exists,
            },
        );
    }

    // Schema and row-count passes for datasets present on at least one side.
    // Dead sides are skipped outright — their catalog may not even exist.
    let mut schema_changes = Vec::new();
    let mut rows = Vec::new();
    for dataset in &datasets {
        let resolved = relations.get(&dataset.dataset).unwrap();
        if dataset.status == DatasetStatus::Absent {
            continue;
        }

        let candidate_columns = match (&resolved.candidate, resolved.candidate_exists) {
            (Some(relation), true) => adapter.relation_columns(relation).await.unwrap_or_default(),
            _ => Vec::new(),
        };
        let base_columns = match (&resolved.base, resolved.base_exists) {
            (Some(relation), true) => adapter.relation_columns(relation).await.unwrap_or_default(),
            _ => Vec::new(),
        };
        let renames = compilation
            .model_by_name(&dataset.dataset)
            .and_then(|model| model.contract.as_ref())
            .map(|contract| &contract.renames);
        let changes = compare_column_lists(
            &candidate_columns,
            &base_columns,
            renames.unwrap_or(&EMPTY_RENAMES),
        );
        if !changes.is_empty() {
            schema_changes.push(ModelSchemaDiff {
                model: dataset.dataset.clone(),
                changes,
            });
        }

        let candidate_rows = match (&resolved.candidate, resolved.candidate_exists) {
            (Some(relation), true) => Some(row_count(adapter.as_ref(), relation).await?),
            _ => None,
        };
        let base_rows = match (&resolved.base, resolved.base_exists) {
            (Some(relation), true) => Some(row_count(adapter.as_ref(), relation).await?),
            _ => None,
        };
        if candidate_rows.is_some() || base_rows.is_some() {
            rows.push(ModelRowDiff {
                dataset: dataset.dataset.clone(),
                base_rows,
                candidate_rows,
                delta: match (base_rows, candidate_rows) {
                    (Some(base), Some(candidate)) => Some(candidate - base),
                    _ => None,
                },
            });
        }
    }

    // Deep mode: keyed value diffs for changed models that declare keys.
    // A keyless model is skipped only when it declares no diff policy — a
    // declared policy (`require_keyed_diff`, row thresholds, ...) must be
    // evaluated, not silently skipped.
    let mut diffs = Vec::new();
    if request.deep {
        for dataset in &datasets {
            if dataset.status != DatasetStatus::Changed {
                continue;
            }
            let Some(model) = compilation.model_by_name(&dataset.dataset) else {
                continue;
            };
            let key_columns = model_keys(model);
            if key_columns.is_empty() && model.config.diff.is_none() {
                continue;
            }
            let resolved = relations.get(&dataset.dataset).unwrap();
            if !resolved.candidate_exists || !resolved.base_exists {
                continue;
            }
            let (Some(candidate_relation), Some(base_relation)) =
                (resolved.candidate.clone(), resolved.base.clone())
            else {
                continue;
            };
            let columns: Vec<String> = if model.schema.known {
                model
                    .schema
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .filter(|name| !key_columns.contains(name))
                    .collect()
            } else {
                Vec::new()
            };
            diffs.push(
                diff(
                    adapter.clone(),
                    &DiffRequest {
                        model: dataset.dataset.clone(),
                        candidate_relation,
                        base_relation,
                        candidate_ref: Some(request.candidate_ref.clone()),
                        base_ref: Some(request.base_ref.clone()),
                        candidate_version: dataset.candidate_version.clone(),
                        base_version: dataset.base_version.clone(),
                        key_columns,
                        columns,
                        strategy: DiffStrategy::Keyed,
                        policy: diff_policy(model.config.diff.as_ref()),
                        sample_fraction: None,
                        renames: model
                            .contract
                            .as_ref()
                            .map(|contract| contract.renames.clone())
                            .unwrap_or_default(),
                    },
                )
                .await?,
            );
        }
    }
    diffs.sort_by(|left, right| left.model.cmp(&right.model));
    let passed = diffs.iter().all(|report| report.passed);

    // Contract diffs: the workspace's declared contract against what the
    // base environment last recorded. A model without a base record reports
    // its contract as newly declared; a model whose base record predates
    // contract persistence compares against `None` the same way.
    let mut contract_changes = Vec::new();
    for model in &compilation.models {
        let name = model.id.logical_name();
        let record = base_records.get(&name);
        let mut changes = crate::contracts::contract_diff(
            record.and_then(|record| record.contract.as_ref()),
            model.contract.as_ref(),
        );
        // The effective key collapses incremental `key` columns and
        // unique-assertion columns onto one identity concept; compare the
        // workspace's against the key the base materialisation persisted.
        // A model never materialised on the base has no key to protect; a
        // record that predates key persistence fails closed.
        if let Some(change) = crate::contracts::key_change(
            &record
                .map(|record| record.recorded_key())
                .unwrap_or(crate::contracts::RecordedKey::Known(None)),
            crate::contracts::effective_key(model).as_deref(),
        ) {
            changes.push(change);
        }
        if !changes.is_empty() {
            contract_changes.push(ModelContractDiff {
                model: name,
                changes,
            });
        }
    }

    // Lineage impact of every breaking change — what promoting this branch
    // would break downstream.
    let mut impacts = Vec::new();
    for schema_diff in &schema_changes {
        for change in &schema_diff.changes {
            if matches!(change.safety.as_str(), "error" | "full_rebuild_required") {
                impacts.push(breaking_impact(
                    compilation,
                    &schema_diff.model,
                    Some(change.column.as_str()),
                    "schema",
                    &change.detail,
                ));
            }
        }
    }
    for contract_diff in &contract_changes {
        for change in &contract_diff.changes {
            if change.safety == crate::contracts::ContractSafety::Breaking {
                impacts.push(breaking_impact(
                    compilation,
                    &contract_diff.model,
                    (!change.column.is_empty()).then_some(change.column.as_str()),
                    "contract",
                    &change.detail,
                ));
            }
        }
    }

    Ok(BranchDiffReport {
        candidate_ref: request.candidate_ref.clone(),
        base_ref: request.base_ref.clone(),
        candidate_hash: request.candidate_hash.clone(),
        base_hash: request.base_hash.clone(),
        datasets,
        schema_changes,
        contract_changes,
        impacts,
        rows,
        diffs,
        deep: request.deep,
        passed,
        started_at,
        finished_at: now_rfc3339(),
    })
}

/// A dataset's resolved relations on each side, with existence.
struct Resolved {
    candidate: Option<Relation>,
    candidate_exists: bool,
    base: Option<Relation>,
    base_exists: bool,
}

/// A dataset's stable key columns: the incremental key, else a declared
/// uniqueness assertion.
pub fn model_keys(model: &phlo_transform_core::CompiledModel) -> Vec<String> {
    if let Some(phlo_transform_core::IncrementalStrategy::Key { columns }) =
        &model.config.incremental
    {
        return columns.clone();
    }
    for assertion in &model.assertions {
        if let phlo_transform_core::Assertion::Unique { columns } = assertion {
            return columns.clone();
        }
    }
    Vec::new()
}

/// Engine-side view of a model's declared diff policy.
fn diff_policy(spec: Option<&phlo_transform_core::DiffPolicySpec>) -> crate::diff::DiffPolicy {
    match spec {
        Some(spec) => crate::diff::DiffPolicy {
            max_added_rows: spec.max_added_rows,
            max_removed_rows: spec.max_removed_rows,
            max_modified_rows: spec.max_modified_rows,
            max_changed_fraction: spec.max_changed_fraction,
            require_full_diff: spec.require_full_diff,
            require_keyed_diff: spec.require_keyed_diff,
            tolerances: spec.tolerances.clone(),
        },
        None => crate::diff::DiffPolicy::default(),
    }
}

/// Re-point a compiled target at a reference's catalog.
pub fn retarget(target: &Relation, catalog: Option<&str>) -> Relation {
    match catalog {
        Some(catalog) => Relation {
            catalog: Some(catalog.to_string()),
            schema: target.schema.clone(),
            table: target.table.clone(),
        },
        None => target.clone(),
    }
}

async fn exists(adapter: &dyn Adapter, relation: Option<&Relation>) -> bool {
    match relation {
        Some(relation) => adapter.relation_exists(relation).await.unwrap_or(false),
        None => false,
    }
}

/// The relation's physical snapshot/source state (Iceberg snapshot id), when
/// the adapter exposes one — the fallback version signal without state
/// records.
async fn snapshot(adapter: &dyn Adapter, relation: Option<&Relation>) -> Option<String> {
    match relation {
        Some(relation) => adapter.source_state(relation).await.ok().flatten(),
        None => None,
    }
}

/// Resolve one breaking change's downstream blast radius through the
/// lineage graph: column-level breaks use the column impact walk,
/// model-level breaks report the whole model's downstream.
fn breaking_impact(
    compilation: &Compilation,
    model_name: &str,
    column: Option<&str>,
    source: &str,
    detail: &str,
) -> BreakingImpact {
    let (downstream_models, tests) = match compilation.model_by_name(model_name) {
        Some(model) => match column {
            Some(column) => {
                let report = compilation.impact_report(
                    &phlo_transform_core::semantic::ColumnRef::model(model.id.clone(), column),
                );
                (report.downstream_models, report.tests)
            }
            None => {
                let mut models = Vec::new();
                for node in compilation.lineage.downstream_transitive(
                    &phlo_transform_core::lineage::LineageNode::Model(model.id.clone()),
                ) {
                    if let phlo_transform_core::lineage::LineageNode::Model(id) = node {
                        models.push(id.logical_name());
                    }
                }
                let tests = compilation
                    .lineage
                    .tests_for_dataset(&phlo_transform_core::lineage::DatasetId::model(&model.id))
                    .into_iter()
                    .map(|test| test.to_string())
                    .collect();
                (models, tests)
            }
        },
        // The breaking dataset is not a compiled model (a seed, or a model
        // dropped from the workspace) — no lineage to walk.
        None => (Vec::new(), Vec::new()),
    };
    BreakingImpact {
        model: model_name.to_string(),
        column: column.map(str::to_string),
        change: detail.to_string(),
        source: source.to_string(),
        downstream_models,
        tests,
    }
}

/// The default (unlabeled) environment — `apply`/`run` with no `--ref`
/// records under it while physically writing the default catalog, which is
/// `main`'s view. When `main` is asked for, its records count too.
pub(crate) fn includes_default_environment(environment: &str) -> bool {
    environment == "main"
}

/// Fold `incoming` records into `by_key`, keeping per key the record with
/// the chronologically latest `timestamp`. The comparison is parsed, not
/// lexical: legacy variable-width fractions (`...:05Z` vs `...:05.5Z`)
/// must order as instants, or an older record can shadow the physical
/// table's real last write.
pub(crate) fn merge_latest<R>(
    by_key: &mut BTreeMap<String, R>,
    incoming: impl IntoIterator<Item = R>,
    key: impl Fn(&R) -> &str,
    timestamp: impl Fn(&R) -> &str,
) {
    for record in incoming {
        match by_key.get(key(&record)) {
            Some(existing)
                if cmp_rfc3339(timestamp(existing), timestamp(&record)) != Ordering::Less => {}
            _ => {
                by_key.insert(key(&record).to_string(), record);
            }
        }
    }
}

/// Every materialised model recorded for an environment label, keyed by
/// model id. `main` also folds in the default environment — a run on `main`
/// with no `--ref` records there — so a dropped model's record is found
/// regardless of which label the run carried. When a model has records in
/// both, the latest `materialized_at` wins: both describe the same physical
/// table, and the last write is what is actually there.
pub fn materialized_for_environment(
    state: &dyn StateStore,
    environment: &str,
) -> Result<BTreeMap<String, MaterializedRecord>, EngineError> {
    let mut records: BTreeMap<String, MaterializedRecord> = state
        .materialized_in(Some(environment))?
        .into_iter()
        .map(|record| (record.model_id.clone(), record))
        .collect();
    if includes_default_environment(environment) {
        merge_latest(
            &mut records,
            state.materialized_in(None)?,
            |record| &record.model_id,
            |record| &record.materialized_at,
        );
    }
    Ok(records)
}

/// Every seed load recorded for an environment label — same `main`
/// convention as [`materialized_for_environment`], latest `loaded_at` wins.
pub fn seeds_for_environment(
    state: &dyn StateStore,
    environment: &str,
) -> Result<BTreeMap<String, SeedRecord>, EngineError> {
    let mut records: BTreeMap<String, SeedRecord> = state
        .seeds_in(Some(environment))?
        .into_iter()
        .map(|record| (record.name.clone(), record))
        .collect();
    if includes_default_environment(environment) {
        merge_latest(
            &mut records,
            state.seeds_in(None)?,
            |record| &record.name,
            |record| &record.loaded_at,
        );
    }
    Ok(records)
}

async fn row_count(adapter: &dyn Adapter, relation: &Relation) -> Result<i64, EngineError> {
    let result = adapter
        .execute(&format!("SELECT count(*) FROM {}", relation.sql()))
        .await
        .map_err(EngineError::Adapter)?;
    let value = result
        .rows
        .first()
        .and_then(|row| row.first())
        .ok_or_else(|| {
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
