//! Golden-path end-to-end test: the real product lifecycle through the CLI
//! binary against a real Trino + Iceberg + Nessie stack.
//!
//!   init workspace (git) → run on main → edit a model → `plan --since`
//!   (Git-aware changed selection) → `run --ref` candidate environment →
//!   induce a failure → `run --retry-failed` (environment inferred from the
//!   stored run) → `lineage --diff` → `diff --from <env> --to main --full`
//!   → `promote --check` → `promote --cleanup` → main serves the candidate
//!   data, the candidate branch and its phlo-owned catalog are gone.
//!
//! The assertions are on observable warehouse and Nessie state, not exit
//! codes alone. Ignored by default; run in CI with `--ignored`.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use tempfile::TempDir;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

use phlo_transform_engine::{catalog_name, Adapter, CatalogRequest, CatalogStatus};
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
const CANDIDATE: &str = "ci/golden";

/// The workspace under test: two models and a custom test, in a Git repo so
/// `--since` has a change set to resolve.
fn write_workspace(root: &Path, results_value: i32) {
    let transforms = root.join("workflows/assay/transforms");
    std::fs::create_dir_all(&transforms).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("phlo.toml"),
        "[transform]\ndefault_catalog = \"phlo_main\"\ndefault_schema = \"default\"\ndefault_materialization = \"table\"\n",
    )
    .unwrap();
    std::fs::write(
        transforms.join("transform.toml"),
        "materialized = \"table\"\n",
    )
    .unwrap();
    std::fs::write(transforms.join("raw.sql"), "select 1 as id, 10 as value").unwrap();
    std::fs::write(
        transforms.join("results.sql"),
        format!("select id, {results_value} as value from assay.raw"),
    )
    .unwrap();
    std::fs::write(
        root.join("tests/results_positive.sql"),
        "select * from assay.results where value < 0",
    )
    .unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "phlo")
        .env("GIT_AUTHOR_EMAIL", "phlo@test")
        .env("GIT_COMMITTER_NAME", "phlo")
        .env("GIT_COMMITTER_EMAIL", "phlo@test")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

struct Golden {
    root: TempDir,
    trino_endpoint: String,
    nessie_endpoint: String,
    /// The URI Trino's Iceberg catalogs use to reach Nessie — the
    /// container-network address, not the host-mapped one.
    nessie_catalog_uri: String,
}

impl Golden {
    /// `phlo-transform` invoked against the workspace and the live stack.
    fn phlo(&self, args: &[&str]) -> Output {
        Command::cargo_bin("phlo-transform")
            .expect("binary builds")
            .args([
                "--root",
                &self.root.path().to_string_lossy(),
                "--adapter",
                "trino",
                "--trino-endpoint",
                &self.trino_endpoint,
                "--trino-catalog",
                "phlo_main",
                "--nessie-endpoint",
                &self.nessie_endpoint,
                "--nessie-catalog-uri",
                &self.nessie_catalog_uri,
                "--warehouse",
                WAREHOUSE,
            ])
            .args(args)
            .output()
            .expect("command runs")
    }

    /// Same but with `--json` for machine-readable assertions.
    fn run_json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let output = self.phlo(&full);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "phlo {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("{args:?} did not emit JSON: {error}\n{stdout}"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker; run with --ignored"]
