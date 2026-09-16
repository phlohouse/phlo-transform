//! Planning: turn a compilation and selection into inspectable work.
//!
//! Planning is state-aware: each model's desired content-addressed version
//! is compared against the version recorded for the target environment, and
//! every build, skip and reuse decision carries structured [`PlanReason`]s
//! explaining *why* — including which dependency version or source state
//! changed when the recorded detail is available.
//!
//! The plan is always dependency-closed: every model the selection needs is
//! planned even when it was not matched directly. Models pulled in by
//! closure carry `membership: "dependency"`; models matched by a term carry
//! `"selected"`, and `+`-expanded matches carry `"expanded"`. Excluded
//! models are never pulled back in by closure — a model whose dependency is
//! excluded plans against the existing materialisation and the plan records
//! a warning.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use serde::Serialize;

use phlo_transform_core::graph::Dependency;
use phlo_transform_core::{
    classify_schema_change, Compilation, CompiledModel, DataType, Diagnostic, GitChanges,
    IncrementalStrategy, Materialization, ModelId, Nullability, Relation, SchemaChangeSafety,
    SchemaColumn, SelectedModel, Selection, SourceId,
};

use crate::adapter::Adapter;
use crate::error::EngineError;
use crate::source_state::{
    adapter_default_schema, relation_for_source, seed_for_relation, seed_relation,
};
use crate::state::{MaterializedRecord, SeedRecord, StateStore};
use crate::util::{now_rfc3339, sha256_hex};

/// What the planner intends to do to a model's physical relation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    /// Build or rebuild the physical relation.
    Build,
    /// The desired version is already materialised in this environment.
    Skip,
    /// The desired version can be reused from a compatible materialisation.
    Cached,
    /// Compilation errors block a decision.
    #[default]
    Unknown,
}

impl PlanAction {
    /// Stable machine-readable code.
    pub fn as_str(self) -> &'static str {
        match self {
            PlanAction::Build => "build",
            PlanAction::Skip => "skip",
            PlanAction::Cached => "cached",
            PlanAction::Unknown => "unknown",
        }
    }

    /// Parse a code produced by `as_str` (used when reconstructing a stored
    /// plan for resume). Unknown values map to `Unknown`.
    pub fn parse(value: &str) -> Self {
        match value {
            "build" => PlanAction::Build,
            "skip" => PlanAction::Skip,
            "cached" => PlanAction::Cached,
            _ => PlanAction::Unknown,
        }
    }
}

/// The kind of a [`PlanReason`] — stable for programmatic consumers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonKind {
    /// The canonical SQL changed.
    SqlSemanticChange,
    /// Semantics-affecting configuration changed.
    ConfigChange,
    /// Contract or assertions changed.
    ContractChange,
    /// A dependency's recorded version input changed.
    DependencyChange,
    /// An upstream model in this plan will also build.
    UpstreamRebuild,
    /// An observed source state changed.
    SourceChange,
    /// The physical target relation changed.
    TargetChange,
    /// The compiler semantics version changed.
    CompilerSemanticsChange,
    /// Incremental strategy or key changed; forces a full rebuild.
    IncrementalChange,
    /// The output schema changes incompatibly.
    SchemaChange,
    /// The target relation does not exist.
    MissingRelation,
    /// Nothing is recorded for this environment.
    UnknownState,
    /// The identical version is materialised in another environment.
    CacheReuse,
    /// `--force` was requested.
    Forced,
    /// The recorded version matches the desired version.
    Unchanged,
    /// Included because a selected model depends on it.
    SelectedDependency,
    /// Included because a `+` expansion term or `--upstream`/`--downstream`
    /// matched it.
    SelectionExpansion,
    /// No state store was available to compare against.
    StateUnavailable,
    /// Selected because a Git-aware change provider marked it changed
    /// (selection provenance — not a rebuild decision).
    GitChange,
    /// An identical version exists in another environment but cannot be
    /// safely reused here — the physical relation isn't reachable, or it
    /// was produced by a different adapter.
    CacheMiss,
    /// The recorded materialisation was produced by a different adapter —
    /// execution semantics differ, so the version alone can't be trusted.
    AdapterChange,
    /// The physical relation no longer holds the recorded materialisation —
    /// another writer overwrote it since the version was recorded.
    OutputDrift,
    /// Re-executed (or reused) as part of a resumed or retried run.
    ResumedRun,
}

impl ReasonKind {
    /// Stable machine-readable code.
    pub fn code(self) -> &'static str {
        match self {
            ReasonKind::SqlSemanticChange => "sql_semantic_change",
            ReasonKind::ConfigChange => "config_change",
            ReasonKind::ContractChange => "contract_change",
            ReasonKind::DependencyChange => "dependency_change",
            ReasonKind::UpstreamRebuild => "upstream_rebuild",
            ReasonKind::SourceChange => "source_change",
            ReasonKind::TargetChange => "target_change",
            ReasonKind::CompilerSemanticsChange => "compiler_semantics_change",
            ReasonKind::IncrementalChange => "incremental_change",
            ReasonKind::SchemaChange => "schema_change",
            ReasonKind::MissingRelation => "missing_relation",
            ReasonKind::UnknownState => "unknown_state",
            ReasonKind::CacheReuse => "cache_reuse",
            ReasonKind::CacheMiss => "cache_miss",
            ReasonKind::AdapterChange => "adapter_change",
            ReasonKind::OutputDrift => "output_drift",
            ReasonKind::Forced => "forced",
            ReasonKind::Unchanged => "unchanged",
            ReasonKind::SelectedDependency => "selected_dependency",
            ReasonKind::SelectionExpansion => "selection_expansion",
            ReasonKind::StateUnavailable => "state_unavailable",
            ReasonKind::GitChange => "git_change",
            ReasonKind::ResumedRun => "resumed_run",
        }
    }
}

