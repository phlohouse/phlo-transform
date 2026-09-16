//! Real Trino + Iceberg planning benchmark.
//!
//! Starts Nessie and Trino on a shared Docker network, materialises a
//! generated workspace of N models into an Iceberg catalog, then measures
//! planning against live warehouse metadata: cold (nothing recorded),
//! warm (every model recorded with a live output identity), changed
//! subset, and the `changed`-selector path `--since` resolves to.
//!
//! Every adapter call is counted and timed so the report separates wall
//! duration from metadata round trips — in particular the per-relation
//! `output_identity` Iceberg snapshot reads that warm plans pay for.
//!
//! Run with:
//!   cargo test -p phlo-transform-trino --test plan_scaling_e2e -- \
//!     --ignored --nocapture
//!
//! `PHLO_BENCH_MODELS="100,1000"` overrides the size list (default
//! `100,1000,5000`); `PHLO_BENCH_CHANGED=10` overrides the changed-subset
//! percentage (default 1%, minimum one model). At 5,000 models the
//! materialise step creates 5,000 real Iceberg tables — expect tens of
//! minutes.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

use phlo_transform_core::{
    compile, Compilation, Materialization, ModelId, Relation, Selection, SelectorSet,
    SemanticModel, SemanticProject, WorkspaceDefaults,
};
use phlo_transform_engine::{
    changed_models, Adapter, AdapterError, CatalogRequest, ColumnInfo, ExecutionStatus,
    PlanOptions, Planner, QueryResult, RunOptions, Runner, SqliteStateStore, StateStore,
};
use phlo_transform_trino::{TrinoAdapter, TrinoConfig};

const TRINO_CONFIG: &str = "\
coordinator=true
node-scheduler.include-coordinator=true
http-server.http.port=8080
discovery.uri=http://localhost:8080
catalog.management=dynamic
catalog.store=memory
";

const WAREHOUSE: &str = "local:///tmp/phlo-bench-warehouse";
const CATALOG: &str = "phlo_bench";
const LAYERS: usize = 5;

/// Counts adapter calls and accumulates per-method wall time, so a plan's
/// cost shows up as "N metadata queries, T total latency" rather than one
/// opaque duration.
#[derive(Default)]
struct Counting {
    calls: Mutex<BTreeMap<&'static str, (usize, Duration)>>,
}

struct CountingAdapter {
    inner: Arc<dyn Adapter>,
    counting: Arc<Counting>,
}

impl CountingAdapter {
    async fn timed<T>(
        &self,
        method: &'static str,
        call: impl std::future::Future<Output = T>,
    ) -> T {
        let started = Instant::now();
        let out = call.await;
        let mut calls = self.counting.calls.lock().unwrap();
        let entry = calls.entry(method).or_default();
        entry.0 += 1;
        entry.1 += started.elapsed();
        out
    }
}

#[async_trait]
impl Adapter for CountingAdapter {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn relation_exists(&self, relation: &Relation) -> Result<bool, AdapterError> {
        let inner = self.inner.clone();
        let relation = relation.clone();
        self.timed("relation_exists", async move {
            inner.relation_exists(&relation).await
        })
        .await
    }

    async fn relations_exist(&self, relations: &[Relation]) -> Result<Vec<bool>, AdapterError> {
        let inner = self.inner.clone();
        let relations = relations.to_vec();
        self.timed("relations_exist", async move {
            inner.relations_exist(&relations).await
        })
        .await
    }

    async fn execute(&self, sql: &str) -> Result<QueryResult, AdapterError> {
        let inner = self.inner.clone();
        let sql = sql.to_string();
        self.timed("execute", async move { inner.execute(&sql).await })
            .await
    }

    async fn create_or_replace_view(
        &self,
        relation: &Relation,
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let inner = self.inner.clone();
        let (relation, sql) = (relation.clone(), sql.to_string());
        self.timed("create_or_replace_view", async move {
            inner.create_or_replace_view(&relation, &sql).await
        })
        .await
    }

    async fn create_or_replace_table(
        &self,
        relation: &Relation,
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let inner = self.inner.clone();
        let (relation, sql) = (relation.clone(), sql.to_string());
        self.timed("create_or_replace_table", async move {
            inner.create_or_replace_table(&relation, &sql).await
        })
        .await
    }

