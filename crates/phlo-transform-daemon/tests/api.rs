//! Daemon API integration tests.

use std::path::PathBuf;

use phlo_transform_daemon::{router, spawn_watcher, WorkspaceService};
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