/// One structured reason attached to a planned model or seed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlanReason {
    /// Stable reason kind.
    pub kind: ReasonKind,
    /// Human-readable explanation, e.g. `source raw.lims changed
    /// (csv:aaa… → csv:bbb…)`.
    pub detail: String,
    /// The model or source this reason is about, when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

impl PlanReason {
    pub(crate) fn simple(kind: ReasonKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            subject: None,
        }
    }

    fn about(kind: ReasonKind, detail: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            subject: Some(subject.into()),
        }
    }
}

/// How a model entered the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    /// Matched a selector term directly (or the default all-selection).
    Selected,
    /// Pulled in by a `+` expansion term or `--upstream`/`--downstream`.
    Expanded,
    /// Not selected — included because a planned model depends on it.
    Dependency,
}

/// Options that shape a plan beyond the selection itself.
#[derive(Clone, Debug, Default)]
pub struct PlanOptions {
    /// Rebuild every planned model regardless of recorded state.
    pub force: bool,
}

/// A model in a plan.
#[derive(Clone, Debug, Serialize)]
pub struct PlannedModel {
    pub id: String,
    pub target: String,
    pub materialization: String,
    pub action: PlanAction,
    /// Why this action was chosen — never empty for decided models.
    pub reasons: Vec<PlanReason>,
    /// How the model entered the plan.
    pub membership: Membership,
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
    /// Why the seed will (re)load.
    pub reasons: Vec<PlanReason>,
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

/// How the resolved selection shaped this plan.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PlanSelection {
    /// The include terms as written.
    pub terms: Vec<String>,
    /// The exclude terms as written.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Models matched by a term directly.
    pub matched: Vec<String>,
    /// Models pulled in by `+` expansion.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expanded: Vec<String>,
    /// Models pulled in by dependency closure.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
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
    /// The selection that produced this plan.
    pub selection: PlanSelection,
    /// The Git-derived change set, when the plan was built with `--since`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitChanges>,
    /// Non-fatal conditions worth surfacing, e.g. planning against a
    /// materialisation whose model was excluded.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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

