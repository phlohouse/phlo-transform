//! Planning: turn a compilation and selection into inspectable work.
//!
//! Phase 3 makes planning state-aware: each model's desired content-addressed
//! version is compared against the version recorded for the target
//! environment, and the plan explains why a model will be built, skipped or
//! reused from cache.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use phlo_transform_core::graph::Dependency;
use phlo_transform_core::{
    classify_schema_change, Compilation, DataType, Diagnostic, IncrementalStrategy,
    Materialization, ModelId, ModelVersion, Nullability, SchemaChangeSafety, SchemaColumn,
};

use crate::adapter::Adapter;
use crate::error::EngineError;
use crate::source_state::{
    adapter_default_schema, relation_for_source, seed_for_relation, seed_relation,
};
use crate::state::StateStore;
use crate::util::{now_rfc3339, sha256_hex};

/// What the planner intends to do to a model's physical relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    /// Build or rebuild the physical relation.
    Build,
    /// The desired version is already materialised in this environment.
    Skip,
    /// The desired version can be reused from a compatible materialisation.
    Cached,
    /// Compilation errors block a decision.
    Unknown,
}

/// Why a model needs work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeReason {
    SqlSemanticChange,
    ConfigChange,
    ContractChange,
    DependencyChange,
    SourceChange,
    TargetChange,
    CompilerSemanticsChange,
    IncrementalChange,
    SchemaChange,
    MissingRelation,
    UnknownState,
}

impl ChangeReason {
    pub fn label(self) -> &'static str {
        match self {
            ChangeReason::SqlSemanticChange => "SQL semantics changed",
            ChangeReason::ConfigChange => "configuration changed",
            ChangeReason::ContractChange => "contract or assertions changed",
            ChangeReason::DependencyChange => "an upstream model version changed",
            ChangeReason::SourceChange => "a source state changed",
            ChangeReason::TargetChange => "physical target changed",
            ChangeReason::CompilerSemanticsChange => "compiler semantics changed",
            ChangeReason::IncrementalChange => "incremental strategy or key changed",
            ChangeReason::SchemaChange => "output schema changed incompatibly",
            ChangeReason::MissingRelation => "target relation does not exist",
            ChangeReason::UnknownState => "no recorded materialised version",
        }
    }
}

/// A model in a plan.
#[derive(Clone, Debug, Serialize)]
pub struct PlannedModel {
    pub id: String,
    pub target: String,
    pub materialization: String,
    pub action: PlanAction,
    pub reasons: Vec<ChangeReason>,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incremental: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub full_rebuild: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watermark: Option<String>,
    pub desired_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    pub dependencies: Vec<String>,
    pub sources: Vec<String>,
    pub sql_hash: String,
    pub compiled_sql: String,
}

/// A seed in a plan — the CSV gets loaded into `target` before models build.
#[derive(Clone, Debug, Serialize)]
pub struct PlannedSeed {
    pub name: String,
    pub target: String,
    /// Workspace-relative CSV path.
    pub path: PathBuf,
    pub action: PlanAction,
    /// The CSV's content hash — the seed's version.
    pub desired_version: String,
}

/// A test in a plan.
#[derive(Clone, Debug, Serialize)]
pub struct PlannedTest {
    pub id: String,
    #[serde(skip_serializing_if = "is_false")]
    pub generated: bool,
    pub targets: Vec<String>,
    pub sources: Vec<String>,
}

/// An inspectable, state-aware plan.
#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub id: String,
    pub created_at: String,
    pub environment: Option<String>,
    pub adapter: String,
    pub compiler_semantics_version: String,
    /// True when compilation errors block execution.
    pub blocked: bool,
    /// Seeds the planned models read through `source(...)`, in load order.
    pub seeds: Vec<PlannedSeed>,
    pub models: Vec<PlannedModel>,
    pub tests: Vec<PlannedTest>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Plan {
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn test_count(&self) -> usize {
        self.tests.len()
    }

    pub fn build_count(&self) -> usize {
        self.models
            .iter()
            .filter(|model| model.action == PlanAction::Build)
            .count()
    }

    /// Verify the plan still matches a freshly compiled workspace.
    pub fn staleness(&self, compilation: &Compilation) -> Option<String> {
        for model in &self.models {
            let Some(id) = ModelId::parse(&model.id).ok() else {
                continue;
            };
            let Some(compiled) = compilation.model(&id) else {
                return Some(format!("model `{}` no longer exists", model.id));
            };
            if compiled.version.hash != model.desired_version {
                return Some(format!(
                    "model `{}` changed since the plan was created",
                    model.id
                ));
            }
        }
        None
    }
}

/// Computes state-aware plans against an adapter.
pub struct Planner {
    adapter: Arc<dyn Adapter>,
    state: Option<Arc<dyn StateStore>>,
}

impl Planner {
    pub fn new(adapter: Arc<dyn Adapter>, state: Option<Arc<dyn StateStore>>) -> Self {
        Self { adapter, state }
    }

