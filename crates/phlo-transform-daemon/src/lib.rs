//! Local semantic service and machine-facing API for Phlo Transform.
//!
//! The daemon wraps the same compiler and engine libraries as the CLI. It
//! holds an immutable compiled snapshot behind an `RwLock` so concurrent
//! readers never observe partially updated state, and recompiles on demand or
//! when watched files change.
//!
//! The API is split in two planes:
//!
//! - **Reads** (`GET /v1/...`) never mutate workspace, state, or warehouse.
//!   They return the same serializable report DTOs as the CLI's `--json`.
//!   Warehouse-touching reads (`/v1/plan`, `/v1/diff/branch`) need an adapter
//!   and answer `API007` when none is configured.
//! - **Operations** (`POST /v1/operations`) submit long-running or mutating
//!   work (`run`, `test`, `promote`, `reload`) and return a handle with a
//!   stable lifecycle: queued → running → succeeded/failed/cancelled.
//!   `POST /v1/operations/{id}/cancel` cancels cooperatively; resubmitting a
//!   body with the same `idempotency_key` returns the existing handle.
//!
//! Errors are `{"error": {"code": "APIxxx", "message": "..."}}` with stable
//! codes documented in `docs/daemon.md`.

pub mod execute;
pub mod operations;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use phlo_transform_core::{
    compile, load_project, resolve_selection, ColumnRef, Compilation, ModelId, SelectorSet,
    SemanticProject,
};
use phlo_transform_engine::util::now_rfc3339;
use phlo_transform_engine::{
    branch_diff, compiled_catalog, environment_catalog, Adapter, ArtifactWriter, BranchDiffRequest,
    EngineError, EnvironmentContext, EnvironmentMode, ExecutionStatus, PlanOptions, Planner,
    RunSummary, StateStore,
};
use phlo_transform_nessie::NessieClient;

use crate::operations::{parse_submission, OperationStatus, OperationStore, Params, SubmitOutcome};

/// What the daemon was launched with. Every field is optional: a daemon
/// without an adapter serves the offline read API, and answers `API007` on
/// endpoints that need the missing piece.
#[derive(Default)]
pub struct ServiceConfig {
    pub adapter: Option<Arc<dyn Adapter>>,
    pub state: Option<Arc<dyn StateStore>>,
    pub nessie: Option<Arc<dyn NessieClient>>,
    /// Default environment label for operations (the CLI's `--environment`).
    pub environment: Option<String>,
    /// Compiled-catalog override (the CLI's `--catalog`).
    pub catalog: Option<String>,
    /// Fallback schema for diff seed targets (the CLI's `--trino-schema`).
    pub default_schema: Option<String>,
    /// The Nessie URI catalogs should be provisioned against — the address
    /// the *warehouse* uses to reach Nessie, which may differ from the URI
    /// the daemon's own client uses (e.g. a container-network hostname).
    /// Environment-targeted operations pass it to `ensure_catalog`.
    pub nessie_uri: Option<String>,
    /// Iceberg warehouse location for provisioned catalogs (the CLI's
    /// `--warehouse`).
    pub warehouse: Option<String>,
    /// Bearer token required on every endpoint except `/status`
    /// (the CLI's `daemon --token`). `None` serves unauthenticated —
    /// appropriate only for loopback use.
    pub token: Option<String>,
}

/// A live, coherent workspace snapshot plus engine handles.
pub struct WorkspaceService {
    root: PathBuf,
    snapshot: RwLock<Arc<Compilation>>,
    last_update: RwLock<String>,
    config: ServiceConfig,
    ops: Arc<OperationStore>,
}

impl WorkspaceService {
    /// Load a workspace root with no engine handles (offline read API).
    pub fn load(root: &Path) -> Arc<Self> {
        Self::load_with_config(root, ServiceConfig::default())
    }