    /// Build a plan for a resolved selection.
    ///
    /// The plan is dependency-closed: every transitive workspace dependency
    /// of a selected model is planned too, except models removed by
    /// `--exclude` — those stay excluded and the plan records a warning.
    /// Ephemeral models are inlined into dependents at compile time and are
    /// never planned.
    pub async fn plan(
        &self,
        compilation: &Compilation,
        selection: &Selection,
        environment: Option<String>,
        options: &PlanOptions,
    ) -> Result<Plan, EngineError> {
        let blocked = !compilation.is_ok();
        let excluded = selection.excluded_ids();
        let member_ids: BTreeSet<ModelId> = selection.ids().into_iter().collect();
        let planned_ids = dependency_closure_excluding(compilation, &member_ids, &excluded);
        // `Selection::get` scans members linearly — fine once, quadratic in
        // this loop. Index by logical name so per-model membership lookups
        // stay O(log n).
        let members_by_name: BTreeMap<&str, &SelectedModel> = selection
            .members
            .iter()
            .map(|member| (member.id.as_str(), member))
            .collect();
        let member = |id: &ModelId| members_by_name.get(id.logical_name().as_str()).copied();
        let requiring = requiring_models(compilation, &member_ids);

        // Ephemeral models are inlined into their dependents at compile time
        // and never produce a relation, so they are not planned or executed.
        let order: Vec<ModelId> = compilation
            .topological_order()
            .unwrap_or_else(|| planned_ids.iter().cloned().collect())
            .into_iter()
            .filter(|id| planned_ids.contains(id))
            .filter(|id| {
                compilation
                    .model(id)
                    .map(|model| model.config.materialization != Materialization::Ephemeral)
                    .unwrap_or(false)
            })
            .collect();

        // Seeds are planned for the source relations the selected models
        // read — including ephemeral models: they are filtered out of `order`
        // but their source reads are inlined into dependents, so a seed used
        // only inside an ephemeral chain still has to be loaded.
        let default_catalog = compilation.defaults.catalog.as_deref();
        let default_schema = compilation
            .defaults
            .schema
            .as_deref()
            .or_else(|| adapter_default_schema(self.adapter.name()));
        let mut needed_seeds: BTreeMap<String, &phlo_transform_core::CompiledSeed> =
            BTreeMap::new();
        let mut seed_sources: Vec<&SourceId> = Vec::new();
        for id in &planned_ids {
            let Some(model) = compilation.model(id) else {
                continue;
            };
            seed_sources.extend(model.source_dependencies());
        }
        // Seed-owned generated tests also need the seed loaded: a test that
        // reads a seed relation must not run against a stale or missing table.
        for test in &compilation.tests {
            if test
                .targets
                .iter()
                .all(|target| planned_ids.contains(target))
            {
                seed_sources.extend(test.sources.iter());
            }
        }
        for source in seed_sources {
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
        // Every read the ordered loops make is fetched before they run: one
        // batched existence probe (adapters with a queryable catalog answer
        // it in a query per schema, not a round trip per relation), a bulk
        // scan of the environment's materialisation/seed state, a bulked
        // version-hash lookup for cache candidates, and bounded-parallel
        // reads of the live output identities drift checks compare against.
        // What used to cost N × 1–3 serial warehouse round trips now costs a
        // handful of queries.
        let mut probe_relations: Vec<Relation> = Vec::new();
        if !blocked {
            // Excluded dependencies a planned model still reads — their
            // existence decides whether exclusion is safe.
            for id in &planned_ids {
                let Some(model) = compilation.model(id) else {
                    continue;
                };
                for dependency in model.model_dependencies() {
                    if !excluded.contains(dependency) {
                        continue;
                    }
                    let Some(excluded_model) = compilation.model(dependency) else {
                        continue;
                    };
                    // Ephemeral dependencies are inlined into the
                    // dependent's SQL — excluding one needs no
                    // materialisation at all.
                    if excluded_model.config.materialization == Materialization::Ephemeral {
                        continue;
                    }
                    probe_relations.push(excluded_model.target.clone());
                }
            }
            for seed in needed_seeds.values() {
                probe_relations.push(seed_relation(
                    seed,
                    default_catalog,
                    default_schema,
                    self.adapter.name(),
                ));
            }
            for id in &order {
                if let Some(model) = compilation.model(id) {
                    probe_relations.push(model.target.clone());
                }
            }
        }
        let exists_map: BTreeMap<String, bool> = {
            // `display()` is the relation's canonical identity — dedup so
            // shared targets are probed once.
            let mut seen = BTreeSet::new();
            let unique: Vec<Relation> = probe_relations
                .into_iter()
                .filter(|relation| seen.insert(relation.display()))
                .collect();
            self.adapter
                .relations_exist(&unique)
                .await?
                .into_iter()
                .zip(unique)
                .map(|(found, relation)| (relation.display(), found))
                .collect()
        };

        // A model whose dependency was excluded builds against whatever is
        // already materialised. That is only valid when a materialisation
        // actually exists — otherwise the plan would schedule a model that
        // reads a relation that does not exist.
        let mut warnings = Vec::new();
        for id in &planned_ids {
            let Some(model) = compilation.model(id) else {
                continue;
            };
            for dependency in model.model_dependencies() {
                if !excluded.contains(dependency) {
                    continue;
                }
                let Some(excluded_model) = compilation.model(dependency) else {
                    continue;
                };
                if excluded_model.config.materialization == Materialization::Ephemeral {
                    continue;
                }
                if !blocked
                    && !exists_map
                        .get(&excluded_model.target.display())
                        .copied()
                        .unwrap_or(false)
                {
                    return Err(EngineError::InvalidPlan(format!(
                        "{} depends on excluded model {}, and {} has never been materialised",
                        id.logical_name(),
                        dependency.logical_name(),
                        excluded_model.target.display()
                    )));
                }
                warnings.push(format!(
                    "{} depends on excluded model {}; it will read the existing materialisation",
                    id.logical_name(),
                    dependency.logical_name()
                ));
            }
        }

        // When compilation is blocked every model plans Unknown — no
        // evidence is read at all, matching the previous behavior of never
        // touching state or the warehouse on a broken compile.
        let materialized = match &self.state {
            Some(state) if !blocked => materialized_scope(state.as_ref(), environment.as_deref())?,
            _ => BTreeMap::new(),
        };
        let recorded_seeds = match &self.state {
            Some(state) if !blocked => seeds_scope(state.as_ref(), environment.as_deref())?,
            _ => BTreeMap::new(),
        };
        // Cache candidates: version-hash lookups only matter for models with
        // no record in this environment that nevertheless exist — exactly
        // the set `decide` would query one by one.
        let by_hash: BTreeMap<String, Vec<MaterializedRecord>> = match &self.state {
            Some(state) if !blocked => {
                let hashes: Vec<String> = order
                    .iter()
                    .filter_map(|id| compilation.model(id))
                    .filter(|model| {
                        !materialized.contains_key(&model.id.logical_name())
                            && exists_map
                                .get(&model.target.display())
                                .copied()
                                .unwrap_or(false)
                    })
                    .map(|model| model.version.hash.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                state.materialized_by_hashes(&hashes)?
            }
            _ => BTreeMap::new(),
        };
        // Live output identities for the models `decide` can ask about: a
        // recorded identity to drift-check, or cache candidates worth a
        // look. Bounded parallelism — each read is an adapter round trip.
        let live_outputs: BTreeMap<String, Option<String>> = {
            let wants: Vec<Relation> = if blocked {
                Vec::new()
            } else {
                order
                    .iter()
                    .filter_map(|id| compilation.model(id))
                    .filter(|model| {
                        let display = model.target.display();
                        let recorded_identity = materialized
                            .get(&model.id.logical_name())
                            .is_some_and(|record| record.output_identity.is_some());
                        let cache_candidate = !materialized.contains_key(&model.id.logical_name())
                            && exists_map.get(&display).copied().unwrap_or(false)
                            && by_hash.get(&model.version.hash).is_some_and(|records| {
                                records.iter().any(|record| {
                                    record.environment.as_deref() != environment.as_deref()
                                        && record.target == display
                                        && record.adapter.as_deref() == Some(self.adapter.name())
                                })
                            });
                        recorded_identity || cache_candidate
                    })
                    .map(|model| model.target.clone())
                    .collect()
            };
            futures::stream::iter(wants)
                .map(|relation| async move {
                    (
                        relation.display(),
                        self.adapter.output_identity(&relation).await.ok().flatten(),
                    )
                })
                .buffered(16)
                .collect()
                .await
        };

        let mut seeds = Vec::with_capacity(needed_seeds.len());
        for seed in needed_seeds.into_values() {
            let relation =
                seed_relation(seed, default_catalog, default_schema, self.adapter.name());
            let (action, reasons) = if blocked {
                (PlanAction::Unknown, Vec::new())
            } else {
                let exists = exists_map
                    .get(&relation.display())
                    .copied()
                    .unwrap_or(false);
                let current = recorded_seeds.get(&seed.name);
                self.decide_seed(seed, &relation.display(), exists, current, options)
            };
            seeds.push(PlannedSeed {
                name: seed.name.clone(),
                target: relation.display(),
                path: seed.path.clone(),
                action,
                desired_version: seed.content_hash.clone(),
                reasons,
            });
        }

        let mut models = Vec::with_capacity(order.len());
        let mut will_build: BTreeSet<ModelId> = BTreeSet::new();
        // Models whose schema-change classification needs the live columns —
        // (index into `models`, target, compiled model). Batched after the
        // loop so `relation_columns_many` can answer them together.
        let mut schema_checks: Vec<(usize, Relation, &CompiledModel)> = Vec::new();
        for id in &order {
            let Some(model) = compilation.model(id) else {
                continue;
            };
            let desired = model.version.clone();

            // How the model entered the plan — the answer to "why is this
            // here" — comes before the change reasons.
            let membership = match member(id) {
                Some(member) if member.is_direct() => Membership::Selected,
                Some(_) => Membership::Expanded,
                None => Membership::Dependency,
            };
            let mut reasons: Vec<PlanReason> = match membership {
                Membership::Selected => selection
                    .causes
                    .get(&id.logical_name())
                    .map(|causes| {
                        causes
                            .iter()
                            .map(|cause| {
                                PlanReason::about(
                                    ReasonKind::GitChange,
                                    format!("selected because {}", cause.detail),
                                    cause.path.clone(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                Membership::Expanded => member(id)
                    .map(|member| {
                        member
                            .expanded
                            .iter()
                            .map(|term| {
                                PlanReason::about(
                                    ReasonKind::SelectionExpansion,
                                    format!("selected by `{term}`"),
                                    term.clone(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                Membership::Dependency => {
                    vec![match requiring.get(id).cloned() {
                        Some(required_by) => PlanReason::about(
                            ReasonKind::SelectedDependency,
                            format!("required by selected model {required_by}"),
                            required_by,
                        ),
                        None => PlanReason::simple(
                            ReasonKind::SelectedDependency,
                            "required by the selection",
                        ),
                    }]
                }
            };

            let (action, mut change_reasons, current_record, exists) = if blocked {
                (PlanAction::Unknown, Vec::new(), None, false)
            } else {
                let evidence = ModelEvidence {
                    exists: exists_map
                        .get(&model.target.display())
                        .copied()
                        .unwrap_or(false),
                    current: materialized.get(&id.logical_name()),
                    elsewhere: by_hash.get(&desired.hash).map(Vec::as_slice).unwrap_or(&[]),
                    live_output: live_outputs.get(&model.target.display()).cloned().flatten(),
                };
                let (action, reasons) =
                    self.decide(model, &evidence, environment.as_deref(), options)?;
                (action, reasons, evidence.current, evidence.exists)
            };
            reasons.append(&mut change_reasons);

            if action == PlanAction::Build {
                will_build.insert(id.clone());
            }

            // Name the upstreams that will rebuild in this same plan so the
            // propagation is legible without cross-referencing rows.
            if action == PlanAction::Build {
                for dependency in model.model_dependencies() {
                    if will_build.contains(dependency) {
                        reasons.push(PlanReason::about(
                            ReasonKind::UpstreamRebuild,
                            format!(
                                "upstream {} will rebuild in this plan",
                                dependency.logical_name()
                            ),
                            dependency.logical_name(),
                        ));
                    }
                }
            }

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
                            let detail = match (&record.incremental_strategy, &desired_strategy) {
                                (Some(was), Some(now)) if was != now => format!(
                                    "incremental strategy changed ({was} → {now}); full rebuild"
                                ),
                                _ => {
                                    "incremental strategy or key changed; full rebuild".to_string()
                                }
                            };
                            reasons.push(PlanReason::simple(ReasonKind::IncrementalChange, detail));
                        }
                    }
                    None => full_rebuild = true,
                }
            }

            // Schema-change classification can force a full rebuild. The
            // column reads are deferred to a batch after the loop — the
            // planner cannot know which models need them until actions are
            // decided, but the read itself is order-independent.
            if action == PlanAction::Build && exists && model.schema.known {
                schema_checks.push((models.len(), model.target.clone(), model));
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
                membership,
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

        // Deferred schema-change classification — one batched column read
        // for every model whose build decision depends on the live schema.
        if !schema_checks.is_empty() {
            let relations: Vec<Relation> = schema_checks
                .iter()
                .map(|(_, relation, _)| relation.clone())
                .collect();
            let found = self.adapter.relation_columns_many(&relations).await;
            for ((index, _, model), columns) in schema_checks.iter().zip(found) {
                let Ok(columns) = columns else {
                    continue;
                };
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
                    models[*index].reasons.push(PlanReason::simple(
                        ReasonKind::SchemaChange,
                        schema_change_detail(&desired_schema, &current_schema, safety),
                    ));
                    if matches!(
                        safety,
                        SchemaChangeSafety::FullRebuildRequired | SchemaChangeSafety::Error
                    ) {
                        models[*index].full_rebuild = true;
                    }
                }
            }
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

        let plan_selection = PlanSelection {
            terms: selection.terms.clone(),
            exclude: selection.exclude_terms.clone(),
            matched: order
                .iter()
                .filter(|id| member(id).is_some_and(|member| member.is_direct()))
                .map(|id| id.logical_name())
                .collect(),
            expanded: order
                .iter()
                .filter(|id| member(id).is_some_and(|member| !member.is_direct()))
                .map(|id| id.logical_name())
                .collect(),
            required: order
                .iter()
                .filter(|id| !member_ids.contains(id))
                .map(|id| id.logical_name())
                .collect(),
        };

        Ok(Plan {
            id: uuid::Uuid::new_v4().to_string(),
            created_at: now_rfc3339(),
            environment,
            adapter: self.adapter.name().to_string(),
            compiler_semantics_version: phlo_transform_core::COMPILER_SEMANTICS_VERSION.to_string(),
            blocked,
            selection: plan_selection,
            git: None,
            warnings,
            seeds,
            models,
            tests,
            diagnostics: compilation.diagnostics.clone(),
        })
    }

    /// The seed analogue of [`Planner::decide`]: content hash + target are
    /// the seed's version.
    fn decide_seed(
        &self,
        seed: &phlo_transform_core::CompiledSeed,
        target: &str,
        exists: bool,
        current: Option<&crate::state::SeedRecord>,
        options: &PlanOptions,
    ) -> (PlanAction, Vec<PlanReason>) {
        if options.force {
            return (
                PlanAction::Build,
                vec![PlanReason::simple(
                    ReasonKind::Forced,
                    "reload forced by --force",
                )],
            );
        }
        if !exists {
            return (
                PlanAction::Build,
                vec![PlanReason::simple(
                    ReasonKind::MissingRelation,
                    format!("seed relation {target} does not exist"),
                )],
            );
        }
        match current {
            Some(record) if record.content_hash == seed.content_hash && record.target == target => {
                (
                    PlanAction::Skip,
                    vec![PlanReason::simple(
                        ReasonKind::Unchanged,
                        "seed content unchanged",
                    )],
                )
            }
            Some(_) => (
                PlanAction::Build,
                vec![PlanReason::simple(
                    ReasonKind::SourceChange,
                    format!("seed {} content changed", seed.name),
                )],
            ),
            None => (
                PlanAction::Build,
                vec![PlanReason::simple(
                    ReasonKind::UnknownState,
                    if self.state.is_some() {
                        "no seed load recorded for this environment".to_string()
                    } else {
                        "no state store; cannot compare against a recorded load".to_string()
                    },
                )],
            ),
        }
    }

    /// Decide a single model from already-fetched evidence: the batched
    /// existence probe, the environment's materialisation record, the
    /// bulked same-version records from other environments, and the live
    /// output identity read in the prefetch pass. No adapter or state calls
    /// happen here — the caller fetched everything this needs.
    fn decide(
        &self,
        model: &CompiledModel,
        evidence: &ModelEvidence<'_>,
        environment: Option<&str>,
        options: &PlanOptions,
    ) -> Result<(PlanAction, Vec<PlanReason>), EngineError> {
        let exists = evidence.exists;
        let current = evidence.current;
        let elsewhere = evidence.elsewhere;
        let live_output = &evidence.live_output;
        let desired = &model.version;
        if options.force {
            return Ok((
                PlanAction::Build,
                vec![PlanReason::simple(
                    ReasonKind::Forced,
                    "rebuild forced by --force",
                )],
            ));
        }
        if !exists {
            return Ok((
                PlanAction::Build,
                vec![PlanReason::simple(
                    ReasonKind::MissingRelation,
                    format!("target relation {} does not exist", model.target.display()),
                )],
            ));
        }
        let Some(current) = current else {
            // Not materialised in this environment; if the exact version
            // exists elsewhere it is a cache candidate — but only a record
            // naming this same physical relation, produced by this adapter,
            // whose recorded strong output identity still matches what the
            // relation reports. A record without a verifiable output
            // identity is a version hash, not evidence.
            if self.state.is_some() {
                let elsewhere: Vec<&MaterializedRecord> = elsewhere
                    .iter()
                    .filter(|record| record.environment.as_deref() != environment)
                    .collect();
                let candidates: Vec<&MaterializedRecord> = elsewhere
                    .iter()
                    .filter(|record| {
                        record.target == model.target.display()
                            && record.adapter.as_deref() == Some(self.adapter.name())
                    })
                    .copied()
                    .collect();
                if !candidates.is_empty() {
                    let live = live_output.clone();
                    if let Some(hit) = candidates.iter().find(|record| {
                        record.output_identity.is_some() && record.output_identity == live
                    }) {
                        let source_env = hit
                            .environment
                            .as_deref()
                            .unwrap_or("the default environment");
                        return Ok((
                            PlanAction::Cached,
                            vec![PlanReason::about(
                                ReasonKind::CacheReuse,
                                format!(
                                    "exact desired version exists in {source_env} on the same relation (run {})",
                                    short(&hit.run_id, 12)
                                ),
                                source_env.to_string(),
                            )],
                        ));
                    }
                    let source_env = candidates[0]
                        .environment
                        .as_deref()
                        .unwrap_or("the default environment");
                    let detail = match live {
                        Some(live) => format!(
                            "identical version recorded in {source_env} but {target} now reports \
                             output `{live}` — another writer owns the relation; rebuilding",
                            target = model.target.display()
                        ),
                        None => format!(
                            "identical version recorded in {source_env} but {target} has no \
                             verifiable output identity; rebuilding",
                            target = model.target.display()
                        ),
                    };
                    return Ok((
                        PlanAction::Build,
                        vec![PlanReason::about(
                            ReasonKind::CacheMiss,
                            detail,
                            source_env.to_string(),
                        )],
                    ));
                }
                if let Some(hit) = elsewhere.first() {
                    let source_env = hit
                        .environment
                        .as_deref()
                        .unwrap_or("the default environment");
                    let detail = if hit.adapter.as_deref() != Some(self.adapter.name()) {
                        format!(
                            "identical version exists in {source_env} but was materialised by \
                             adapter `{}` (this run uses `{}`); metadata-only, rebuilding",
                            hit.adapter.as_deref().unwrap_or("<unrecorded>"),
                            self.adapter.name()
                        )
                    } else {
                        format!(
                            "identical version exists in {source_env} at {} but that relation \
                             is not the one here ({}); metadata-only, rebuilding",
                            hit.target,
                            model.target.display()
                        )
                    };
                    return Ok((
                        PlanAction::Build,
                        vec![PlanReason::about(
                            ReasonKind::CacheMiss,
                            detail,
                            source_env.to_string(),
                        )],
                    ));
                }
            }
            return Ok((
                PlanAction::Build,
                vec![PlanReason::simple(
                    if self.state.is_some() {
                        ReasonKind::UnknownState
                    } else {
                        ReasonKind::StateUnavailable
                    },
                    if self.state.is_some() {
                        "no version recorded for this environment".to_string()
                    } else {
                        "no state store; cannot compare against a recorded version".to_string()
                    },
                )],
            ));
        };

        // A materialisation produced under a different adapter cannot vouch
        // for this output — execution semantics differ between engines — and
        // a record with no adapter cannot vouch for anything.
        match &current.adapter {
            Some(recorded_adapter) if recorded_adapter != self.adapter.name() => {
                return Ok((
                    PlanAction::Build,
                    vec![PlanReason::simple(
                        ReasonKind::AdapterChange,
                        format!(
                            "materialised by adapter `{recorded_adapter}`; current adapter is \
                             `{}`",
                            self.adapter.name()
                        ),
                    )],
                ));
            }
            None => {
                return Ok((
                    PlanAction::Build,
                    vec![PlanReason::simple(
                        ReasonKind::AdapterChange,
                        format!(
                            "materialisation was recorded before adapter identity was tracked; \
                             cannot verify it was produced by `{}`",
                            self.adapter.name()
                        ),
                    )],
                ));
            }
            _ => {}
        }

        // A recorded output identity that no longer matches what the adapter
        // can prove means another writer overwrote the relation — the record
        // is stale, whatever the version hash says.
        if let Some(recorded_output) = &current.output_identity {
            let live = live_output;
            if live.as_deref() != Some(recorded_output.as_str()) {
                return Ok((
                    PlanAction::Build,
                    vec![PlanReason::simple(
                        ReasonKind::OutputDrift,
                        format!(
                            "{} no longer holds the recorded output{} — another writer owns \
                             the relation",
                            model.target.display(),
                            live.as_ref()
                                .map(|live| format!(" (now `{live}`)"))
                                .unwrap_or_default()
                        ),
                    )],
                ));
            }
        }

        if current.version.hash == desired.hash {
            return Ok((
                PlanAction::Skip,
                vec![PlanReason::simple(
                    ReasonKind::Unchanged,
                    "SQL, config, contract and inputs unchanged",
                )],
            ));
        }

        let mut reasons = diff_reasons(model, current);
        if reasons.is_empty() {
            reasons.push(PlanReason::simple(
                ReasonKind::UnknownState,
                "version hash changed but no component differs",
            ));
        }
        Ok((PlanAction::Build, reasons))
    }
}

/// Compare a model's desired version against the recorded materialisation
/// and produce one reason per differing component. Shared by `plan` and
/// `explain` so both describe change identically.
pub fn diff_reasons(model: &CompiledModel, current: &MaterializedRecord) -> Vec<PlanReason> {
    let desired = &model.version;
    let mut reasons = Vec::new();
    if current.version.sql_hash != desired.sql_hash {
        reasons.push(PlanReason::simple(
            ReasonKind::SqlSemanticChange,
            "SQL semantics changed",
        ));
    }
    if current.version.config_hash != desired.config_hash {
        reasons.push(PlanReason::simple(
            ReasonKind::ConfigChange,
            "configuration changed",
        ));
    }
    if current.version.contract_hash != desired.contract_hash {
        reasons.push(contract_diff_reason(model, current));
    }
    if current.version.dependency_hash != desired.dependency_hash {
        reasons.push(dependency_diff_reason(model, current));
    }
    if current.version.source_state_hash != desired.source_state_hash {
        reasons.extend(source_diff_reasons(model, current));
    }
    if current.version.target_hash != desired.target_hash {
        reasons.push(PlanReason::simple(
            ReasonKind::TargetChange,
            format!(
                "physical target changed (was {}, now {})",
                current.target,
                model.target.display()
            ),
        ));
    }
    if current.version.compiler_version != desired.compiler_version {
        reasons.push(PlanReason::simple(
            ReasonKind::CompilerSemanticsChange,
            "compiler semantics changed",
        ));
    }
    reasons
}

/// Explain a contract-hash change with the structured contract diff when
/// both sides' contracts are available — names the breaking changes instead
/// of reporting an opaque hash change.
fn contract_diff_reason(model: &CompiledModel, current: &MaterializedRecord) -> PlanReason {
    let Some(changes) = model
        .contract
        .as_ref()
        .map(|desired| crate::contracts::contract_diff(current.contract.as_ref(), Some(desired)))
    else {
        // The model dropped its contract — report what the base recorded.
        if let Some(previous) = &current.contract {
            let changes = crate::contracts::contract_diff(Some(previous), None);
            return contract_reason_from_changes(changes);
        }
        return PlanReason::simple(ReasonKind::ContractChange, "contract or assertions changed");
    };
    if changes.is_empty() {
        // Assertions changed, or the recorded contract is missing — the hash
        // moved but no column-level diff is derivable.
        return PlanReason::simple(ReasonKind::ContractChange, "contract or assertions changed");
    }
    contract_reason_from_changes(changes)
}

fn contract_reason_from_changes(changes: Vec<crate::contracts::ContractChange>) -> PlanReason {
    let breaking = changes
        .iter()
        .filter(|change| change.safety == crate::contracts::ContractSafety::Breaking)
        .count();
    let summary = changes
        .iter()
        .take(3)
        .map(|change| {
            if change.column.is_empty() {
                change.detail.clone()
            } else {
                format!("{}: {}", change.column, change.detail)
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    let more = if changes.len() > 3 {
        format!(" (+{} more)", changes.len() - 3)
    } else {
        String::new()
    };
    PlanReason::simple(
        ReasonKind::ContractChange,
        format!(
            "contract changed: {summary}{more}{}",
            if breaking > 0 {
                format!(" — {breaking} breaking")
            } else {
                String::new()
            }
        ),
    )
}

/// Explain a changed dependency hash in terms of which dependency's version
/// input moved, using the detail recorded at materialisation time.
fn dependency_diff_reason(model: &CompiledModel, current: &MaterializedRecord) -> PlanReason {
    let desired = &model.version_detail.dependencies;
    match &current.detail {
        Some(detail) => {
            let recorded = &detail.dependencies;
            let mut changed: Vec<String> = Vec::new();
            for name in desired.keys() {
                match recorded.get(name) {
                    Some(was) if *was != desired[name] => changed.push(name.clone()),
                    None => changed.push(format!("{name} (new dependency)")),
                    _ => {}
                }
            }
            for name in recorded.keys() {
                if !desired.contains_key(name) {
                    changed.push(format!("{name} (removed)"));
                }
            }
            if changed.is_empty() {
                PlanReason::simple(
                    ReasonKind::DependencyChange,
                    "upstream version inputs changed",
                )
            } else {
                let subject = changed
                    .first()
                    .map(|name| name.split(" (").next().unwrap_or(name).to_string());
                let mut reason = PlanReason::simple(
                    ReasonKind::DependencyChange,
                    format!("upstream version changed: {}", changed.join(", ")),
                );
                reason.subject = subject;
                reason
            }
        }
        None => PlanReason::simple(
            ReasonKind::DependencyChange,
            "upstream version changed (recorded before dependency detail was tracked)",
        ),
    }
}

/// One reason per source whose observed state moved since materialisation.
fn source_diff_reasons(model: &CompiledModel, current: &MaterializedRecord) -> Vec<PlanReason> {
    let desired = &model.version_detail.sources;
    match &current.detail {
        Some(detail) => {
            let recorded = &detail.sources;
            let mut reasons = Vec::new();
            for (name, now) in desired {
                match recorded.get(name) {
                    Some(was) if was != now => reasons.push(PlanReason::about(
                        ReasonKind::SourceChange,
                        format!(
                            "source {name} changed ({} → {})",
                            short_state(was),
                            short_state(now)
                        ),
                        name.clone(),
                    )),
                    None => reasons.push(PlanReason::about(
                        ReasonKind::SourceChange,
                        format!("source {name} first observed ({})", short_state(now)),
                        name.clone(),
                    )),
                    _ => {}
                }
            }
            for name in recorded.keys() {
                if !desired.contains_key(name) {
                    reasons.push(PlanReason::about(
                        ReasonKind::SourceChange,
                        format!("source {name} removed"),
                        name.clone(),
                    ));
                }
            }
            if reasons.is_empty() {
                reasons.push(PlanReason::simple(
                    ReasonKind::SourceChange,
                    "source states changed",
                ));
            }
            reasons
        }
        None => vec![PlanReason::simple(
            ReasonKind::SourceChange,
            "a source state changed (recorded before source detail was tracked)",
        )],
    }
}

/// A compact description of a classified schema change.
fn schema_change_detail(
    desired: &[SchemaColumn],
    current: &[SchemaColumn],
    safety: SchemaChangeSafety,
) -> String {
    let current_names: BTreeMap<&str, &SchemaColumn> = current
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect();
    let desired_names: BTreeSet<&str> = desired.iter().map(|column| column.name.as_str()).collect();
    let mut pieces: Vec<String> = Vec::new();
    for column in desired {
        match current_names.get(column.name.as_str()) {
            None => pieces.push(format!("added {}", column.name)),
            Some(was) if was.data_type != column.data_type => pieces.push(format!(
                "{}: {:?} → {:?}",
                column.name, was.data_type, column.data_type
            )),
            _ => {}
        }
    }
    for column in current {
        if !desired_names.contains(column.name.as_str()) {
            pieces.push(format!("removed {}", column.name));
        }
    }
    let label = match safety {
        SchemaChangeSafety::FullRebuildRequired => "schema change requires a full rebuild",
        SchemaChangeSafety::Error => "incompatible schema change",
        _ => "output schema changed",
    };
    if pieces.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {}", pieces.join(", "))
    }
}

/// Expand a selection to include every transitive model dependency.
pub fn dependency_closure(compilation: &Compilation, selected: &[ModelId]) -> BTreeSet<ModelId> {
    dependency_closure_excluding(
        compilation,
        &selected.iter().cloned().collect(),
        &BTreeSet::new(),
    )
}

/// Dependency closure that never pulls excluded models back in.
fn dependency_closure_excluding(
    compilation: &Compilation,
    selected: &BTreeSet<ModelId>,
    excluded: &BTreeSet<ModelId>,
) -> BTreeSet<ModelId> {
    let mut included: BTreeSet<ModelId> = selected.clone();
    let mut frontier: Vec<ModelId> = selected.iter().cloned().collect();
    while let Some(id) = frontier.pop() {
        for dependency in compilation.dependencies(&id) {
            if let Dependency::Model(dependency_id) = dependency {
                if excluded.contains(&dependency_id) {
                    continue;
                }
                if included.insert(dependency_id.clone()) {
                    frontier.push(dependency_id);
                }
            }
        }
    }
    included
}

/// The per-model evidence `decide` consumes — every field is fetched in the
/// planner's batched prefetch pass, so deciding a model costs no round trips.
struct ModelEvidence<'a> {
    /// The target relation exists (batched `relations_exist` probe).
    exists: bool,
    /// The version recorded for this model in the plan's environment scope.
    current: Option<&'a MaterializedRecord>,
    /// Same-version records materialised anywhere — cache-reuse candidates.
    elsewhere: &'a [MaterializedRecord],
    /// The relation's live output identity, when one was probed.
    live_output: Option<String>,
}

/// The materialisation records a plan consults for `environment`: the
/// env's own records keyed by model id. `main` and the unlabeled default
/// share one physical catalog, so each folds the other's records in —
/// matching the scope `branch_diff` uses — with the latest
/// `materialized_at` winning when both describe the same model.
fn materialized_scope(
    state: &dyn StateStore,
    environment: Option<&str>,
) -> Result<BTreeMap<String, MaterializedRecord>, EngineError> {
    match environment {
        Some(environment) => crate::branch_diff::materialized_for_environment(state, environment),
        None => {
            let mut records: BTreeMap<String, MaterializedRecord> = state
                .materialized_in(None)?
                .into_iter()
                .map(|record| (record.model_id.clone(), record))
                .collect();
            crate::branch_diff::merge_latest(
                &mut records,
                state.materialized_in(Some("main"))?,
                |record| &record.model_id,
                |record| &record.materialized_at,
            );
            Ok(records)
        }
    }
}

/// The seed records a plan consults for `environment` — the same
/// default/`main` convention as [`materialized_scope`], latest `loaded_at`
/// wins.
fn seeds_scope(
    state: &dyn StateStore,
    environment: Option<&str>,
) -> Result<BTreeMap<String, SeedRecord>, EngineError> {
    match environment {
        Some(environment) => crate::branch_diff::seeds_for_environment(state, environment),
        None => {
            let mut records: BTreeMap<String, SeedRecord> = state
                .seeds_in(None)?
                .into_iter()
                .map(|record| (record.name.clone(), record))
                .collect();
            crate::branch_diff::merge_latest(
                &mut records,
                state.seeds_in(Some("main"))?,
                |record| &record.name,
                |record| &record.loaded_at,
            );
            Ok(records)
        }
    }
}

/// For every model the selection pulls in, the nearest selected dependent
/// that requires it — one multi-source BFS from the selection down the
/// dependency edges, instead of a per-model walk that costs O(E) each.
fn requiring_models(
    compilation: &Compilation,
    selected: &BTreeSet<ModelId>,
) -> BTreeMap<ModelId, String> {
    let mut claims: BTreeMap<ModelId, String> = BTreeMap::new();
    let mut frontier: VecDeque<(ModelId, ModelId)> =
        selected.iter().map(|id| (id.clone(), id.clone())).collect();
    while let Some((current, source)) = frontier.pop_front() {
        for dependency in compilation.dependencies(&current) {
            let Dependency::Model(dependency_id) = dependency else {
                continue;
            };
            if selected.contains(&dependency_id) || claims.contains_key(&dependency_id) {
                continue;
            }
            claims.insert(dependency_id.clone(), source.logical_name());
            frontier.push_back((dependency_id, source.clone()));
        }
    }
    claims
}

/// Abbreviate a hash/id for display: keep the recognisable prefix.
fn short(value: &str, len: usize) -> String {
    if value.len() <= len {
        value.to_string()
    } else {
        format!("{}…", &value[..len])
    }
}

/// Source states can be long hashes; show a recognisable prefix.
fn short_state(state: &str) -> String {
    if state.is_empty() {
        "unobserved".to_string()
    } else {
        short(state, 20)
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}
