//! Planning: turn a compilation and selection into inspectable work.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Serialize;

use phlo_transform_core::graph::Dependency;
use phlo_transform_core::{Compilation, Diagnostic, ModelId};

use crate::adapter::Adapter;
use crate::error::EngineError;
use crate::util::{now_rfc3339, sha256_hex};

/// What the planner intends to do to a model's physical relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Create,
    Replace,
    NoOp,
    Unknown,
}

/// A model in a plan.
#[derive(Clone, Debug, Serialize)]
pub struct PlannedModel {
    pub id: String,
    pub target: String,
    pub materialization: String,
    pub action: PlanAction,
    pub exists: bool,
    pub dependencies: Vec<String>,
    pub sources: Vec<String>,
    pub sql_hash: String,
    pub compiled_sql: String,
}

/// A test in a plan.
#[derive(Clone, Debug, Serialize)]
pub struct PlannedTest {
    pub id: String,
    pub targets: Vec<String>,
    pub sources: Vec<String>,
}

/// An inspectable plan.
#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub id: String,
    pub created_at: String,
    pub environment: Option<String>,
    pub adapter: String,
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
}

/// Computes plans against an adapter.
pub struct Planner {
    adapter: Arc<dyn Adapter>,
}

impl Planner {
    pub fn new(adapter: Arc<dyn Adapter>) -> Self {
        Self { adapter }
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
            let exists = if blocked {
                false
            } else {
                self.adapter.relation_exists(&model.target).await?
            };
            let action = match (blocked, exists) {
                (true, _) => PlanAction::Unknown,
                (false, true) => PlanAction::Replace,
                (false, false) => PlanAction::Create,
            };
            models.push(PlannedModel {
                id: model.id.logical_name(),
                target: model.target.display(),
                materialization: model.config.materialization.to_string(),
                action,
                exists,
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
            blocked,
            models,
            tests,
            diagnostics: compilation.diagnostics.clone(),
        })
    }
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