    async fn append(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError> {
        let inner = self.inner.clone();
        let (relation, sql) = (relation.clone(), sql.to_string());
        self.timed("append", async move { inner.append(&relation, &sql).await })
            .await
    }

    async fn merge(
        &self,
        relation: &Relation,
        key_columns: &[String],
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let inner = self.inner.clone();
        let (relation, key_columns, sql) =
            (relation.clone(), key_columns.to_vec(), sql.to_string());
        self.timed("merge", async move {
            inner.merge(&relation, &key_columns, &sql).await
        })
        .await
    }

    async fn replace_partitions(
        &self,
        relation: &Relation,
        partition_columns: &[String],
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let inner = self.inner.clone();
        let (relation, partition_columns, sql) = (
            relation.clone(),
            partition_columns.to_vec(),
            sql.to_string(),
        );
        self.timed("replace_partitions", async move {
            inner
                .replace_partitions(&relation, &partition_columns, &sql)
                .await
        })
        .await
    }

    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError> {
        self.inner.cancel(query_id).await
    }

    fn track_attempt(&self) -> Option<Arc<dyn Adapter>> {
        self.inner.track_attempt()
    }

    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError> {
        let inner = self.inner.clone();
        let relation = relation.clone();
        self.timed("relation_columns", async move {
            inner.relation_columns(&relation).await
        })
        .await
    }

    async fn relation_columns_many(
        &self,
        relations: &[Relation],
    ) -> Vec<Result<Vec<ColumnInfo>, AdapterError>> {
        let inner = self.inner.clone();
        let relations = relations.to_vec();
        self.timed("relation_columns_many", async move {
            inner.relation_columns_many(&relations).await
        })
        .await
    }

    fn supports_catalog_provisioning(&self) -> bool {
        self.inner.supports_catalog_provisioning()
    }

    async fn ensure_catalog(
        &self,
        request: &CatalogRequest,
    ) -> Result<phlo_transform_engine::CatalogStatus, AdapterError> {
        self.inner.ensure_catalog(request).await
    }

    async fn drop_catalog(&self, catalog: &str) -> Result<(), AdapterError> {
        self.inner.drop_catalog(catalog).await
    }

    async fn ensure_schema(&self, relation: &Relation) -> Result<(), AdapterError> {
        let inner = self.inner.clone();
        let relation = relation.clone();
        self.timed("ensure_schema", async move {
            inner.ensure_schema(&relation).await
        })
        .await
    }

    async fn source_state(&self, relation: &Relation) -> Result<Option<String>, AdapterError> {
        let inner = self.inner.clone();
        let relation = relation.clone();
        self.timed("source_state", async move {
            inner.source_state(&relation).await
        })
        .await
    }

    async fn output_identity(&self, relation: &Relation) -> Result<Option<String>, AdapterError> {
        let inner = self.inner.clone();
        let relation = relation.clone();
        self.timed("output_identity", async move {
            inner.output_identity(&relation).await
        })
        .await
    }

    async fn partition_counts(
        &self,
        relation: &Relation,
        partition_columns: &[String],
    ) -> Result<Option<Vec<(String, i64)>>, AdapterError> {
        let inner = self.inner.clone();
        let (relation, partition_columns) = (relation.clone(), partition_columns.to_vec());
        self.timed("partition_counts", async move {
            inner.partition_counts(&relation, &partition_columns).await
        })
        .await
    }

    async fn load_csv(
        &self,
        relation: &Relation,
        path: &Path,
    ) -> Result<QueryResult, AdapterError> {
        self.inner.load_csv(relation, path).await
    }
}

/// `count` models across `LAYERS` namespaces; each layer-L model reads one
/// parent in layer L-1, so dependency depth and the downstream-propagation
/// a real project has are both present. `changed` models get a different
/// literal so their version hashes move.
fn scaled_project(count: usize, changed: &std::collections::BTreeSet<usize>) -> Compilation {
    let per_layer = count.div_ceil(LAYERS);
    let mut models = Vec::with_capacity(count);
    for layer in 0..LAYERS {
        for i in 0..per_layer {
            let index = layer * per_layer + i;
            if index >= count {
                break;
            }
            let literal = if changed.contains(&index) { 2 } else { 1 };
            let sql = if layer == 0 {
                format!("select {literal} as id")
            } else {
                let parent = (layer - 1) * per_layer + (i % per_layer);
                format!(
                    "select {literal} as id, p.id as parent from m{}.m{} p",
                    layer - 1,
                    parent
                )
            };
            let mut model = SemanticModel::in_memory(
                ModelId::parse(&format!("m{layer}.m{index}")).unwrap(),
                sql,
            );
            model.config.materialization = Materialization::Table;
            models.push(model);
        }
    }
    let mut project = SemanticProject::in_memory(models);
    project.defaults = WorkspaceDefaults {
        materialization: Materialization::Table,
        catalog: Some(CATALOG.to_string()),
        schema: Some("default".to_string()),
    };
    let compilation = compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    compilation
}

fn bench_sizes() -> Vec<usize> {
    std::env::var("PHLO_BENCH_MODELS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|part| part.trim().parse().ok())
                .collect()
        })
        .filter(|sizes: &Vec<usize>| !sizes.is_empty())
        .unwrap_or_else(|| vec![100, 1_000, 5_000])
}

