//! Local semantic service for Phlo Transform.
//!
//! The daemon wraps the same compiler and engine libraries as the CLI. It
//! holds an immutable compiled snapshot behind an `RwLock` so concurrent
//! readers never observe partially updated state, and recompiles on demand or
//! when watched files change.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use phlo_transform_core::{
    compile, load_project, ColumnRef, Compilation, ModelId, SemanticProject,
};
use phlo_transform_engine::util::now_rfc3339;

/// A live, coherent workspace snapshot.
pub struct WorkspaceService {
    root: PathBuf,
    snapshot: RwLock<Arc<Compilation>>,
    last_update: RwLock<String>,
}

impl WorkspaceService {
    /// Load a workspace root.
    pub fn load(root: &Path) -> Arc<Self> {
        let compilation = Arc::new(compile_root(root));
        Arc::new(Self {
            root: root.to_path_buf(),
            snapshot: RwLock::new(compilation),
            last_update: RwLock::new(now_rfc3339()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn snapshot(&self) -> Arc<Compilation> {
        self.snapshot.read().expect("snapshot lock").clone()
    }

    /// Recompile the workspace and publish a new snapshot atomically.
    pub fn reload(&self) {
        let compilation = Arc::new(compile_root(&self.root));
        *self.snapshot.write().expect("snapshot lock") = compilation;
        *self.last_update.write().expect("snapshot lock") = now_rfc3339();
    }

    fn last_update(&self) -> String {
        self.last_update.read().expect("snapshot lock").clone()
    }
}

fn compile_root(root: &Path) -> Compilation {
    match load_project(root) {
        Ok(project) => compile(&project),
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
        .route("/v1/lineage/{target}", get(lineage))
        .route("/v1/impact/{column}", get(impact))
        .route("/v1/graph", get(graph))
        .with_state(service)
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

fn error(status: StatusCode, code: &str, message: String) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
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
        return Err(error(
            StatusCode::BAD_REQUEST,
            "API001",
            format!("invalid model id `{id}`"),
        ));
    };
    match snapshot.inspect_report(&model_id) {
        Some(report) => Ok(Json(
            serde_json::to_value(report).expect("report serialises"),
        )),
        None => Err(error(
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
        return Err(error(
            StatusCode::BAD_REQUEST,
            "API003",
            format!("invalid lineage target `{target}`"),
        ));
    };
    let Ok(id) = ModelId::parse(model) else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "API003",
            format!("invalid model `{model}`"),
        ));
    };
    match snapshot.column_lineage_report(&id, column) {
        Some(report) => Ok(Json(
            serde_json::to_value(report).expect("report serialises"),
        )),
        None => Err(error(
            StatusCode::NOT_FOUND,
            "API004",
            format!("no column lineage for `{target}`"),
        )),
    }
}

async fn impact(
    State(service): State<Arc<WorkspaceService>>,
    AxumPath(column): AxumPath<String>,
) -> ApiResult {
    let snapshot = service.snapshot();
    let Some((model, name)) = column.rsplit_once('.') else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "API005",
            format!("invalid column `{column}`"),
        ));
    };
    let Ok(id) = ModelId::parse(model) else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "API005",
            format!("invalid model `{model}`"),
        ));
    };
    let target = ColumnRef::model(id, name);
    Ok(Json(
        serde_json::to_value(snapshot.impact_report(&target)).expect("report serialises"),
    ))
}

async fn graph(State(service): State<Arc<WorkspaceService>>) -> ApiResult {
    let snapshot = service.snapshot();
    Ok(Json(
        serde_json::to_value(snapshot.graph_artifact()).expect("report serialises"),
    ))
}