    /// Build a plan for the selected models.
    ///
    /// The selection is expanded to include every transitive workspace
    /// dependency so that execution is always dependency-closed.
    pub async fn plan(
        &self,
        compilation: &Compilation,
        selected: &[ModelId],
        environment: Option<String>,
    ) -> Result<Plan, EngineError> {
        let blocked = !compilation.is_ok();
        let planned_ids = dependency_closure(compilation, selected);

        let order: Vec<ModelId> = compilation
            .topological_order()
            .unwrap_or_else(|| planned_ids.iter().cloned().collect())
            .into_iter()
            .filter(|id| planned_ids.contains(id))
            .collect();

        // Seeds are planned for the source relations the selected models read.
        let default_catalog = compilation.defaults.catalog.as_deref();
        let default_schema = compilation
            .defaults
            .schema
            .as_deref()
            .or_else(|| adapter_default_schema(self.adapter.name()));
        let mut needed_seeds: BTreeMap<String, &phlo_transform_core::CompiledSeed> =
            BTreeMap::new();
        for id in &order {
            let Some(model) = compilation.model(id) else {
                continue;
            };
            for source in model.source_dependencies() {
                let relation = relation_for_source(source, default_catalog, default_schema);
                if let Some(seed) = seed_for_relation(
                    &compilation.seeds,
                    &relation,
                    default_catalog,
                    default_schema,
                    self.adapter.name(),
                ) {
                    needed_seeds.insert(seed.name.clone(), seed);
                }
            }
        }
        let mut seeds = Vec::with_capacity(needed_seeds.len());
        for seed in needed_seeds.into_values() {
            let relation =
                seed_relation(seed, default_catalog, default_schema, self.adapter.name());
            let action = if blocked {
                PlanAction::Unknown
            } else {
                let exists = self.adapter.relation_exists(&relation).await?;
                let current = match &self.state {
                    Some(state) => state.seed_state(&seed.name, environment.as_deref())?,
                    None => None,
                };
                let current_ok = current
                    .map(|record| {
                        record.content_hash == seed.content_hash
                            && record.target == relation.display()
                    })
                    .unwrap_or(false);
                if exists && current_ok {
                    PlanAction::Skip
                } else {
                    PlanAction::Build
                }
            };
            seeds.push(PlannedSeed {
                name: seed.name.clone(),
                target: relation.display(),
                path: seed.path.clone(),
                action,
                desired_version: seed.content_hash.clone(),
            });
        }

        let mut models = Vec::with_capacity(order.len());
        for id in &order {
            let Some(model) = compilation.model(id) else {
                continue;
            };
            let desired = model.version.clone();

            let (action, mut reasons, current_record, exists) = if blocked {
                (PlanAction::Unknown, Vec::new(), None, false)
            } else {
                let exists = self.adapter.relation_exists(&model.target).await?;
                let current = match &self.state {
                    Some(state) => {
                        state.materialized_version(&id.logical_name(), environment.as_deref())?
                    }
                    None => None,
                };
                let (action, reasons) =
                    self.decide(&desired, current.as_ref(), exists, environment.as_deref())?;
                (action, reasons, current, exists)
            };

            // A changed incremental strategy or key needs a full rebuild.
            let mut full_rebuild = false;
            if action == PlanAction::Build
                && model.config.materialization == Materialization::Incremental
            {
                match &current_record {
                    Some(record) => {
                        let desired_strategy = model
                            .config
                            .incremental
                            .as_ref()
                            .map(|strategy| strategy.as_str().to_string());
                        let desired_key = model
                            .config
                            .incremental
                            .as_ref()
                            .map(|strategy| strategy.columns().join(","))
                            .filter(|key| !key.is_empty());
                        if record.incremental_strategy != desired_strategy
                            || record.incremental_key != desired_key
                        {
                            full_rebuild = true;
                            if !reasons.contains(&ChangeReason::IncrementalChange) {
                                reasons.push(ChangeReason::IncrementalChange);
                            }
                        }
                    }
                    None => full_rebuild = true,
                }
            }

            // Schema-change classification can force a full rebuild.
            if action == PlanAction::Build && exists && model.schema.known {
                if let Ok(columns) = self.adapter.relation_columns(&model.target).await {
                    let current_schema: Vec<SchemaColumn> = columns
                        .iter()
                        .map(|column| SchemaColumn {
                            name: column.name.clone(),
                            data_type: DataType::parse_trino(&column.data_type),
                            nullability: Nullability::Unknown,
                        })
                        .collect();
                    let desired_schema: Vec<SchemaColumn> = model
                        .schema
                        .columns
                        .iter()
                        .map(|column| SchemaColumn {
                            name: column.name.clone(),
                            data_type: column.data_type.clone(),
                            nullability: column.nullability,
                        })
                        .collect();
                    let safety = classify_schema_change(&desired_schema, &current_schema);
                    if safety != SchemaChangeSafety::Safe {
                        if !reasons.contains(&ChangeReason::SchemaChange) {
                            reasons.push(ChangeReason::SchemaChange);
                        }
                        if matches!(
                            safety,
                            SchemaChangeSafety::FullRebuildRequired | SchemaChangeSafety::Error
                        ) {
                            full_rebuild = true;
                        }
                    }
                }
            }

            // Time-window models resume from the last successful watermark.
            let watermark = match (&model.config.incremental, &self.state) {
                (Some(IncrementalStrategy::TimeWindow { .. }), Some(state)) => {
                    state.watermark(&id.logical_name(), environment.as_deref())?
                }
                _ => None,
            };

            let current_version = current_record
                .as_ref()
                .map(|record| record.version.hash.clone());

            models.push(PlannedModel {
                id: model.id.logical_name(),
                target: model.target.display(),
                materialization: model.config.materialization.to_string(),
                action,
                reasons,
                exists,
                incremental: model
                    .config
                    .incremental
                    .as_ref()
                    .map(|strategy| strategy.as_str().to_string()),
                full_rebuild,
                watermark,
                desired_version: desired.hash.clone(),
                current_version,
                dependencies: model
                    .model_dependencies()
                    .map(|dependency| dependency.logical_name())
                    .collect(),
                sources: model
                    .source_dependencies()
                    .map(|dependency| dependency.logical_name())
                    .collect(),
                sql_hash: sha256_hex(&model.compiled_sql),
                compiled_sql: model.compiled_sql.clone(),
            });
        }

        let tests = compilation
            .tests
            .iter()
            .filter(|test| {
                test.targets
                    .iter()
                    .all(|target| planned_ids.contains(target))
            })
            .map(|test| PlannedTest {
                id: test.id.to_string(),
                generated: test.generated,
                targets: test
                    .targets
                    .iter()
                    .map(|target| target.logical_name())
                    .collect(),
                sources: test
                    .sources
                    .iter()
                    .map(|source| source.logical_name())
                    .collect(),
            })
            .collect();

        Ok(Plan {
            id: uuid::Uuid::new_v4().to_string(),
            created_at: now_rfc3339(),
            environment,
            adapter: self.adapter.name().to_string(),
            compiler_semantics_version: phlo_transform_core::COMPILER_SEMANTICS_VERSION.to_string(),
            blocked,
            seeds,
            models,
            tests,
            diagnostics: compilation.diagnostics.clone(),
        })
    }