    /// Load a workspace root with the given engine handles.
    pub fn load_with_config(root: &Path, config: ServiceConfig) -> Arc<Self> {
        let compilation = Arc::new(compile_root(root, config.catalog.as_deref()));
        Arc::new(Self {
            root: root.to_path_buf(),
            snapshot: RwLock::new(compilation),
            last_update: RwLock::new(now_rfc3339()),
            ops: Arc::new(OperationStore::open(
                ArtifactWriter::for_workspace(root)
                    .directory()
                    .join("operations.jsonl"),
            )),
            config,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn snapshot(&self) -> Arc<Compilation> {
        self.snapshot.read().expect("snapshot lock").clone()
    }

    pub fn adapter(&self) -> Option<Arc<dyn Adapter>> {
        self.config.adapter.clone()
    }

    pub fn state(&self) -> Option<Arc<dyn StateStore>> {
        self.config.state.clone()
    }

    pub fn nessie(&self) -> Option<Arc<dyn NessieClient>> {
        self.config.nessie.clone()
    }

    pub fn default_environment(&self) -> Option<&str> {
        self.config.environment.as_deref()
    }

    /// The `--catalog` override, when configured.
    pub fn catalog_override(&self) -> Option<&str> {
        self.config.catalog.as_deref()
    }

    /// The catalog-facing Nessie URI used when provisioning environments.
    pub fn nessie_uri(&self) -> Option<&str> {
        self.config.nessie_uri.as_deref()
    }

    /// The warehouse location for provisioned catalogs.
    pub fn warehouse(&self) -> Option<&str> {
        self.config.warehouse.as_deref()
    }

    /// The bearer token the API requires, when one was configured.
    pub fn token(&self) -> Option<&str> {
        self.config.token.as_deref()
    }

    /// The environment-resolution context every operation and read shares —
    /// one code path so a plan's physical target is always the target a run
    /// against the same environment would execute. Borrows the service.
    pub fn environment_context(&self) -> EnvironmentContext<'_> {
        EnvironmentContext {
            root: &self.root,
            nessie: self.config.nessie.as_deref(),
            adapter: self.config.adapter.as_deref(),
            state: self.config.state.as_deref(),
            nessie_uri: self.config.nessie_uri.as_deref(),
            warehouse: self.config.warehouse.as_deref(),
            catalog: self.config.catalog.as_deref(),
        }
    }

    pub fn operations(&self) -> Arc<OperationStore> {
        self.ops.clone()
    }

    /// Recompile the workspace and publish a new snapshot atomically.
    pub fn reload(&self) {
        let compilation = Arc::new(compile_root(&self.root, self.config.catalog.as_deref()));
        *self.snapshot.write().expect("snapshot lock") = compilation;
        *self.last_update.write().expect("snapshot lock") = now_rfc3339();
    }

    pub fn last_update(&self) -> String {
        self.last_update.read().expect("snapshot lock").clone()
    }
}

fn compile_root(root: &Path, catalog: Option<&str>) -> Compilation {
    match load_project(root) {
        Ok(mut project) => {
            // The `--catalog` override applies to every compile — the same
            // rule the CLI applies before `compile(&project)`.
            if let Some(catalog) = catalog {
                project.defaults.catalog = Some(catalog.to_string());
            }
            compile(&project)
        }
        Err(diagnostics) => {
            let project = SemanticProject {
                workspace_root: Some(root.to_path_buf()),
                diagnostics,
                ..SemanticProject::default()
            };
            compile(&project)
        }
    }
}

/// Build the versioned API router.
pub fn router(service: Arc<WorkspaceService>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/v1/status", get(status))
        .route("/v1/check", get(check))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(inspect))
        .route("/v1/lineage", get(lineage_document))
        .route("/v1/lineage/{target}", get(lineage))
        .route("/v1/impact", get(impact_selection))
        .route("/v1/impact/{target}", get(impact))
        .route("/v1/graph", get(graph))
        .route("/v1/plan", get(plan))
        .route("/v1/diff/lineage", get(diff_lineage))
        .route("/v1/diff/branch", get(diff_branch))
        .route("/v1/state/runs", get(state_runs))
        .route("/v1/state/runs/{id}", get(state_run))
        .route("/v1/state/runs/{id}/failed", get(state_run_failed))
        .route("/v1/state/models/{id}", get(state_model))
        .route("/v1/state/promotions", get(state_promotions))
        .route(
            "/v1/operations",
            get(operations_list).post(operations_submit),
        )
        .route("/v1/operations/{id}", get(operations_get))
        .route("/v1/operations/{id}/cancel", post(operations_cancel))
        .route("/v1/reload", post(reload_handler))
        .layer(middleware::from_fn_with_state(
            service.clone(),
            require_auth,
        ))
        .with_state(service)
}

/// Bearer-token gate: when the service was configured with a token, every
/// endpoint except `/status` (liveness) requires `Authorization: Bearer
/// <token>`. Without a token the API is unauthenticated — loopback only.
async fn require_auth(
    State(service): State<Arc<WorkspaceService>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(token) = service.token() else {
        return next.run(request).await;
    };
    if request.uri().path() == "/status" {
        return next.run(request).await;
    }
    let authorised = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {token}"));
    if authorised {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "code": "API015",
                    "message": "missing or invalid bearer token",
                }
            })),
        )
            .into_response()
    }
}

