//! Live Nessie + Trino daemon e2e: an environment-targeted `run` operation
//! is genuinely branch-isolated — the API provisions the candidate Nessie
//! branch and a branch-scoped Iceberg catalog, recompiles against it, and
//! binds the run to the candidate's post-run head, leaving `main`
//! physically untouched. Ignored by default; run in CI with `--ignored`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

use phlo_transform_daemon::{router, ServiceConfig, WorkspaceService};
use phlo_transform_engine::{
    catalog_name, read_environment_for, Adapter, CatalogRequest, CatalogStatus, SqliteStateStore,
};
use phlo_transform_nessie::{NessieClient, NessieConfig, NessieRestClient};
use phlo_transform_trino::{TrinoAdapter, TrinoConfig};

const TRINO_CONFIG: &str = "\
coordinator=true
node-scheduler.include-coordinator=true
http-server.http.port=8080
discovery.uri=http://localhost:8080
catalog.management=dynamic
catalog.store=memory
";

const WAREHOUSE: &str = "local:///tmp/phlo-warehouse";

/// A two-model workspace compiled against the `phlo_main` catalog: `raw`
/// materialises the given value, `results` copies it.
fn workspace(dir: &Path, value: i32) {
    std::fs::write(
        dir.join("phlo.toml"),
        "[transform]\ndefault_materialization = \"table\"\n\
         default_catalog = \"phlo_main\"\ndefault_schema = \"default\"\n",
    )
    .expect("phlo.toml");
    let transforms = dir.join("transforms").join("assay");
    std::fs::create_dir_all(&transforms).expect("dirs");
    std::fs::write(
        transforms.join("raw.sql"),
        format!("select 1 as id, {value} as value\n"),
    )
    .expect("raw.sql");
    std::fs::write(transforms.join("results.sql"), "select * from assay.raw\n")
        .expect("results.sql");
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

async fn wait_operation(client: &reqwest::Client, base: &str, id: &str) -> serde_json::Value {
    for _ in 0..600 {
        let body = get_json(client, &format!("{base}/v1/operations/{id}")).await;
        let status = body["operation"]["status"].as_str().unwrap_or("");
        if matches!(status, "succeeded" | "failed" | "cancelled") {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("operation {id} did not finish");
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn environment_targeted_run_is_branch_isolated() {
    let suffix = std::process::id();
    let network = format!("phlo-daemon-it-{suffix}");
    let nessie_name = format!("phlo-nessie-daemon-it-{suffix}");

    let nessie = GenericImage::new("ghcr.io/projectnessie/nessie", "latest")
        .with_wait_for(WaitFor::message_on_stdout(
            "Listening on: http://0.0.0.0:19120",
        ))
        .with_exposed_port(19120.tcp())
        .with_container_name(&nessie_name)
        .with_network(&network)
        .with_startup_timeout(Duration::from_secs(240))
        .start()
        .await
        .expect("nessie starts");

    let trino = GenericImage::new("trinodb/trino", "latest")
        .with_wait_for(WaitFor::healthcheck())
        .with_exposed_port(8080.tcp())
        .with_container_name(format!("phlo-trino-daemon-it-{suffix}"))
        .with_network(&network)
        .with_copy_to(
            "/etc/trino/config.properties",
            TRINO_CONFIG.as_bytes().to_vec(),
        )
        .with_startup_timeout(Duration::from_secs(300))
        .start()
        .await
        .expect("trino starts");

    let trino_port = trino.get_host_port_ipv4(8080).await.expect("trino port");
    let nessie_port = nessie.get_host_port_ipv4(19120).await.expect("nessie port");
    // The catalog-facing URI is the container-network address — Trino must
    // reach Nessie inside Docker, not via the host-mapped port the daemon's
    // own client uses.
    let nessie_internal = format!("http://{nessie_name}:19120");

    let adapter = Arc::new(
        TrinoAdapter::new(TrinoConfig::new(format!("http://127.0.0.1:{trino_port}")))
            .expect("adapter"),
    );
    let nessie_client: Arc<dyn NessieClient> = Arc::new(
        NessieRestClient::new(NessieConfig::new(format!("http://127.0.0.1:{nessie_port}")))
            .expect("nessie client"),
    );

    // The base catalog reads and writes Nessie `main`.
    adapter
        .ensure_catalog(&CatalogRequest {
            catalog: "phlo_main".to_string(),
            reference: Some("main".to_string()),
            nessie_uri: Some(nessie_internal.clone()),
            warehouse: Some(WAREHOUSE.to_string()),
        })
        .await
        .expect("main catalog");

    let dir = tempfile::tempdir().expect("tempdir");
    workspace(dir.path(), 10);
    let service = WorkspaceService::load_with_config(
        dir.path(),
        ServiceConfig {
            adapter: Some(adapter.clone()),
            state: Some(Arc::new(
                SqliteStateStore::open(&dir.path().join("state.db")).expect("state"),
            )),
            nessie: Some(nessie_client.clone()),
            nessie_uri: Some(nessie_internal.clone()),
            warehouse: Some(WAREHOUSE.to_string()),
            ..ServiceConfig::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = router(service.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let base = format!("http://{address}");
    let client = reqwest::Client::new();

    // The base run targets `main` — the default environment — and writes
    // through `phlo_main` onto the main branch.
    let (status, submitted) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({"kind": "run", "params": {"environment": "main"}}),
    )
    .await;
    assert_eq!(status, 200, "{submitted}");
    let op = submitted["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &op).await;
    assert_eq!(done["operation"]["result"]["status"], "passed", "{done}");
    let main_value = adapter
        .execute("SELECT value FROM phlo_main.default.assay__results")
        .await
        .expect("main read");
    assert_eq!(main_value.rows[0][0], "10");
    let main_head = nessie_client
        .get_reference("main")
        .await
        .expect("main")
        .expect("main exists")
        .hash;

    // The workspace now defines the candidate change — the operation
    // recompiles from disk, so the API sees the new source.
    workspace(dir.path(), 25);

    // Plan/run parity: a read-only `plan(environment=ci/pr-1)` resolves the
    // same physical catalog the run will execute against — and provisions
    // nothing (the ref need not exist yet for a preview).
    let planned = get_json(&client, &format!("{base}/v1/plan?environment=ci%2Fpr-1")).await;
    let candidate_catalog = catalog_name("ci/pr-1");
    let planned_target = planned["models"]
        .as_array()
        .and_then(|models| {
            models
                .iter()
                .find(|model| model["id"].as_str() == Some("assay.results"))
        })
        .and_then(|model| model["target"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("assay.results missing from plan: {planned}"));
    assert!(
        planned_target.starts_with(&format!("{candidate_catalog}.")),
        "plan must target the candidate catalog {candidate_catalog}: {planned_target}"
    );

    // `run(environment=ci/pr-1)` provisions the branch and its catalog,
    // retargets the compile, executes there, and binds the run to the
    // candidate's post-run head.
    let (status, submitted) = post_json(
        &client,
        &format!("{base}/v1/operations"),
        &serde_json::json!({
            "kind": "run",
            "params": {"environment": "ci/pr-1", "base": "main"}
        }),
    )
    .await;
    assert_eq!(status, 200, "{submitted}");
    let op = submitted["operation"]["id"].as_str().unwrap().to_string();
    let done = wait_operation(&client, &base, &op).await;
    assert_eq!(done["operation"]["status"], "succeeded", "{done}");
    assert_eq!(done["operation"]["result"]["status"], "passed", "{done}");
    assert_eq!(
        done["operation"]["result"]["environment"].as_str(),
        Some("ci/pr-1"),
        "{done}"
    );
    let candidate_run_id = done["operation"]["result"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The run executed exactly what the earlier read-only plan previewed —
    // the machine API's plan→run contract is exact.
    let ran_target = done["operation"]["result"]["models"]
        .as_array()
        .and_then(|models| {
            models
                .iter()
                .find(|model| model["model"].as_str() == Some("assay.results"))
        })
        .and_then(|model| model["target"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("assay.results missing from run result: {done}"));
    assert_eq!(
        ran_target, planned_target,
        "run target must equal the target `plan(environment)` previewed"
    );

    // The candidate catalog and branch now exist and hold the new value;
    // `main` is physically and commit-wise untouched.
    let candidate_value = adapter
        .execute(&format!(
            "SELECT value FROM {candidate_catalog}.default.assay__results"
        ))
        .await
        .expect("candidate read");
    assert_eq!(candidate_value.rows[0][0], "25");
    let main_after = adapter
        .execute("SELECT value FROM phlo_main.default.assay__results")
        .await
        .expect("main read");
    assert_eq!(main_after.rows[0][0], "10");
    let main_head_after = nessie_client
        .get_reference("main")
        .await
        .expect("main")
        .expect("main exists")
        .hash;
    assert_eq!(main_head_after, main_head, "main must not move");

    // The stored run is bound to the candidate's POST-RUN head — the commit
    // its writes produced, not the provisioning-time hash.
    let candidate_head = nessie_client
        .get_reference("ci/pr-1")
        .await
        .expect("candidate")
        .expect("candidate exists")
        .hash;
    assert_ne!(candidate_head, main_head);
    let shown = get_json(&client, &format!("{base}/v1/state/runs/{candidate_run_id}")).await;
    assert_eq!(
        shown["run"]["reference_hash"].as_str(),
        Some(candidate_head.as_str()),
        "the run is bound to the candidate's post-run head: {shown}"
    );
    assert_eq!(
        shown["run"]["environment"].as_str(),
        Some("ci/pr-1"),
        "{shown}"
    );

    // The provisioning evidence the operation persisted records the
    // cut-from provenance and the candidate catalog.
    let setup = read_environment_for(dir.path(), None, "ci/pr-1")
        .expect("read")
        .expect("environment artifact");
    assert_eq!(setup.candidate.name, "ci/pr-1");
    assert_eq!(setup.catalog, candidate_catalog);
    assert_eq!(setup.catalog_status, CatalogStatus::Created);
    assert_eq!(
        setup.created_from.as_ref().map(|base| base.name.as_str()),
        Some("main")
    );
}