async fn golden_path() {
    let suffix = std::process::id();
    let network = format!("phlo-golden-it-{suffix}");
    let nessie_name = format!("phlo-golden-nessie-{suffix}");

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
        .with_container_name(format!("phlo-golden-trino-{suffix}"))
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
    let nessie_internal = format!("http://{nessie_name}:19120");

    let adapter = Arc::new(
        TrinoAdapter::new(TrinoConfig::new(format!("http://127.0.0.1:{trino_port}")))
            .expect("adapter"),
    );
    let nessie_client =
        NessieRestClient::new(NessieConfig::new(format!("http://127.0.0.1:{nessie_port}")))
            .expect("nessie client");

    // The base catalog serves `main`. Candidates get their own catalog per
    // ref — provisioned by `run --ref`, not by hand.
    let status = adapter
        .ensure_catalog(&CatalogRequest {
            catalog: "phlo_main".to_string(),
            reference: Some("main".to_string()),
            nessie_uri: Some(nessie_internal.clone()),
            warehouse: Some(WAREHOUSE.to_string()),
        })
        .await
        .expect("main catalog");
    assert_eq!(status, CatalogStatus::Created);

    let golden = Golden {
        root: tempfile::tempdir().expect("workspace"),
        trino_endpoint: format!("http://127.0.0.1:{trino_port}"),
        nessie_endpoint: format!("http://127.0.0.1:{nessie_port}"),
        nessie_catalog_uri: nessie_internal.clone(),
    };
    let root: PathBuf = golden.root.path().to_path_buf();
    write_workspace(&root, 10);
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-m", "initial"]);

    // --- initial main run -------------------------------------------------
    let output = golden.phlo(&["run"]);
    assert!(
        output.status.success(),
        "main run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let main_value = adapter
        .execute("SELECT value FROM phlo_main.default.assay__results")
        .await
        .expect("main read");
    assert_eq!(main_value.rows[0][0], "10", "main serves the base data");

    // --- edit + Git-aware changed selection -------------------------------
    write_workspace(&root, 25);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-m", "results: value 25"]);

    let plan = golden.run_json(&["plan", "--since", "HEAD~1"]);
    let matched: Vec<&str> = plan["selection"]["matched"]
        .as_array()
        .expect("matched list")
        .iter()
        .filter_map(|model| model.as_str())
        .collect();
    assert_eq!(matched, vec!["assay.results"], "only the edited model");

    // --- candidate environment run ----------------------------------------
    let output = golden.phlo(&["run", "--ref", CANDIDATE, "--since", "HEAD~1"]);
    assert!(
        output.status.success(),
        "candidate run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let candidate_catalog = catalog_name(CANDIDATE);
    let candidate_value = adapter
        .execute(&format!(
            "SELECT value FROM {candidate_catalog}.default.assay__results"
        ))
        .await
        .expect("candidate read");
    assert_eq!(candidate_value.rows[0][0], "25");
    // Isolation: main is untouched.
    let main_value = adapter
        .execute("SELECT value FROM phlo_main.default.assay__results")
        .await
        .expect("main read");
    assert_eq!(
        main_value.rows[0][0], "10",
        "candidate writes stay on the branch"
    );

    // --- induce a failure, then retry the failed portion -------------------
    std::fs::write(
        root.join("workflows/assay/transforms/results.sql"),
        "select id, 1/0 as value from assay.raw",
    )
    .unwrap();
    let output = golden.phlo(&["run", "--ref", CANDIDATE]);
    assert!(
        !output.status.success(),
        "the failing model must fail the run"
    );
    let runs = golden.run_json(&["state", "runs"]);
    let failed = runs
        .as_array()
        .expect("runs list")
        .iter()
        .find(|run| {
            run["environment"].as_str() == Some(CANDIDATE)
                && run["status"].as_str() != Some("passed")
        })
        .unwrap_or_else(|| panic!("a failed {CANDIDATE} run: {runs}"));
    let run_id = failed["run_id"].as_str().expect("run id").to_string();

    // Fix the model and retry WITHOUT repeating --ref — the environment is
    // inferred from the stored run.
    write_workspace(&root, 25);
    let output = golden.phlo(&["run", "--retry-failed", &run_id]);
    assert!(
        output.status.success(),
        "retry-failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // --- lineage diff (Git baseline) --------------------------------------
    let output = golden.phlo(&["lineage", "--diff", "HEAD~1"]);
    assert!(
        output.status.success(),
        "lineage diff: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // --- full branch diff → promotion evidence -----------------------------
    let output = golden.phlo(&["diff", "--from", CANDIDATE, "--to", "main", "--full"]);
    assert!(
        output.status.success(),
        "branch diff: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // --- promote --check, then promote + cleanup ---------------------------
    let output = golden.phlo(&["promote", CANDIDATE, "--to", "main", "--check"]);
    assert!(
        output.status.success(),
        "promote --check: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = golden.phlo(&["promote", CANDIDATE, "--to", "main", "--cleanup"]);
    assert!(
        output.status.success(),
        "promote --cleanup: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Main now serves the candidate's data.
    let promoted = adapter
        .execute("SELECT value FROM phlo_main.default.assay__results")
        .await
        .expect("promoted read");
    assert_eq!(promoted.rows[0][0], "25", "main serves the promoted data");

    // Cleanup removed the branch and the phlo-owned catalog.
    let refs = nessie_client.list_references().await.expect("list refs");
    assert!(
        !refs.iter().any(|reference| reference.name == CANDIDATE),
        "candidate branch deleted: {refs:?}"
    );
    let catalogs = adapter
        .execute(&format!(
            "SELECT connector_name FROM system.metadata.catalogs \
             WHERE catalog_name = '{}'",
            candidate_catalog.replace('\'', "''")
        ))
        .await
        .expect("catalog lookup");
    assert!(
        catalogs.rows.is_empty(),
        "the phlo-owned candidate catalog is dropped, not left behind"
    );
}