fn bench_changed_percent(count: usize) -> usize {
    let percent: usize = std::env::var("PHLO_BENCH_CHANGED")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(1);
    (count * percent / 100).max(1)
}

fn report(models: usize, scenario: &str, elapsed: Duration, counting: &Counting, note: String) {
    let calls = counting.calls.lock().unwrap();
    let total: usize = calls.values().map(|(count, _)| *count).sum();
    let latency: Duration = calls.values().map(|(_, duration)| *duration).sum();
    let detail = calls
        .iter()
        .map(|(method, (count, duration))| {
            format!("{method}={count}/{:.0}ms", duration.as_millis())
        })
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "BENCH {:>5} models {:<16} {:>8}ms  {:>6} adapter calls ({:.0}ms latency)  {}{}",
        models,
        scenario,
        elapsed.as_millis(),
        total,
        latency.as_millis(),
        detail,
        if note.is_empty() {
            note
        } else {
            format!("  | {note}")
        },
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires Docker; run explicitly — see file header"]
async fn plan_scaling_on_real_trino() {
    let suffix = std::process::id();
    let network = format!("phlo-bench-it-{suffix}");
    let nessie_name = format!("phlo-bench-nessie-{suffix}");

    let _nessie = GenericImage::new("ghcr.io/projectnessie/nessie", "latest")
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
        .with_container_name(format!("phlo-bench-trino-{suffix}"))
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
    let nessie_internal = format!("http://{nessie_name}:19120");

    let adapter: Arc<dyn Adapter> = Arc::new(
        TrinoAdapter::new(TrinoConfig::new(format!("http://127.0.0.1:{trino_port}")))
            .expect("adapter"),
    );
    adapter
        .ensure_catalog(&CatalogRequest {
            catalog: CATALOG.to_string(),
            reference: Some("main".to_string()),
            nessie_uri: Some(nessie_internal),
            warehouse: Some(WAREHOUSE.to_string()),
        })
        .await
        .expect("bench catalog");

    for count in bench_sizes() {
        let counting = Arc::new(Counting::default());
        let adapter: Arc<dyn Adapter> = Arc::new(CountingAdapter {
            inner: adapter.clone(),
            counting: counting.clone(),
        });
        let state: Arc<dyn StateStore> = Arc::new(SqliteStateStore::in_memory().expect("state"));

        // Cold: nothing recorded, nothing materialised. Every model plans
        // Build off one batched existence probe.
        let compilation = scaled_project(count, &Default::default());
        let selected = Selection::all(&compilation);
        let started = Instant::now();
        let plan = Planner::new(adapter.clone(), Some(state.clone()))
            .plan(&compilation, &selected, None, &PlanOptions::default())
            .await
            .expect("cold plan");
        report(count, "cold", started.elapsed(), &counting, String::new());
        assert_eq!(plan.models.len(), count);

        // Materialise so the warm scenarios have real tables and recorded
        // output identities to check. Transient catalog/commit failures
        // (a hiccupping Nessie under thousands of rapid commits) get a
        // retry budget, then bounded re-plans — each one rebuilds only
        // what has no recorded materialisation.
        counting.calls.lock().unwrap().clear();
        let started = Instant::now();
        let options = RunOptions {
            run_tests: false,
            retry: phlo_transform_engine::RetryPolicy {
                retries: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut run = Runner::new(adapter.clone(), Some(state.clone()))
            .apply(&compilation, &plan, &options)
            .await
            .expect("materialise");
        let mut attempts = 1;
        while run.status != ExecutionStatus::Passed && attempts < 3 {
            attempts += 1;
            let plan = Planner::new(adapter.clone(), Some(state.clone()))
                .plan(
                    &compilation,
                    &Selection::all(&compilation),
                    None,
                    &PlanOptions::default(),
                )
                .await
                .expect("re-plan");
            run = Runner::new(adapter.clone(), Some(state.clone()))
                .apply(&compilation, &plan, &options)
                .await
                .expect("re-materialise");
        }
        report(
            count,
            "materialise",
            started.elapsed(),
            &counting,
            format!("status={:?} attempts={attempts}", run.status),
        );
        assert_eq!(run.status, ExecutionStatus::Passed, "{:?}", run.models);

        // Warm: every model recorded with a strong output identity. Each
        // pays a live snapshot read (the drift check) — this is the number
        // that decides whether warm plans stay cheap at scale.
        counting.calls.lock().unwrap().clear();
        let started = Instant::now();
        let warm = Planner::new(adapter.clone(), Some(state.clone()))
            .plan(
                &compilation,
                &Selection::all(&compilation),
                None,
                &PlanOptions::default(),
            )
            .await
            .expect("warm plan");
        let skipped = warm
            .models
            .iter()
            .filter(|model| model.action == phlo_transform_engine::PlanAction::Skip)
            .count();
        report(
            count,
            "warm",
            started.elapsed(),
            &counting,
            format!("{skipped}/{count} skip"),
        );

        // Changed subset: 1% of models recompiled with different SQL, then a
        // full-selection plan — the CI shape after a small commit. Clamped
        // to the model count so an oversized PHLO_BENCH_CHANGED cannot make
        // the stride zero.
        let changed = bench_changed_percent(count).min(count);
        let changed_indexes: std::collections::BTreeSet<usize> =
            (0..count).step_by(count / changed).take(changed).collect();
        let touched = scaled_project(count, &changed_indexes);
        let change_set = changed_models(&touched, Some(&state), None).expect("change set");
        counting.calls.lock().unwrap().clear();
        let started = Instant::now();
        let plan = Planner::new(adapter.clone(), Some(state.clone()))
            .plan(
                &touched,
                &Selection::all(&touched),
                None,
                &PlanOptions::default(),
            )
            .await
            .expect("changed plan");
        let building = plan
            .models
            .iter()
            .filter(|model| model.action != phlo_transform_engine::PlanAction::Skip)
            .count();
        report(
            count,
            "changed-subset",
            started.elapsed(),
            &counting,
            format!("{building} build of {count}"),
        );

        // `--since`: the CLI resolves the Git-derived change set into the
        // `changed` selector; state-derived `changed_models` produces the
        // same set shape here (edited models plus their dependents).
        let set =
            SelectorSet::parse(&["changed".to_string()], &[], &[], false, false).expect("selector");
        let selected = phlo_transform_core::resolve_selection(&touched, &set, Some(&change_set))
            .expect("resolve");
        counting.calls.lock().unwrap().clear();
        let started = Instant::now();
        let plan = Planner::new(adapter.clone(), Some(state.clone()))
            .plan(&touched, &selected, None, &PlanOptions::default())
            .await
            .expect("since plan");
        report(
            count,
            "since",
            started.elapsed(),
            &counting,
            format!("{} selected of {count}", plan.models.len()),
        );

        // Microbenchmark: per-call snapshot-read latency — the unit cost
        // behind every warm-plan `output_identity` line above.
        let sample: Vec<Relation> = compilation
            .models
            .iter()
            .take(50)
            .map(|model| model.target.clone())
            .collect();
        counting.calls.lock().unwrap().clear();
        let started = Instant::now();
        for relation in &sample {
            adapter
                .output_identity(relation)
                .await
                .expect("identity")
                .expect("iceberg snapshot");
        }
        report(
            count,
            "output_identity x50",
            started.elapsed(),
            &counting,
            format!(
                "{:.1}ms/call",
                started.elapsed().as_millis() as f64 / sample.len() as f64
            ),
        );
    }
}