/// Serve the API on a local address until the process is stopped.
pub async fn serve(
    service: Arc<WorkspaceService>,
    address: SocketAddr,
) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, router(service)).await
}

/// Reload the workspace when any watched file changes (polling watcher).
pub fn spawn_watcher(
    service: Arc<WorkspaceService>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut previous = workspace_fingerprint(service.root());
        loop {
            tokio::time::sleep(interval).await;
            let current = workspace_fingerprint(service.root());
            if current != previous {
                previous = current;
                let service = service.clone();
                let _ = tokio::task::spawn_blocking(move || service.reload()).await;
            }
        }
    })
}

fn workspace_fingerprint(root: &Path) -> u64 {
    let mut fingerprint = 0u64;
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | ".phlo" | "node_modules")
            )
        })
        .filter_map(Result::ok)
    {
        let is_relevant = entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension == "sql" || extension == "toml")
            .unwrap_or(false);
        if !is_relevant {
            continue;
        }
        if let Ok(metadata) = entry.metadata() {
            if let Ok(modified) = metadata.modified() {
                if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                    fingerprint ^= duration.as_nanos() as u64;
                    fingerprint = fingerprint.rotate_left(7);
                }
            }
        }
    }
    fingerprint
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn api_error(status: StatusCode, code: &str, message: String) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
}

/// Parse query parameters with a structured error — `serde_urlencoded`
/// handles repeated `?select=` keys into `Vec<String>` fields.
fn parse_query<T: for<'de> Deserialize<'de>>(
    raw: &Option<String>,
) -> Result<T, (StatusCode, Json<Value>)> {
    serde_urlencoded::from_str(raw.as_deref().unwrap_or("")).map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "API012",
            format!("invalid query parameters: {error}"),
        )
    })
}

fn missing(what: &str) -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "API007",
        format!("{what} is not configured for this daemon instance"),
    )
}

async fn status(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let snapshot = service.snapshot();
    let errors = snapshot.errors().count();
    Ok(Json(json!({
        "workspace_root": snapshot.workspace_root.as_ref().map(|path| path.to_string_lossy().replace('\\', "/")),
        "compiler_semantics_version": phlo_transform_core::COMPILER_SEMANTICS_VERSION,
        "models": snapshot.models.len(),
        "sources": snapshot.sources().len(),
        "tests": snapshot.tests.len(),
        "diagnostics": snapshot.diagnostics.len(),
        "errors": errors,
        "capabilities": {
            "adapter": service.adapter().is_some(),
            "state": service.state().is_some(),
            "nessie": service.nessie().is_some(),
        },
        "last_update": service.last_update(),
    })))
}

async fn check(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let snapshot = service.snapshot();
    Ok(Json(
        serde_json::to_value(snapshot.check_report()).expect("report serialises"),
    ))
}

async fn models(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let snapshot = service.snapshot();
    Ok(Json(
        serde_json::to_value(snapshot.list_report()).expect("report serialises"),
    ))
}

async fn inspect(
    State(service): State<Arc<WorkspaceService>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult {
    let snapshot = service.snapshot();
    let Ok(model_id) = ModelId::parse(&id) else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "API001",
            format!("invalid model id `{id}`"),
        ));
    };
    match snapshot.inspect_report(&model_id) {
        Some(report) => Ok(Json(
            serde_json::to_value(report).expect("report serialises"),
        )),
        None => Err(api_error(
            StatusCode::NOT_FOUND,
            "API002",
            format!("no such model: {}", model_id.logical_name()),
        )),
    }
}

async fn lineage(
    State(service): State<Arc<WorkspaceService>>,
    AxumPath(target): AxumPath<String>,
) -> ApiResult {
    let snapshot = service.snapshot();
    if let Ok(id) = ModelId::parse(&target) {
        if snapshot.model(&id).is_some() {
            let report = snapshot.model_lineage_report(&id).expect("model exists");
            return Ok(Json(
                serde_json::to_value(report).expect("report serialises"),
            ));
        }
    }
    let Some((model, column)) = target.rsplit_once('.') else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "API003",
            format!("invalid lineage target `{target}`"),
        ));
    };
    let Ok(id) = ModelId::parse(model) else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "API003",
            format!("invalid model `{model}`"),
        ));
    };
    match snapshot.column_lineage_report(&id, column) {
        Some(report) => Ok(Json(
            serde_json::to_value(report).expect("report serialises"),
        )),
        None => Err(api_error(
            StatusCode::NOT_FOUND,
            "API004",
            format!("no column lineage for `{target}`"),
        )),
    }
}

