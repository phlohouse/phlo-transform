//! Planning: turn a compilation and selection into inspectable work.
//!
//! Phase 3 makes planning state-aware: each model's desired content-addressed
//! version is compared against the version recorded for the target
//! environment, and the plan explains why a model will be built, skipped or
//! reused from cache.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Serialize;

use phlo_transform_core::graph::Dependency;
use phlo_transform_core::{Compilation, Diagnostic, ModelId, ModelVersion};

use crate::adapter::Adapter;
use crate::error::EngineError;
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
    pub desired_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    pub dependencies: Vec<String>,
    pub sources: Vec<String>,
    pub sql_hash: String,
    pub compiled_sql: String,
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

        let mut models = Vec::with_capacity(order.len());
        for id in &order {
            let Some(model) = compilation.model(id) else {
                continue;
            };
            let desired = model.version.clone();

            let (action, reasons, current_version, exists) = if blocked {
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
                (
                    action,
                    reasons,
                    current.as_ref().map(|record| record.version.hash.clone()),
                    exists,
                )
            };

            models.push(PlannedModel {
                id: model.id.logical_name(),
                target: model.target.display(),
                materialization: model.config.materialization.to_string(),
                action,
                reasons,
                exists,
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
