//! Daemon API integration tests.

use std::path::PathBuf;

use phlo_transform_daemon::{router, spawn_watcher, ServiceConfig, WorkspaceService};
use phlo_transform_duckdb::DuckDbAdapter;
use phlo_transform_engine::SqliteStateStore;
use std::sync::Arc;
use std::time::Duration;

fn fixture() -> PathBuf {
    PathBuf::from("../../fixtures/basic-multi-root")
}

async fn start(root: PathBuf) -> String {
    let service = WorkspaceService::load(&root);
    let app = router(service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{address}")
}

async fn get_json(client: &reqwest::Client, url: &str) -> serde_json::Value {
    client
        .get(url)
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json")
}

#[tokio::test]
async fn exposes_semantic_api() {
    let base = start(fixture()).await;
    let client = reqwest::Client::new();

    let status = get_json(&client, &format!("{base}/status")).await;
    assert_eq!(status["models"], 3);
    assert_eq!(status["sources"], 1);

    let models = get_json(&client, &format!("{base}/v1/models")).await;
    assert_eq!(models["models"].as_array().unwrap().len(), 3);

    let inspect = get_json(&client, &format!("{base}/v1/models/assay.results")).await;
    assert_eq!(inspect["model"]["name"], "assay.results");
    assert_eq!(inspect["model"]["depends_on"][0], "assay.raw");

    let lineage = get_json(&client, &format!("{base}/v1/lineage/assay.results")).await;
    assert_eq!(lineage["upstream"][0], "assay.raw");

    let impact = get_json(&client, &format!("{base}/v1/impact/assay.raw.value")).await;
    assert_eq!(impact["column"], "assay.raw.value");

    let graph = get_json(&client, &format!("{base}/v1/graph")).await;
    assert!(!graph["nodes"].as_array().unwrap().is_empty());

    let check = get_json(&client, &format!("{base}/v1/check")).await;
    assert_eq!(check["ok"], true);
}

#[tokio::test]
async fn reload_picks_up_file_edits() {
    let directory = tempfile::tempdir().expect("temp dir");
    let transforms = directory.path().join("transforms").join("shared");
    std::fs::create_dir_all(&transforms).expect("dirs");
    std::fs::write(transforms.join("sites.sql"), "select * from external.sites").expect("write");

    let service = WorkspaceService::load(directory.path());
    assert_eq!(service.snapshot().models.len(), 1);

    std::fs::write(
        transforms.join("regions.sql"),
        "select * from external.regions",
    )
    .expect("write");
    service.reload();
    assert_eq!(service.snapshot().models.len(), 2);
}

#[tokio::test]
async fn watcher_reloads_after_file_edit_without_restart() {
    let directory = tempfile::tempdir().expect("temp dir");
    let transforms = directory.path().join("transforms").join("shared");
    std::fs::create_dir_all(&transforms).expect("dirs");
    std::fs::write(transforms.join("sites.sql"), "select * from external.sites").expect("write");

    let service = WorkspaceService::load(directory.path());
    assert_eq!(service.snapshot().models.len(), 1);
    let _watcher = spawn_watcher(service.clone(), Duration::from_millis(50));

    // Allow the watcher to capture the initial fingerprint, then edit.
    tokio::time::sleep(Duration::from_millis(150)).await;
    std::fs::write(
        transforms.join("regions.sql"),
        "select * from external.regions",
    )
    .expect("write");

    for _ in 0..50 {
        if service.snapshot().models.len() == 2 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("watcher did not reload the workspace");
}

// ---------------------------------------------------------------------------
// Bundle 9: machine-facing API — operations, reads, structured errors
// ---------------------------------------------------------------------------

/// A workspace that materialises on DuckDB: one trivial model, sqlite state.
fn duckdb_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("phlo.toml"),
        "[transform]\ndefault_materialization = \"view\"\n\
         default_catalog = \"memory\"\ndefault_schema = \"analytics\"\n",
    )
    .expect("phlo.toml");
    let transforms = dir.path().join("transforms").join("shared");
    std::fs::create_dir_all(&transforms).expect("dirs");
    std::fs::write(transforms.join("sites.sql"), "select 1 as value").expect("sql");
    dir
}