/// The full lineage document — `phlo-transform lineage --format graph`.
async fn lineage_document(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let snapshot = service.snapshot();
    Ok(Json(
        serde_json::to_value(snapshot.lineage.document()).expect("document serialises"),
    ))
}

/// `phlo-transform impact <target>` — a model name reports its downstream
/// models and their tests; `model.column` (or `dataset.column`) reports the
/// column-level impact report.
async fn impact(
    State(service): State<Arc<WorkspaceService>>,
    AxumPath(target): AxumPath<String>,
) -> ApiResult {
    let snapshot = service.snapshot();
    // A model-only target reports downstream models/tests (works offline).
    if let Some(model) = snapshot.model_by_name(&target) {
        let lineage = snapshot
            .model_lineage_report(&model.id)
            .expect("model exists");
        let mut tests: Vec<String> = Vec::new();
        for dependent in &lineage.downstream {
            if let Ok(id) = ModelId::parse(dependent) {
                tests.extend(
                    snapshot
                        .tests_for(&id)
                        .iter()
                        .map(|test| test.id.to_string()),
                );
            }
        }
        return Ok(Json(json!({
            "model": model.id.logical_name(),
            "downstream_models": lineage.downstream,
            "tests": tests,
        })));
    }
    let Some((model, name)) = target.rsplit_once('.') else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "API005",
            format!("invalid impact target `{target}`; expected model or model.column"),
        ));
    };
    // A model column first; otherwise the prefix may be a source or seed
    // dataset (`impact external.samples.volume`), like the CLI.
    let target = if let Ok(id) = ModelId::parse(model) {
        if snapshot.model(&id).is_some() {
            ColumnRef::model(id, name)
        } else if let Some(dataset) = snapshot.lineage.dataset_by_name(model) {
            let source = phlo_transform_core::SourceId::new(dataset.parts().to_vec())
                .map_err(|error| api_error(StatusCode::BAD_REQUEST, "API005", error.to_string()))?;
            ColumnRef::source(source, name)
        } else {
            return Err(api_error(
                StatusCode::NOT_FOUND,
                "API005",
                format!("no such model or dataset: {model}"),
            ));
        }
    } else if let Some(dataset) = snapshot.lineage.dataset_by_name(model) {
        let source = phlo_transform_core::SourceId::new(dataset.parts().to_vec())
            .map_err(|error| api_error(StatusCode::BAD_REQUEST, "API005", error.to_string()))?;
        ColumnRef::source(source, name)
    } else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "API005",
            format!("invalid model `{model}`; no such dataset either"),
        ));
    };
    Ok(Json(
        serde_json::to_value(snapshot.impact_report(&target)).expect("report serialises"),
    ))
}

#[derive(Debug, Deserialize)]
struct ImpactQuery {
    /// Selector terms — repeat `?select=` for each (same syntax as the CLI).
    #[serde(default, deserialize_with = "string_or_seq")]
    select: Vec<String>,
}

/// `phlo-transform impact --select <terms>` — the selection's blast radius:
/// dependents outside the selected set, plus the tests covering them.
async fn impact_selection(
    RawQuery(raw): RawQuery,
    State(service): State<Arc<WorkspaceService>>,
) -> ApiResult {
    let query = parse_query::<ImpactQuery>(&raw)?;
    if query.select.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "API005",
            "impact needs a target (`/v1/impact/{target}`) or `?select=` terms".to_string(),
        ));
    }
    let snapshot = service.snapshot();
    let set = match SelectorSet::parse(&query.select, &[], &[], false, false) {
        Ok(set) => set,
        Err(error) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "API006",
                error.to_string(),
            ))
        }
    };
    let selection = match resolve_selection(&snapshot, &set, None) {
        Ok(selection) => selection,
        Err(error) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "API006",
                error.to_string(),
            ))
        }
    };
    let members: std::collections::BTreeSet<ModelId> = selection.ids().into_iter().collect();
    let mut impacted: std::collections::BTreeSet<ModelId> = std::collections::BTreeSet::new();
    for member in &members {
        for node in snapshot
            .lineage
            .downstream_transitive(&phlo_transform_core::LineageNode::Model(member.clone()))
        {
            if let phlo_transform_core::LineageNode::Model(id) = node {
                impacted.insert(id);
            }
        }
    }
    for member in &members {
        impacted.remove(member);
    }
    let mut tests: Vec<String> = Vec::new();
    for id in &impacted {
        for test in snapshot
            .lineage
            .tests_for_dataset(&snapshot.lineage.output_dataset(id))
        {
            tests.push(test.to_string());
        }
    }
    let impacted: Vec<String> = impacted.iter().map(|id| id.logical_name()).collect();
    Ok(Json(json!({
        "selected": selection,
        "impacted_models": impacted,
        "tests": tests,
    })))
}