    fn decide(
        &self,
        desired: &ModelVersion,
        current: Option<&crate::state::MaterializedRecord>,
        exists: bool,
        environment: Option<&str>,
    ) -> Result<(PlanAction, Vec<ChangeReason>), EngineError> {
        if !exists {
            return Ok((PlanAction::Build, vec![ChangeReason::MissingRelation]));
        }
        let Some(current) = current else {
            // Not materialised in this environment; if the exact version
            // exists elsewhere it is a cache candidate.
            if let Some(state) = &self.state {
                let elsewhere = state.materialized_by_hash(&desired.hash)?;
                if elsewhere
                    .iter()
                    .any(|record| record.environment.as_deref() != environment)
                {
                    return Ok((PlanAction::Cached, Vec::new()));
                }
            }
            return Ok((PlanAction::Build, vec![ChangeReason::UnknownState]));
        };

        if current.version.hash == desired.hash {
            return Ok((PlanAction::Skip, Vec::new()));
        }

        let mut reasons = Vec::new();
        if current.version.sql_hash != desired.sql_hash {
            reasons.push(ChangeReason::SqlSemanticChange);
        }
        if current.version.config_hash != desired.config_hash {
            reasons.push(ChangeReason::ConfigChange);
        }
        if current.version.contract_hash != desired.contract_hash {
            reasons.push(ChangeReason::ContractChange);
        }
        if current.version.dependency_hash != desired.dependency_hash {
            reasons.push(ChangeReason::DependencyChange);
        }
        if current.version.source_state_hash != desired.source_state_hash {
            reasons.push(ChangeReason::SourceChange);
        }
        if current.version.target_hash != desired.target_hash {
            reasons.push(ChangeReason::TargetChange);
        }
        if current.version.compiler_version != desired.compiler_version {
            reasons.push(ChangeReason::CompilerSemanticsChange);
        }
        if reasons.is_empty() {
            reasons.push(ChangeReason::UnknownState);
        }
        Ok((PlanAction::Build, reasons))
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Expand a selection to include every transitive model dependency.
pub fn dependency_closure(compilation: &Compilation, selected: &[ModelId]) -> BTreeSet<ModelId> {
    let mut included: BTreeSet<ModelId> = selected.iter().cloned().collect();
    let mut frontier: Vec<ModelId> = selected.to_vec();
    while let Some(id) = frontier.pop() {
        for dependency in compilation.dependencies(&id) {
            if let Dependency::Model(dependency_id) = dependency {
                if included.insert(dependency_id.clone()) {
                    frontier.push(dependency_id);
                }
            }
        }
    }
    included
}