async fn start_with_config(
    root: PathBuf,
    config: ServiceConfig,
) -> (String, Arc<WorkspaceService>) {
    let service = WorkspaceService::load_with_config(&root, config);
    let app = router(service.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{address}"), service)
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let response = client.post(url).json(body).send().await.expect("request");
    let status = response.status().as_u16();
    (status, response.json().await.expect("json"))
}

/// Poll an operation until it reaches a terminal state.
async fn wait_operation(client: &reqwest::Client, base: &str, id: &str) -> serde_json::Value {
    for _ in 0..200 {
        let body = get_json(client, &format!("{base}/v1/operations/{id}")).await;
        let status = body["operation"]["status"].as_str().unwrap_or("");
        if matches!(status, "succeeded" | "failed" | "cancelled") {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("operation {id} did not finish");
}

#[tokio::test]
async fn run_operation_lifecycle_and_state() {
    let dir = duckdb_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            state: Some(Arc::new(
                SqliteStateStore::open(&state_path).expect("state"),
            )),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    // Submit a run with an idempotency key.
    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run", "idempotency_key": "k1"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["replayed"], false);

    // Same key replays the existing handle — no duplicate execution.
    let (_, replay) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run", "idempotency_key": "k1"}),
    )
    .await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["operation"]["id"], id);

    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    assert_eq!(done["operation"]["result"]["status"], "passed", "{done}");

    // The run persisted state the read endpoints now serve.
    let runs = get_json(&client, &format!("{base}/v1/state/runs")).await;
    assert_eq!(runs.as_array().unwrap().len(), 1);
    let run_id = runs[0]["run_id"].as_str().unwrap();

    let shown = get_json(&client, &format!("{base}/v1/state/runs/{run_id}")).await;
    assert_eq!(shown["run"]["run_id"], run_id);
    assert!(!shown["models"].as_array().unwrap().is_empty());

    let record = get_json(&client, &format!("{base}/v1/state/models/shared.sites")).await;
    assert_eq!(record["model_id"], "shared.sites");

    // The list endpoint exposes the finished op.
    let ops = get_json(&client, &format!("{base}/v1/operations")).await;
    assert!(ops["operations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|op| op["id"] == id));
}

#[tokio::test]
async fn test_operation_and_plan_read() {
    let dir = duckdb_workspace();
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    // GET /v1/plan — plan DTO parity with `phlo-transform plan --json`.
    let plan = get_json(&client, &format!("{base}/v1/plan")).await;
    assert!(
        plan["models"].as_array().unwrap().iter().any(|m| {
            m["id"]
                .as_str()
                .map(|id| id.contains("sites"))
                .unwrap_or(false)
        }),
        "{plan}"
    );

    // A real selector narrows the plan to the selected model (+ deps).
    let plan2 = get_json(&client, &format!("{base}/v1/plan?select=shared.sites")).await;
    assert_eq!(plan2["models"].as_array().unwrap().len(), 1);

    // A garbage selector is a structured 400.
    let response = client
        .get(format!("{base}/v1/plan?select=[[["))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 400);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API006");

    // test op with no tests in the workspace succeeds with an empty report.
    let (_, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "test"}),
    )
    .await;
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    assert_eq!(
        done["operation"]["result"]["tests"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn operation_rejections_and_structured_errors() {
    // No engine handles at all: reads work, ops answer API007.
    let (base, _service) = start_with_config(fixture(), ServiceConfig::default()).await;
    let client = reqwest::Client::new();

    for kind in ["run", "test"] {
        let response = client
            .post(format!("{base}/v1/operations"))
            .json(&serde_json::json!({"kind": kind}))
            .send()
            .await
            .expect("request");
        assert_eq!(response.status().as_u16(), 503, "{kind}");
        let body: serde_json::Value = response.json().await.expect("json");
        assert_eq!(body["error"]["code"], "API007");
    }
    let (_, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "promote", "params": {"candidate": "a", "to": "main"}}),
    )
    .await;
    assert_eq!(body["error"]["code"], "API007");

    // Unknown kind and malformed params → API012.
    let response = client
        .post(format!("{base}/v1/operations"))
        .json(&serde_json::json!({"kind": "explode"}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 400);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API012");

    let (_, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "promote", "params": {"candidate": "a"}}),
    )
    .await;
    assert_eq!(body["error"]["code"], "API012");

    // Unknown operation id → API009.
    let response = client
        .get(format!("{base}/v1/operations/deadbeef"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 404);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API009");

    // Missing capability on reads → API007.
    let response = client
        .get(format!("{base}/v1/plan"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 503);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API007");

    let response = client
        .get(format!("{base}/v1/state/runs"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 503);

    // reload works without any engine handle.
    let (_, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "reload"}),
    )
    .await;
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");

    // POST /v1/reload — synchronous variant.
    let (_, body) = post_json(
        &client,
        &format!("{base}/v1/reload"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(body["reloaded"], true);
}

#[tokio::test]
async fn mutating_operations_are_serialized() {
    let dir = duckdb_workspace();
    let (base, service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    // Hold the mutating gate with a queued (never-executed) op.
    let held = match service.operations().submit(
        &phlo_transform_daemon::operations::Params::Run(
            phlo_transform_daemon::operations::RunParams {
                selectors: vec![],
                environment: None,
                force: false,
                run_tests: None,
            },
        ),
        None,
    ) {
        phlo_transform_daemon::operations::SubmitOutcome::New(record) => record,
        _ => panic!("submit should have queued"),
    };

    let response = client
        .post(format!("{base}/v1/operations"))
        .json(&serde_json::json!({"kind": "run"}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 409);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API008");

    // Cancelling the held op is accepted; a non-mutating op is not gated.
    let (_, body) = post_json(
        &client,
        &format!("{base}/v1/operations/{}/cancel", held.id),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(body["cancelled"], true);
    let (_, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "reload"}),
    )
    .await;
    assert!(body["operation"]["id"].is_string(), "{body}");

    // Releasing the gate lets the next run through.
    service.operations().release("run");
    let (_, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run"}),
    )
    .await;
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
}

#[tokio::test]
async fn lineage_document_and_diff_endpoints() {
    // A git workspace: lineage --diff HEAD is a self-comparison (empty diff).
    let dir = tempfile::tempdir().expect("tempdir");
    let transforms = dir.path().join("transforms").join("shared");
    std::fs::create_dir_all(&transforms).expect("dirs");
    std::fs::write(transforms.join("sites.sql"), "select * from external.sites").expect("write");
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git")
    };
    assert!(run(&["init"]).status.success());
    assert!(run(&["add", "-A"]).status.success());
    assert!(run(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-m",
        "init"
    ])
    .status
    .success());

    let (base, _service) =
        start_with_config(dir.path().to_path_buf(), ServiceConfig::default()).await;
    let client = reqwest::Client::new();

    let doc = get_json(&client, &format!("{base}/v1/lineage")).await;
    assert!(
        doc["nodes"]
            .as_array()
            .map(|n| !n.is_empty())
            .unwrap_or(false),
        "{doc}"
    );

    let diff = get_json(&client, &format!("{base}/v1/diff/lineage?base=HEAD")).await;
    assert!(diff.get("diff").is_some(), "{diff}");
    // Self-diff is empty — empty vectors are omitted by skip_serializing_if.
    assert!(diff["diff"]["nodes_added"].is_null(), "{diff}");
    assert!(diff["diff"]["edges_added"].is_null(), "{diff}");

    // A bogus ref is a structured error, not a panic.
    let response = client
        .get(format!("{base}/v1/diff/lineage?base=no-such-ref"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 400);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API010");
}