async fn graph(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let snapshot = service.snapshot();
    Ok(Json(
        serde_json::to_value(snapshot.graph_artifact()).expect("report serialises"),
    ))
}

/// `?select=a` alone parses as a scalar in serde_urlencoded — accept both a
/// single term and repeated `?select=` keys.
fn string_or_seq<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

#[derive(Debug, Deserialize)]
struct PlanQuery {
    /// Selector terms — repeat `?select=` for each (same syntax as the CLI).
    #[serde(default, deserialize_with = "string_or_seq")]
    select: Vec<String>,
    environment: Option<String>,
    /// The ref `environment` resolves against — `run`'s `base` param.
    /// Defaults to `main`.
    base: Option<String>,
    #[serde(default)]
    force: bool,
}

/// `phlo-transform plan` — needs an adapter (and optionally a state store
/// for change detection), exactly like the CLI. An `environment` resolves
/// through the same `EnvironmentContext` a run uses, in `ReadOnly` mode:
/// the plan targets the candidate's physical catalog (existing relations
/// may report missing) without provisioning anything.
async fn plan(RawQuery(raw): RawQuery, State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let query = parse_query::<PlanQuery>(&raw)?;
    let Some(adapter) = service.adapter() else {
        return Err(missing("adapter"));
    };
    let environment = query
        .environment
        .or_else(|| service.default_environment().map(str::to_string));
    let base_ref = phlo_transform_engine::base_ref_for(
        service.root(),
        service.state().as_deref(),
        environment.as_deref(),
        query.base.as_deref(),
    )
    .map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )
    })?;
    let target = service
        .environment_context()
        .resolve(environment.as_deref(), &base_ref, EnvironmentMode::ReadOnly)
        .await
        .map_err(|error| match error {
            EngineError::NotConfigured(_) => {
                api_error(StatusCode::SERVICE_UNAVAILABLE, "API007", error.to_string())
            }
            error => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API011",
                error.to_string(),
            ),
        })?;
    let compilation = target
        .compilation
        .map_or_else(|| service.snapshot(), Arc::new);
    let set = match SelectorSet::parse(&query.select, &[], &[], false, false) {
        Ok(set) => set,
        Err(error) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "API006",
                error.to_string(),
            ))
        }
    };
    let selection = match resolve_selection(&compilation, &set, None) {
        Ok(selection) => selection,
        Err(error) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "API006",
                error.to_string(),
            ))
        }
    };
    let planner = Planner::new(adapter, service.state());
    match planner
        .plan(
            &compilation,
            &selection,
            environment,
            &PlanOptions { force: query.force },
        )
        .await
    {
        Ok(plan) => Ok(Json(serde_json::to_value(&plan).expect("plan serialises"))),
        Err(error) => Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct LineageDiffQuery {
    /// Git ref to compare against — `phlo-transform lineage --diff <ref>`.
    base: String,
    /// Optional second ref — `lineage --diff <base> <candidate>` compares
    /// the exact refs; omitted compares merge-base → workspace.
    candidate: Option<String>,
}

/// `phlo-transform lineage --diff <base> [candidate]` — the shared
/// engine orchestration (`LineageDiffContext`), so the response is the
/// same provenance artifact the CLI writes and `promote` audits. The base
/// tree is extracted into a temp dir and compiled; the worktree is never
/// touched.
async fn diff_lineage(
    RawQuery(raw): RawQuery,
    State(service): State<Arc<WorkspaceService>>,
) -> ApiResult {
    let query = parse_query::<LineageDiffQuery>(&raw)?;
    let context = phlo_transform_engine::LineageDiffContext {
        root: service.root().to_path_buf(),
        workspace: Some(service.snapshot()),
        catalog: service.catalog_override().map(str::to_string),
        adapter: service.adapter(),
        nessie: service.nessie(),
        candidate_env: service.default_environment().map(str::to_string),
        write_artifact: true,
        state: service.state(),
    };
    let result = match &query.candidate {
        Some(candidate) => context.diff_ref_vs_ref(&query.base, candidate).await,
        None => context.diff_vs_ref(&query.base).await,
    };
    match result {
        Ok(artifact) => Ok(Json(
            serde_json::to_value(&artifact).expect("artifact serialises"),
        )),
        Err(
            error @ (phlo_transform_engine::EngineError::Git(_)
            | phlo_transform_engine::EngineError::FailedDiagnostics { .. }),
        ) => Err(api_error(
            StatusCode::BAD_REQUEST,
            "API010",
            format!("lineage diff failed: {error}"),
        )),
        Err(error) => Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct BranchDiffQuery {
    /// Candidate ref — the CLI's `--from`/`--ref`.
    from: String,
    /// Base ref — the CLI's `--to` (default `main`).
    to: Option<String>,
    /// Run keyed value diffs — the CLI's `--full`.
    #[serde(default)]
    full: bool,
}

/// `phlo-transform diff --from <ref> --to <ref> [--full]` — read-only on
/// the workspace but queries the warehouse; needs an adapter. When Nessie
/// is configured both refs are validated like the CLI does.
async fn diff_branch(
    RawQuery(raw): RawQuery,
    State(service): State<Arc<WorkspaceService>>,
) -> ApiResult {
    let query = parse_query::<BranchDiffQuery>(&raw)?;
    let Some(adapter) = service.adapter() else {
        return Err(missing("adapter"));
    };
    let candidate_ref = query.from;
    let base_ref = query.to.unwrap_or_else(|| "main".to_string());
    // When Nessie is configured both sides must be real references — the
    // resolved heads are recorded on the report so `promote` can verify the
    // audit covered exactly the commits being merged.
    let mut candidate_hash = None;
    let mut base_hash = None;
    if let Some(nessie) = service.nessie() {
        for (name, slot) in [
            (&candidate_ref, &mut candidate_hash),
            (&base_ref, &mut base_hash),
        ] {
            match nessie.get_reference(name).await {
                Ok(Some(reference)) => *slot = Some(reference.hash),
                Ok(None) => {
                    return Err(api_error(
                        StatusCode::NOT_FOUND,
                        "API013",
                        format!("reference `{name}` was not found"),
                    ))
                }
                Err(error) => {
                    return Err(api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "API011",
                        error.to_string(),
                    ))
                }
            }
        }
    }
    let compilation = service.snapshot();
    let root = service.root();
    // The shared catalog resolution — the same precedence the CLI's `diff`
    // applies: candidate through its recorded/generated binding, `main`
    // through the deployment catalog, other refs through their bindings.
    let candidate_catalog = environment_catalog(root, service.state().as_deref(), &candidate_ref)
        .map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )
    })?;
    let base_catalog = phlo_transform_engine::base_catalog(
        root,
        service.state().as_deref(),
        &base_ref,
        service.config.catalog.as_deref(),
        compiled_catalog(&compilation).as_deref(),
    )
    .map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )
    })?;
    let report = branch_diff(
        adapter,
        service.state().as_deref(),
        &compilation,
        &BranchDiffRequest {
            candidate_ref: candidate_ref.clone(),
            base_ref: base_ref.clone(),
            candidate_catalog: Some(candidate_catalog),
            base_catalog,
            candidate_hash,
            base_hash,
            deep: query.full,
            default_schema: service.config.default_schema.clone(),
        },
    )
    .await;
    match report {
        Ok(report) => {
            // Persist the audit evidence exactly like the CLI: the store
            // record is the portable authority `promote` reads; the
            // artifact stays the human-readable export.
            phlo_transform_engine::persist_branch_diff(root, service.state().as_deref(), &report)
                .map_err(|error| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "API011",
                    error.to_string(),
                )
            })?;
            Ok(Json(
                serde_json::to_value(&report).expect("report serialises"),
            ))
        }
        Err(error) => Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )),
    }
}

