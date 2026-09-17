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
    assert_eq!(
        status["version"],
        env!("CARGO_PKG_VERSION"),
        "status reports the daemon's release version"
    );

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
    let held = match service
        .operations()
        .submit(
            &phlo_transform_daemon::operations::Params::Run(
                phlo_transform_daemon::operations::RunParams {
                    selectors: vec![],
                    environment: None,
                    base: None,
                    force: false,
                    run_tests: None,
                    retries: None,
                },
            ),
            None,
        )
        .expect("submit")
    {
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

#[tokio::test]
async fn promote_operation_runs_the_full_gated_merge() {
    use phlo_transform_engine::{
        branch_diff, write_environment_artifacts, ArtifactWriter, BranchDiffRequest, CatalogStatus,
        EnvironmentSetup,
    };
    use phlo_transform_nessie::{InMemoryNessie, NessieClient, ReferenceInfo};

    let dir = duckdb_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let nessie = Arc::new(InMemoryNessie::new());
    nessie.seed("main", "aaaa").seed("dev", "bbbb");
    let (base, service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            state: Some(Arc::new(
                SqliteStateStore::open(&state_path).expect("state"),
            )),
            nessie: Some(nessie.clone()),
            // A catalog-facing Nessie URI is configured, so the run op
            // genuinely provisions the dev branch and recompiles retargeted;
            // the `--catalog` override pins the candidate catalog back to
            // `memory` because DuckDB cannot host a second catalog — the
            // branch is still real on the Nessie side and the run binds to
            // its head.
            nessie_uri: Some("http://nessie.invalid".to_string()),
            catalog: Some("memory".to_string()),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    // Recorded provisioning evidence for the dev -> main pair: the dev
    // branch pre-exists in Nessie (seeded), so `ensure_candidate` preserves
    // this cut-from provenance rather than guessing it.
    write_environment_artifacts(
        dir.path(),
        None,
        &EnvironmentSetup {
            base: ReferenceInfo::branch("main", "aaaa"),
            candidate: ReferenceInfo::branch("dev", "bbbb"),
            // dev@bbbb was provably cut from main@aaaa — immutable provenance
            // the `base` gate evaluates the target against.
            created_from: Some(ReferenceInfo::branch("main", "aaaa")),
            created_branch: false,
            catalog: "memory".to_string(),
            // DuckDB cannot provision catalogs — the `memory` pin is an
            // unmanaged binding, recorded as such.
            catalog_status: CatalogStatus::Unmanaged,
            catalog_owned_by_phlo: Some(false),
        },
    )
    .expect("environment artifacts");

    // Base-side materialisation (default env folds into `main`), then the
    // candidate-side run under env label `dev`.
    for environment in [None, Some("dev")] {
        let (status, body) = post_json(
            &client,
            &format!("{base}/v1/operations"),
            &serde_json::json!({"kind": "run", "params": {"environment": environment}}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let id = body["operation"]["id"].as_str().unwrap().to_string();
        let done = wait_operation(&client, &base, &id).await;
        assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    }

    // The audited evidence: a real deep branch diff over the recorded state.
    // The DuckDB adapter's diff path drives a private runtime, so run it on
    // a blocking thread with its own current-thread runtime rather than the
    // test's.
    let diff_service = service.clone();
    let report = tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(branch_diff(
                diff_service.adapter().expect("adapter"),
                diff_service.state().as_deref(),
                &diff_service.snapshot(),
                &BranchDiffRequest {
                    candidate_ref: "dev".to_string(),
                    base_ref: "main".to_string(),
                    candidate_catalog: None,
                    base_catalog: None,
                    // Commit-bound audit: the artifact names the exact heads
                    // promotion will verify against.
                    candidate_hash: Some("bbbb".to_string()),
                    base_hash: Some("aaaa".to_string()),
                    deep: true,
                    default_schema: None,
                },
            ))
    })
    .await
    .expect("branch diff task")
    .expect("branch diff");
    ArtifactWriter::for_workspace(dir.path())
        .write_branch_diff(&report)
        .expect("write diff");

    // `--check`: gates evaluate, nothing merges.
    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({
            "kind": "promote",
            "params": {"candidate": "dev", "to": "main", "require_diff": true, "check": true}
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    assert_eq!(done["operation"]["result"]["ok"], true, "{done}");
    assert_eq!(done["operation"]["result"]["check_only"], true, "{done}");
    assert_eq!(
        nessie
            .get_reference("main")
            .await
            .expect("ref")
            .expect("main")
            .hash,
        "aaaa",
        "check-only promote must not merge"
    );

    // The real promote: gates pass, the merge lands, the record persists.
    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({
            "kind": "promote",
            "params": {"candidate": "dev", "to": "main", "require_diff": true}
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    assert_eq!(
        done["operation"]["result"]["promotion"]["merged"], true,
        "{done}"
    );
    assert_eq!(
        nessie
            .get_reference("main")
            .await
            .expect("ref")
            .expect("main")
            .hash,
        "bbbb"
    );
    let promotions = get_json(&client, &format!("{base}/v1/state/promotions")).await;
    assert_eq!(promotions.as_array().unwrap().len(), 1, "{promotions}");

    // A candidate with no run and no audit evidence fails the gates — the op
    // completes (the verdict was delivered) with ok:false and a failed gate.
    nessie.seed("ghost", "cccc");
    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({
            "kind": "promote",
            "params": {"candidate": "ghost", "to": "main"}
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    assert_eq!(done["operation"]["result"]["ok"], false, "{done}");
    assert!(done["operation"]["result"]["gates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|gate| gate["name"] == "run" && gate["passed"] == false));
}

/// A daemon that knows Nessie but cannot provision a branch-scoped catalog
/// (no catalog-facing `nessie_uri`) must refuse a non-base environment run
/// outright — running it against the default target and then binding the
/// evidence to the candidate's head would record work the branch never saw.
#[tokio::test]
async fn run_against_an_unprovisionable_nessie_environment_fails_closed() {
    use phlo_transform_nessie::InMemoryNessie;

    let dir = duckdb_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let nessie = Arc::new(InMemoryNessie::new());
    nessie.seed("main", "aaaa").seed("dev", "bbbb");
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            state: Some(Arc::new(
                SqliteStateStore::open(&state_path).expect("state"),
            )),
            nessie: Some(nessie),
            nessie_uri: None,
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run", "params": {"environment": "dev"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "failed", "{done}");
    assert_eq!(done["operation"]["error"]["code"], "API007", "{done}");

    // Nothing executed and nothing was recorded: no run exists for `dev`
    // to be bound to.
    let runs = get_json(&client, &format!("{base}/v1/state/runs")).await;
    assert_eq!(runs.as_array().unwrap().len(), 0, "{runs}");

    // The same run without a Nessie client is the honest label-only mode.
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
    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run", "params": {"environment": "dev"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
}

/// A daemon scoped to an environment resolves `test` against it like
/// `run` does — an operation without `params.environment` inherits the
/// configured default, and an environment whose catalog cannot be
/// provisioned fails closed instead of testing the default catalog's data.
#[tokio::test]
async fn test_operation_inherits_the_daemons_default_environment() {
    use phlo_transform_nessie::InMemoryNessie;

    let dir = duckdb_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let nessie = Arc::new(InMemoryNessie::new());
    nessie.seed("main", "aaaa").seed("dev", "bbbb");
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            state: Some(Arc::new(
                SqliteStateStore::open(&state_path).expect("state"),
            )),
            nessie: Some(nessie),
            nessie_uri: None,
            environment: Some("dev".to_string()),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "test", "params": {}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "failed", "{done}");
    assert_eq!(done["operation"]["error"]["code"], "API007", "{done}");
}

/// `GET /v1/plan?environment=X` resolves the same physical catalog a run
/// against X would — the machine API's plan→run contract is exact. The
/// preview is read-only: no branch is created, no evidence written.
#[tokio::test]
async fn plan_and_run_against_an_environment_target_the_same_catalog() {
    use phlo_transform_engine::{catalog_name, read_environment_for};
    use phlo_transform_nessie::{InMemoryNessie, NessieClient};

    let dir = duckdb_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let nessie = Arc::new(InMemoryNessie::new());
    nessie.seed("main", "aaaa");
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            state: Some(Arc::new(
                SqliteStateStore::open(&state_path).expect("state"),
            )),
            nessie: Some(nessie.clone()),
            nessie_uri: Some("http://nessie.invalid".to_string()),
            // DuckDB cannot host a per-ref catalog — pin the retarget to
            // the existing `memory` catalog. The resolution path is the
            // same one Trino takes with the generated name.
            catalog: Some("memory".to_string()),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    // The preview resolves through `resolve(ReadOnly)` — the same catalog
    // the run's `resolve(Ensure)` computes — and provisions nothing.
    let plan = get_json(&client, &format!("{base}/v1/plan?environment=dev")).await;
    let plan_target = plan["models"]
        .as_array()
        .and_then(|models| {
            models
                .iter()
                .find(|model| model["id"].as_str() == Some("shared.sites"))
        })
        .and_then(|model| model["target"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("shared.sites missing from plan: {plan}"));
    assert_eq!(plan_target, "memory.analytics.shared__sites", "{plan}");
    assert!(
        nessie.get_reference("dev").await.expect("refs").is_none(),
        "a read-only plan must not create the branch"
    );
    assert!(
        read_environment_for(dir.path(), None, "dev")
            .expect("read")
            .is_none(),
        "a read-only plan must not write provisioning evidence"
    );

    // The run targets the same physical catalog the plan previewed.
    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run", "params": {"environment": "dev"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    let run_target = done["operation"]["result"]["models"]
        .as_array()
        .and_then(|models| {
            models
                .iter()
                .find(|model| model["model"].as_str() == Some("shared.sites"))
        })
        .and_then(|model| model["target"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("shared.sites missing from run: {done}"));
    assert_eq!(run_target, plan_target, "{done}");

    // Without the override the environment resolves its own generated
    // name — and DuckDB cannot provision it, so plan and run now fail
    // closed the same way (covered by
    // `plan_fails_closed_when_the_adapter_cannot_provision` below).
    let generated = catalog_name("dev");
    assert!(generated.starts_with("phlo_dev_"));
}

/// A `plan(environment)` whose generated catalog needs provisioning the
/// adapter cannot perform fails closed — the same API007 the run gets,
/// not a preview of a target no run could reach. An explicit `--catalog`
/// pin stays the escape hatch (asserted by
/// `plan_and_run_against_an_environment_target_the_same_catalog` above).
#[tokio::test]
async fn plan_fails_closed_when_the_adapter_cannot_provision() {
    use phlo_transform_nessie::InMemoryNessie;

    let dir = duckdb_workspace();
    let nessie = Arc::new(InMemoryNessie::new());
    nessie.seed("main", "aaaa").seed("dev", "bbbb");
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            nessie: Some(nessie),
            nessie_uri: Some("http://nessie.invalid".to_string()),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/v1/plan?environment=dev"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 503);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API007", "{body}");
}

/// A `plan(environment)` on a Nessie-configured service that cannot
/// provision fails closed — the same API007 a run would get.
#[tokio::test]
async fn plan_against_an_unprovisionable_nessie_environment_fails_closed() {
    use phlo_transform_nessie::InMemoryNessie;

    let dir = duckdb_workspace();
    let nessie = Arc::new(InMemoryNessie::new());
    nessie.seed("main", "aaaa").seed("dev", "bbbb");
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
            nessie: Some(nessie),
            nessie_uri: None,
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/v1/plan?environment=dev"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 503);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API007", "{body}");

    // No environment and the base env itself still plan fine.
    let plan = get_json(&client, &format!("{base}/v1/plan?environment=main")).await;
    assert!(plan["models"].as_array().is_some(), "{plan}");
}

#[tokio::test]
async fn operations_survive_a_daemon_restart() {
    let dir = duckdb_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let config = || ServiceConfig {
        adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
        state: Some(Arc::new(
            SqliteStateStore::open(&state_path).expect("state"),
        )),
        ..ServiceConfig::default()
    };
    let (base, _service) = start_with_config(dir.path().to_path_buf(), config()).await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run", "idempotency_key": "across-restart"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &id).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");

    // A fresh service on the same root: the journal folds back in.
    let (base, _service) = start_with_config(dir.path().to_path_buf(), config()).await;
    let restored = get_json(&client, &format!("{base}/v1/operations/{id}")).await;
    assert_eq!(restored["operation"]["status"], "succeeded", "{restored}");
    assert_eq!(
        restored["operation"]["result"]["status"], "passed",
        "{restored}"
    );

    // A keyed retry after the restart replays the original handle.
    let (_, replay) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run", "idempotency_key": "across-restart"}),
    )
    .await;
    assert_eq!(replay["replayed"], true, "{replay}");
    assert_eq!(replay["operation"]["id"], id);
    let ops = get_json(&client, &format!("{base}/v1/operations")).await;
    assert_eq!(ops["operations"].as_array().unwrap().len(), 1, "{ops}");
}

#[tokio::test]
async fn bearer_token_gates_everything_but_status() {
    let dir = duckdb_workspace();
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            token: Some("s3cret".to_string()),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    // Liveness stays open; everything else requires the token.
    let status = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("request");
    assert_eq!(status.status().as_u16(), 200);

    for method in ["GET", "POST"] {
        let request = match method {
            "GET" => client.get(format!("{base}/v1/models")),
            _ => client
                .post(format!("{base}/v1/operations"))
                .json(&serde_json::json!({"kind": "reload"})),
        };
        let denied = request.try_clone().unwrap().send().await.expect("request");
        assert_eq!(denied.status().as_u16(), 401);
        let body: serde_json::Value = denied.json().await.expect("json");
        assert_eq!(body["error"]["code"], "API015");

        let wrong = request
            .try_clone()
            .unwrap()
            .bearer_auth("wrong")
            .send()
            .await
            .expect("request");
        assert_eq!(wrong.status().as_u16(), 401);

        let ok = request.bearer_auth("s3cret").send().await.expect("request");
        assert_eq!(ok.status().as_u16(), 200);
    }
}

// ---------------------------------------------------------------------------
// Continuation operations, failed-model reads, request-bound idempotency
// ---------------------------------------------------------------------------

/// A workspace whose run fails at execution: `main.broken` reads a source
/// the warehouse does not have, `main.child` blocks behind it, `main.ok`
/// is healthy — the same shape the CLI resilience tests use.
fn resilience_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let transforms = dir.path().join("workflows/main/transforms");
    std::fs::create_dir_all(&transforms).unwrap();
    std::fs::write(transforms.join("ok.sql"), "select 1 as id\n").unwrap();
    std::fs::write(
        transforms.join("broken.sql"),
        "select * from external.missing_table\n",
    )
    .unwrap();
    std::fs::write(transforms.join("child.sql"), "select * from main.broken\n").unwrap();
    dir
}

/// Submit an operation and wait for its terminal record.
async fn submit_and_wait(
    client: &reqwest::Client,
    base: &str,
    body: &serde_json::Value,
) -> serde_json::Value {
    let (status, submitted) = post_json(client, &format!("{base}/v1/operations"), body).await;
    assert_eq!(status, 200, "{submitted}");
    let id = submitted["operation"]["id"].as_str().unwrap().to_string();
    wait_operation(client, base, &id).await
}

#[tokio::test]
async fn failed_models_endpoint_and_retry_failed_operation() {
    use phlo_transform_engine::Adapter;

    let dir = resilience_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let adapter = Arc::new(DuckDbAdapter::in_memory().expect("duckdb"));
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(adapter.clone()),
            state: Some(Arc::new(
                SqliteStateStore::open(&state_path).expect("state"),
            )),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    // The run op itself succeeds (the verdict was delivered); the run fails.
    let done = submit_and_wait(&client, &base, &serde_json::json!({"kind": "run"})).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    assert_eq!(done["operation"]["result"]["status"], "failed", "{done}");
    let run_id = done["operation"]["result"]["run_id"].as_str().unwrap();

    // The failed-models read names the failed and the blocked work — what a
    // continuation would pick up.
    let failed = get_json(&client, &format!("{base}/v1/state/runs/{run_id}/failed")).await;
    let model_ids: Vec<&str> = failed["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["model_id"].as_str().unwrap())
        .collect();
    assert!(model_ids.contains(&"main.broken"), "{failed}");
    assert!(model_ids.contains(&"main.child"), "{failed}");
    assert!(!model_ids.contains(&"main.ok"), "{failed}");
    // A run-id prefix resolves like the CLI.
    let prefix = get_json(
        &client,
        &format!("{base}/v1/state/runs/{}/failed", &run_id[..8]),
    )
    .await;
    assert_eq!(prefix["run_id"], run_id);

    // `resume` on a finished run is refused — history stays immutable.
    let resumed = submit_and_wait(
        &client,
        &base,
        &serde_json::json!({"kind": "resume", "params": {"run": run_id}}),
    )
    .await;
    assert_eq!(resumed["operation"]["status"], "failed", "{resumed}");
    assert!(
        resumed["operation"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("retry-failed"),
        "{resumed}"
    );

    // `retry_failed` starts a NEW run over the failed portion — still
    // failing, because the source is still missing.
    let retried = submit_and_wait(
        &client,
        &base,
        &serde_json::json!({"kind": "retry_failed", "params": {"run": run_id}}),
    )
    .await;
    assert_eq!(retried["operation"]["status"], "succeeded", "{retried}");
    assert_eq!(
        retried["operation"]["result"]["status"], "failed",
        "{retried}"
    );
    let retry_id = retried["operation"]["result"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(retry_id, run_id);
    assert_eq!(
        retried["operation"]["result"]["continued_from"], run_id,
        "{retried}"
    );
    // The healthy model was not retried.
    let retried_models: Vec<&str> = retried["operation"]["result"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["model"].as_str().unwrap())
        .collect();
    assert!(!retried_models.contains(&"main.ok"), "{retried}");

    // Heal the warehouse: the missing source now exists. Retrying the retry
    // run fixes the remainder.
    adapter
        .execute("create schema if not exists external")
        .await
        .expect("schema");
    adapter
        .execute("create or replace table external.missing_table as select 1 as id, 'x' as label")
        .await
        .expect("heal source");
    let healed = submit_and_wait(
        &client,
        &base,
        &serde_json::json!({"kind": "retry_failed", "params": {"run": retry_id}}),
    )
    .await;
    assert_eq!(healed["operation"]["status"], "succeeded", "{healed}");
    assert_eq!(
        healed["operation"]["result"]["status"], "passed",
        "{healed}"
    );

    // An unknown run id is a structured operation failure, not a 500.
    let unknown = submit_and_wait(
        &client,
        &base,
        &serde_json::json!({"kind": "resume", "params": {"run": "no-such-run"}}),
    )
    .await;
    assert_eq!(unknown["operation"]["status"], "failed", "{unknown}");
    assert!(
        unknown["operation"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no run matches"),
        "{unknown}"
    );
}

#[tokio::test]
async fn resume_continues_an_interrupted_run() {
    use phlo_transform_engine::{Adapter, ExecutionStatus, RunRecord, StateStore};

    let dir = resilience_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let adapter = Arc::new(DuckDbAdapter::in_memory().expect("duckdb"));
    let state = Arc::new(SqliteStateStore::open(&state_path).expect("state"));
    let (base, _service) = start_with_config(
        dir.path().to_path_buf(),
        ServiceConfig {
            adapter: Some(adapter.clone()),
            state: Some(state.clone()),
            ..ServiceConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    // A real failed run supplies the stored plan resume reconstructs from.
    let done = submit_and_wait(&client, &base, &serde_json::json!({"kind": "run"})).await;
    let run_id = done["operation"]["result"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let stored = state
        .run(&run_id)
        .expect("stored run")
        .expect("run recorded");
    let plan = stored.plan.expect("resumable plan persisted");

    // Fabricate the interrupted run: a daemon that died mid-flight — run
    // record still `running`, `main.ok` already passed at its desired
    // version, `main.broken` in flight, `main.child` never reached.
    let interrupted = "interrupted-run-for-resume";
    state
        .start_run(
            &RunRecord {
                run_id: interrupted.to_string(),
                plan_id: plan.plan_id.clone(),
                environment: stored.record.environment.clone(),
                reference_hash: None,
                started_at: phlo_transform_engine::util::now_rfc3339(),
                finished_at: None,
                status: ExecutionStatus::Running,
                model_count: 3,
                failed_count: 0,
            },
            &plan,
        )
        .expect("start interrupted run");
    let ok_record = state
        .model_runs(&run_id)
        .expect("model runs")
        .into_iter()
        .find(|record| record.model_id == "main.ok")
        .expect("main.ok ran");
    let mut reused = ok_record.clone();
    reused.run_id = interrupted.to_string();
    state.record_model(&reused).expect("record main.ok");

    // Heal the warehouse, then resume — the continuation keeps the original
    // run id, reuses `main.ok`, and finishes the rest.
    adapter
        .execute("create schema if not exists external")
        .await
        .expect("schema");
    adapter
        .execute("create or replace table external.missing_table as select 1 as id, 'x' as label")
        .await
        .expect("heal source");
    let resumed = submit_and_wait(
        &client,
        &base,
        &serde_json::json!({"kind": "resume", "params": {"run": interrupted}}),
    )
    .await;
    assert_eq!(resumed["operation"]["status"], "succeeded", "{resumed}");
    let result = &resumed["operation"]["result"];
    assert_eq!(result["status"], "passed", "{resumed}");
    assert_eq!(result["run_id"], interrupted, "resume keeps the run id");
    let ok = result["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["model"] == "main.ok")
        .expect("main.ok in result");
    assert_eq!(
        ok["status"], "cached",
        "a passed model is reused, not rerun"
    );

    // Resuming the now-passed run is refused.
    let again = submit_and_wait(
        &client,
        &base,
        &serde_json::json!({"kind": "resume", "params": {"run": interrupted}}),
    )
    .await;
    assert_eq!(again["operation"]["status"], "failed", "{again}");
}

#[tokio::test]
async fn idempotent_replay_requires_the_same_request() {
    let dir = duckdb_workspace();
    let state_path = dir.path().join(".phlo").join("transform").join("state.db");
    std::fs::create_dir_all(state_path.parent().unwrap()).expect("state dir");
    let config = || ServiceConfig {
        adapter: Some(Arc::new(DuckDbAdapter::in_memory().expect("duckdb"))),
        state: Some(Arc::new(
            SqliteStateStore::open(&state_path).expect("state"),
        )),
        ..ServiceConfig::default()
    };
    let (base, _service) = start_with_config(dir.path().to_path_buf(), config()).await;
    let client = reqwest::Client::new();

    let dev = serde_json::json!({
        "kind": "run",
        "idempotency_key": "req-1",
        "params": {"environment": "dev"},
    });
    let (status, first) = post_json(&client, &format!("{base}/v1/operations"), &dev).await;
    assert_eq!(status, 200, "{first}");
    let op_id = first["operation"]["id"].as_str().unwrap().to_string();
    wait_operation(&client, &base, &op_id).await;

    // Same key + same request → the original handle replays.
    let (_, replay) = post_json(&client, &format!("{base}/v1/operations"), &dev).await;
    assert_eq!(replay["replayed"], true, "{replay}");
    assert_eq!(replay["operation"]["id"], op_id);

    // Same key + different request → 409 API016, not a silent replay.
    let (status, conflict) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({
            "kind": "run",
            "idempotency_key": "req-1",
            "params": {"environment": "prod"},
        }),
    )
    .await;
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(conflict["error"]["code"], "API016");

    // Keys are global to the endpoint: the same key under a different
    // kind is a conflict too, not a separate operation in another bucket.
    let (status, other) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "test", "idempotency_key": "req-1"}),
    )
    .await;
    assert_eq!(status, 409, "{other}");
    assert_eq!(other["error"]["code"], "API016");

    // After a restart the request hash still binds: the conflicting request
    // is rejected again, the matching one replays.
    let (base, _service) = start_with_config(dir.path().to_path_buf(), config()).await;
    let (status, _) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({
            "kind": "run",
            "idempotency_key": "req-1",
            "params": {"environment": "prod"},
        }),
    )
    .await;
    assert_eq!(status, 409, "the conflict survives a restart");
    let (_, replay) = post_json(&client, &format!("{base}/v1/operations"), &dev).await;
    assert_eq!(replay["replayed"], true, "{replay}");
    assert_eq!(replay["operation"]["id"], op_id);
}

#[tokio::test]
async fn impact_covers_models_columns_and_selections() {
    let (base, _service) = start_with_config(fixture(), ServiceConfig::default()).await;
    let client = reqwest::Client::new();

    // A model target reports downstream models and their tests.
    let impact = get_json(&client, &format!("{base}/v1/impact/assay.results")).await;
    assert_eq!(impact["model"], "assay.results", "{impact}");
    assert_eq!(
        impact["downstream_models"].as_array().unwrap(),
        &vec![serde_json::json!("reporting.monthly")],
        "{impact}"
    );

    // A column target keeps the column-level impact report.
    let column = get_json(&client, &format!("{base}/v1/impact/assay.raw.value")).await;
    assert_eq!(column["column"], "assay.raw.value", "{column}");

    // A selector reports the blast radius outside the selected set.
    let selection = get_json(&client, &format!("{base}/v1/impact?select=assay.results")).await;
    assert_eq!(
        selection["impacted_models"].as_array().unwrap(),
        &vec![serde_json::json!("reporting.monthly")],
        "{selection}"
    );

    // A bogus selector is a structured 400.
    let response = client
        .get(format!("{base}/v1/impact?select=[[["))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 400);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "API006");

    // No target and no selection is a structured 400.
    let response = client
        .get(format!("{base}/v1/impact"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 400);
}