fn require_state(
    service: &Arc<WorkspaceService>,
) -> Result<Arc<dyn StateStore>, (StatusCode, Json<Value>)> {
    service.state().ok_or_else(|| missing("state"))
}

#[derive(Debug, Deserialize)]
struct EnvQuery {
    environment: Option<String>,
}

/// `phlo-transform state runs` — newest first.
async fn state_runs(
    RawQuery(raw): RawQuery,
    State(service): State<Arc<WorkspaceService>>,
) -> ApiResult {
    let query = parse_query::<EnvQuery>(&raw)?;
    let state = require_state(&service)?;
    let runs = state.runs().map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )
    })?;
    let env = query
        .environment
        .or_else(|| service.default_environment().map(str::to_string));
    let runs: Vec<_> = runs
        .into_iter()
        .filter(|run| {
            env.as_deref()
                .map(|env| run.environment.as_deref() == Some(env))
                .unwrap_or(true)
        })
        .collect();
    Ok(Json(serde_json::to_value(&runs).expect("runs serialise")))
}

/// Resolve a run id or unique prefix to its summary — the CLI's prefix
/// semantics: `API013` when nothing matches, `API014` when ambiguous.
fn resolve_run(state: &dyn StateStore, id: &str) -> Result<RunSummary, (StatusCode, Json<Value>)> {
    phlo_transform_engine::find_unique_run(state, id).map_err(|error| match error {
        phlo_transform_engine::EngineError::Ambiguous(_) => {
            api_error(StatusCode::BAD_REQUEST, "API014", error.to_string())
        }
        phlo_transform_engine::EngineError::NotFound(_) => {
            api_error(StatusCode::NOT_FOUND, "API013", error.to_string())
        }
        _ => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        ),
    })
}

/// `phlo-transform state show <run>` — run record + stored plan + per-item
/// records. The id may be a unique prefix, like the CLI.
async fn state_run(
    State(service): State<Arc<WorkspaceService>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult {
    let state = require_state(&service)?;
    let summary = resolve_run(state.as_ref(), &id)?;
    let stored = state
        .run(&summary.run_id)
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API011",
                error.to_string(),
            )
        })?
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                "API013",
                format!("run `{}` was not found", summary.run_id),
            )
        })?;
    let models = state.model_runs(&summary.run_id).map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )
    })?;
    let seeds = state.seed_runs(&summary.run_id).map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )
    })?;
    let tests = state.test_runs(&summary.run_id).map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )
    })?;
    Ok(Json(json!({
        "run": stored.record,
        "plan": stored.plan,
        "models": models,
        "seeds": seeds,
        "tests": tests,
    })))
}

/// `phlo-transform state show <run> --failed` — the failed/blocked/
/// cancelled items of one run: what `resume`/`retry_failed` would pick up.
async fn state_run_failed(
    State(service): State<Arc<WorkspaceService>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult {
    let state = require_state(&service)?;
    let summary = resolve_run(state.as_ref(), &id)?;
    let models = state
        .model_runs(&summary.run_id)
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API011",
                error.to_string(),
            )
        })?
        .into_iter()
        .filter(|record| {
            matches!(
                record.status,
                ExecutionStatus::Failed | ExecutionStatus::Blocked | ExecutionStatus::Cancelled
            )
        })
        .collect::<Vec<_>>();
    let seeds = state
        .seed_runs(&summary.run_id)
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API011",
                error.to_string(),
            )
        })?
        .into_iter()
        .filter(|record| {
            matches!(
                record.status,
                ExecutionStatus::Failed | ExecutionStatus::Blocked | ExecutionStatus::Cancelled
            )
        })
        .collect::<Vec<_>>();
    let tests = state
        .test_runs(&summary.run_id)
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API011",
                error.to_string(),
            )
        })?
        .into_iter()
        .filter(|record| record.status == ExecutionStatus::Failed)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "run_id": summary.run_id,
        "models": models,
        "seeds": seeds,
        "tests": tests,
    })))
}

/// `phlo-transform state model <id>` — the recorded materialisation for a
/// model in an environment.
async fn state_model(
    AxumPath(id): AxumPath<String>,
    RawQuery(raw): RawQuery,
    State(service): State<Arc<WorkspaceService>>,
) -> ApiResult {
    let query = parse_query::<EnvQuery>(&raw)?;
    let state = require_state(&service)?;
    let Ok(model_id) = ModelId::parse(&id) else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "API001",
            format!("invalid model id `{id}`"),
        ));
    };
    let env = query
        .environment
        .or_else(|| service.default_environment().map(str::to_string));
    let record = state
        .materialized_version(&model_id.logical_name(), env.as_deref())
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API011",
                error.to_string(),
            )
        })?;
    match record {
        Some(record) => Ok(Json(
            serde_json::to_value(&record).expect("record serialises"),
        )),
        None => Err(api_error(
            StatusCode::NOT_FOUND,
            "API013",
            format!(
                "no materialisation recorded for {} in environment {}",
                model_id.logical_name(),
                env.as_deref().unwrap_or("<default>")
            ),
        )),
    }
}

/// `phlo-transform state promotions` — newest first.
async fn state_promotions(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let state = require_state(&service)?;
    let promotions = state.promotions().map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API011",
            error.to_string(),
        )
    })?;
    Ok(Json(
        serde_json::to_value(&promotions).expect("promotions serialise"),
    ))
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// `POST /v1/operations` — submit a long-running operation.
/// Body: `{kind, idempotency_key?, params?}` or params spread at top level.
/// The `Idempotency-Key` header is honoured too (header wins).
async fn operations_submit(
    State(service): State<Arc<WorkspaceService>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult {
    let header_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "API012",
                format!("request body is not valid JSON: {error}"),
            ))
        }
    };
    let (_kind, params, body_key) = match parse_submission(&body) {
        Ok(parsed) => parsed,
        Err((code, message)) => return Err(api_error(StatusCode::BAD_REQUEST, &code, message)),
    };
    let idempotency_key = header_key.or(body_key);
    let ops = service.operations();
    // Capability check before queuing — fail fast instead of a queued op
    // that can never run.
    let needs = match &params {
        Params::Run(_) | Params::Test(_) => service.adapter().is_none().then_some("adapter"),
        Params::Resume(_) | Params::RetryFailed(_) => {
            if service.adapter().is_none() {
                Some("adapter")
            } else {
                service.state().is_none().then_some("state")
            }
        }
        Params::Promote(_) => service.nessie().is_none().then_some("nessie"),
        Params::Reload(_) => None,
    };
    if let Some(what) = needs {
        return Err(missing(what));
    }
    let record = match ops
        .submit(&params, idempotency_key.as_deref())
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.code, error.message))?
    {
        SubmitOutcome::New(record) => record,
        SubmitOutcome::Replayed(record) => {
            return Ok(Json(json!({ "operation": record, "replayed": true })))
        }
        SubmitOutcome::Conflict(existing) => {
            return Err(api_error(
                StatusCode::CONFLICT,
                "API016",
                format!(
                    "idempotency key already used for a different `{}` request — it owns operation `{}`",
                    existing.kind, existing.id
                ),
            ))
        }
        SubmitOutcome::Busy => {
            return Err(api_error(
                StatusCode::CONFLICT,
                "API008",
                "another mutating operation is already running".to_string(),
            ))
        }
    };
    let spawned_service = service.clone();
    let spawned_ops = ops.clone();
    let op_id = record.id.clone();
    let op_kind = record.kind.clone();
    tokio::spawn(async move {
        spawned_ops.mark_running(&op_id);
        let outcome =
            execute::execute(spawned_service, spawned_ops.clone(), op_id.clone(), params).await;
        spawned_ops.finish(&op_id, outcome);
        spawned_ops.release(&op_kind);
    });
    Ok(Json(json!({ "operation": record, "replayed": false })))
}

/// `GET /v1/operations` — every known operation, oldest first.
async fn operations_list(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    Ok(Json(json!({ "operations": service.operations().list() })))
}

/// `GET /v1/operations/{id}` — the operation record. While a `run` op is
/// running, `progress` carries the live per-model execution state read back
/// from the state store.
async fn operations_get(
    State(service): State<Arc<WorkspaceService>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult {
    let Some(record) = service.operations().get(&id) else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "API009",
            format!("no such operation `{id}`"),
        ));
    };
    let mut body = json!({ "operation": record });
    if record.status == OperationStatus::Running && record.kind == "run" {
        if let Some(progress) = run_progress(&service, &record.params) {
            body["progress"] = progress;
        }
    }
    Ok(Json(body))
}

/// Live progress of a running `run` op: the runner persists its run record
/// and per-model transitions up front, so state is the progress source.
fn run_progress(service: &Arc<WorkspaceService>, params: &Value) -> Option<Value> {
    let state = service.state()?;
    let environment = params
        .get("environment")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| service.default_environment().map(str::to_string));
    let run = state.latest_run(environment.as_deref()).ok()??;
    if run.status != ExecutionStatus::Running {
        return None;
    }
    let models = state.model_runs(&run.run_id).ok()?;
    let finished = models
        .iter()
        .filter(|model| model.status.is_terminal())
        .count();
    Some(json!({
        "run_id": run.run_id,
        "planned": run.model_count,
        "finished": finished,
        "models": models,
    }))
}

/// `POST /v1/operations/{id}/cancel` — cooperative cancellation. The engine
/// observes the flag between model builds/test executions.
async fn operations_cancel(
    State(service): State<Arc<WorkspaceService>>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult {
    if !service.operations().cancel(&id) {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "API009",
            format!("no running operation `{id}`"),
        ));
    }
    Ok(Json(json!({ "cancelled": true })))
}

/// `POST /v1/reload` — synchronous recompile, equivalent to the
/// `{kind: "reload"}` operation but blocking until the snapshot is swapped.
async fn reload_handler(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let service2 = service.clone();
    tokio::task::spawn_blocking(move || service2.reload())
        .await
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API011",
                error.to_string(),
            )
        })?;
    Ok(Json(json!({
        "reloaded": true,
        "last_update": service.last_update(),
    })))
}
