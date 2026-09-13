//! PostgreSQL-backed shared state store.
//!
//! The same [`StateStore`] contract as [`SqliteStateStore`], but against a
//! database many processes and machines can reach — CI jobs, developers and
//! environments sharing run history, materialised versions and watermarks.
//!
//! Concurrency: every write is a single statement (insert-or-upsert or
//! update), which Postgres executes atomically; two simultaneous writers
//! contend on row locks rather than corrupting records. A `BIGSERIAL`
//! tiebreak column preserves the deterministic newest-first ordering SQLite
//! gets from `rowid`.
//!
//! Threading: the `postgres` client drives its own internal Tokio runtime,
//! which cannot be blocked on inside an outer runtime. The client therefore
//! lives on a dedicated worker thread; each trait method ships a closure
//! over a channel and waits on the reply — legal from sync or async callers
//! alike, and the worker serialises access without a `Mutex<Client>`.

use std::sync::mpsc::{channel, Sender};
use std::thread::JoinHandle;

use postgres::{Client, NoTls, Row};

use phlo_transform_core::{ModelVersion, VersionDetail};

use crate::error::EngineError;
use crate::events::ExecutionStatus;
use crate::state::{
    incremental_key_claim, status_str, MaterializedRecord, ModelRunRecord, RunRecord, RunSummary,
    SeedRecord, SeedRunRecord, StateStore, StoredPlan, StoredRun, TestRunRecord,
};
use crate::util::now_rfc3339;

/// The `model_runs` read-back column list, in order — mirrors SQLite.
const MODEL_RUN_COLUMNS: &str = "run_id, model_id, materialization, status, started_at, \
     finished_at, sql_hash, target, query_id, error, action, desired_version, attempts_json, \
     error_category";

/// The `model_versions` read-back column list — mirrors `MATERIALIZED_COLUMNS`.
const MATERIALIZED_COLUMNS: &str = "model_id, environment, version_hash, sql_hash, config_hash, \
     contract_hash, dependency_hash, source_state_hash, compiler_version, target_hash, target, \
     run_id, materialized_at, incremental_strategy, incremental_key, version_detail, adapter, \
     output_identity, contract_json, effective_key_json";

fn parse_status(value: &str) -> ExecutionStatus {
    match value {
        "passed" => ExecutionStatus::Passed,
        "failed" => ExecutionStatus::Failed,
        "skipped" => ExecutionStatus::Skipped,
        "cached" => ExecutionStatus::Cached,
        "blocked" => ExecutionStatus::Blocked,
        "cancelled" => ExecutionStatus::Cancelled,
        "running" => ExecutionStatus::Running,
        "ready" => ExecutionStatus::Ready,
        _ => ExecutionStatus::Pending,
    }
}

fn map_error(error: postgres::Error) -> EngineError {
    EngineError::State(error.to_string())
}

fn run_summary(row: &Row) -> RunSummary {
    let status: String = row.get(5);
    let environment: String = row.get(2);
    RunSummary {
        run_id: row.get(0),
        plan_id: row.get(1),
        environment: if environment.is_empty() {
            None
        } else {
            Some(environment)
        },
        reference_hash: row.get(8),
        started_at: row.get(3),
        finished_at: row.get(4),
        status: parse_status(&status),
        model_count: row.get::<usize, i64>(6) as usize,
        failed_count: row.get::<usize, i64>(7) as usize,
    }
}

fn model_run_from_row(row: &Row) -> ModelRunRecord {
    let status: String = row.get(3);
    let attempts: Option<String> = row.get(12);
    ModelRunRecord {
        run_id: row.get(0),
        model_id: row.get(1),
        materialization: row.get(2),
        status: parse_status(&status),
        started_at: row.get(4),
        finished_at: row.get(5),
        sql_hash: row.get(6),
        target: row.get(7),
        query_id: row.get(8),
        error: row.get(9),
        action: row.get(10),
        desired_version: row.get(11),
        attempts: attempts
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
        error_category: row.get(13),
    }
}

fn materialized_from_row(row: &Row) -> MaterializedRecord {
    let environment: String = row.get(1);
    let detail: Option<String> = row.get(15);
    let contract: Option<String> = row.get(18);
    let effective_key: Option<String> = row.get(19);
    MaterializedRecord {
        model_id: row.get(0),
        environment: if environment.is_empty() {
            None
        } else {
            Some(environment)
        },
        version: ModelVersion {
            hash: row.get(2),
            sql_hash: row.get(3),
            config_hash: row.get(4),
            contract_hash: row.get(5),
            dependency_hash: row.get(6),
            source_state_hash: row.get(7),
            compiler_version: row.get(8),
            target_hash: row.get(9),
        },
        detail: detail.and_then(|json| serde_json::from_str::<VersionDetail>(&json).ok()),
        target: row.get(10),
        incremental_strategy: row.get(13),
        incremental_key: row.get(14),
        adapter: row.get(16),
        output_identity: row.get(17),
        contract: contract.and_then(|json| serde_json::from_str(&json).ok()),
        effective_key: effective_key.and_then(|json| serde_json::from_str(&json).ok()),
        run_id: row.get(11),
        materialized_at: row.get(12),
    }
}

fn seed_from_row(row: &Row) -> SeedRecord {
    let environment: String = row.get(1);
    SeedRecord {
        name: row.get(0),
        environment: if environment.is_empty() {
            None
        } else {
            Some(environment)
        },
        content_hash: row.get(2),
        target: row.get(3),
        run_id: row.get(4),
        loaded_at: row.get(5),
    }
}

/// One unit of work for the worker thread.
type Job = Box<dyn FnOnce(&mut Client) + Send>;

/// PostgreSQL implementation of [`StateStore`].
///
/// All calls dispatch to a dedicated worker thread that owns the
/// `postgres::Client`; dropping the store closes the channel and lets the
/// worker exit.
pub struct PostgresStateStore {
    jobs: Option<Sender<Job>>,
    worker: Option<JoinHandle<()>>,
}

impl PostgresStateStore {
    /// Connect and ensure the schema exists. `url` is a libpq-style
    /// connection string (`postgres://user@host/db`); TLS is not configured —
    /// point at networks or a socket you trust, as with any state store.
    pub fn connect(url: &str) -> Result<Self, EngineError> {
        let url = url.to_string();
        let (jobs, inbox) = channel::<Job>();
        let (ready, ready_rx) = channel::<Result<(), String>>();
        let worker = std::thread::spawn(move || {
            let mut client = match Client::connect(&url, NoTls) {
                Ok(client) => client,
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
            };
            if let Err(error) = ensure_schema(&mut client) {
                let _ = ready.send(Err(error));
                return;
            }
            let _ = ready.send(Ok(()));
            // Process work until every sender is gone.
            for job in inbox {
                job(&mut client);
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                jobs: Some(jobs),
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(EngineError::State(error))
            }
            Err(_) => {
                let _ = worker.join();
                Err(EngineError::State(
                    "postgres worker stopped during connect".to_string(),
                ))
            }
        }
    }

    /// Run `f` on the worker's client and wait for its result.
    fn call<T>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError>
    where
        T: Send + 'static,
    {
        let (reply, replies) = channel();
        self.jobs
            .as_ref()
            .ok_or_else(|| EngineError::State("postgres worker stopped".to_string()))?
            .send(Box::new(move |client| {
                let _ = reply.send(f(client));
            }))
            .map_err(|_| EngineError::State("postgres worker stopped".to_string()))?;
        replies
            .recv()
            .map_err(|_| EngineError::State("postgres worker stopped".to_string()))?
    }
}

impl Drop for PostgresStateStore {
    fn drop(&mut self) {
        // Taking the sender closes the inbox and ends the worker's receive
        // loop; joining here is safe because the worker owns the client.
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn ensure_schema(client: &mut Client) -> Result<(), String> {
    client
        .batch_execute(
            "
            CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                plan_id TEXT NOT NULL,
                environment TEXT NOT NULL DEFAULT '',
                started_at TEXT NOT NULL,
                finished_at TEXT,
                status TEXT NOT NULL,
                model_count BIGINT NOT NULL,
                failed_count BIGINT NOT NULL,
                plan_json TEXT,
                reference_hash TEXT,
                seq BIGINT GENERATED ALWAYS AS IDENTITY
            );
            CREATE TABLE IF NOT EXISTS model_runs (
                run_id TEXT NOT NULL,
                model_id TEXT NOT NULL,
                materialization TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                sql_hash TEXT NOT NULL,
                target TEXT NOT NULL,
                query_id TEXT,
                error TEXT,
                action TEXT NOT NULL DEFAULT '',
                desired_version TEXT NOT NULL DEFAULT '',
                attempts_json TEXT,
                error_category TEXT,
                PRIMARY KEY (run_id, model_id)
            );
            CREATE TABLE IF NOT EXISTS seed_runs (
                run_id TEXT NOT NULL,
                name TEXT NOT NULL,
                status TEXT NOT NULL,
                target TEXT NOT NULL,
                attempts_json TEXT,
                error TEXT,
                error_category TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                PRIMARY KEY (run_id, name)
            );
            CREATE TABLE IF NOT EXISTS test_runs (
                run_id TEXT NOT NULL,
                test_id TEXT NOT NULL,
                status TEXT NOT NULL,
                row_count BIGINT NOT NULL,
                query_id TEXT,
                error TEXT,
                error_category TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                PRIMARY KEY (run_id, test_id)
            );
            CREATE TABLE IF NOT EXISTS model_versions (
                model_id TEXT NOT NULL,
                environment TEXT NOT NULL DEFAULT '',
                version_hash TEXT NOT NULL,
                sql_hash TEXT NOT NULL,
                config_hash TEXT NOT NULL,
                contract_hash TEXT NOT NULL,
                dependency_hash TEXT NOT NULL,
                source_state_hash TEXT NOT NULL,
                compiler_version TEXT NOT NULL,
                target_hash TEXT NOT NULL,
                target TEXT NOT NULL,
                run_id TEXT NOT NULL,
                materialized_at TEXT NOT NULL,
                incremental_strategy TEXT,
                incremental_key TEXT,
                version_detail TEXT,
                adapter TEXT,
                output_identity TEXT,
                contract_json TEXT,
                effective_key_json TEXT,
                PRIMARY KEY (model_id, environment)
            );
            CREATE TABLE IF NOT EXISTS incremental_state (
                model_id TEXT NOT NULL,
                environment TEXT NOT NULL DEFAULT '',
                last_value TEXT,
                run_id TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (model_id, environment)
            );
            CREATE TABLE IF NOT EXISTS seed_loads (
                name TEXT NOT NULL,
                environment TEXT NOT NULL DEFAULT '',
                content_hash TEXT NOT NULL,
                target TEXT NOT NULL,
                run_id TEXT NOT NULL,
                loaded_at TEXT NOT NULL,
                PRIMARY KEY (name, environment)
            );
            CREATE TABLE IF NOT EXISTS promotions (
                promotion_id TEXT PRIMARY KEY,
                candidate_ref TEXT NOT NULL,
                target_ref TEXT NOT NULL,
                merged BIGINT NOT NULL,
                dry_run BIGINT NOT NULL,
                record_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                seq BIGINT GENERATED ALWAYS AS IDENTITY
            );
            ",
        )
        .map_err(|error| error.to_string())?;
    // Same best-effort migrations as the SQLite backend, for schemas
    // created before these columns existed.
    for column in [
        "ALTER TABLE model_versions ADD COLUMN incremental_strategy TEXT",
        "ALTER TABLE model_versions ADD COLUMN incremental_key TEXT",
        "ALTER TABLE model_versions ADD COLUMN version_detail TEXT",
        "ALTER TABLE model_versions ADD COLUMN adapter TEXT",
        "ALTER TABLE model_versions ADD COLUMN output_identity TEXT",
        "ALTER TABLE model_versions ADD COLUMN contract_json TEXT",
        "ALTER TABLE model_versions ADD COLUMN effective_key_json TEXT",
        "ALTER TABLE runs ADD COLUMN plan_json TEXT",
        "ALTER TABLE runs ADD COLUMN reference_hash TEXT",
        "ALTER TABLE runs ADD COLUMN seq BIGINT GENERATED ALWAYS AS IDENTITY",
        "ALTER TABLE promotions ADD COLUMN seq BIGINT GENERATED ALWAYS AS IDENTITY",
        "ALTER TABLE model_runs ADD COLUMN action TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE model_runs ADD COLUMN desired_version TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE model_runs ADD COLUMN attempts_json TEXT",
        "ALTER TABLE model_runs ADD COLUMN error_category TEXT",
        "ALTER TABLE test_runs ADD COLUMN error_category TEXT",
        "ALTER TABLE seed_runs ADD COLUMN error_category TEXT",
    ] {
        let _ = client.execute(column, &[]);
    }
    // Backfill `effective_key_json` for rows written before the column
    // existed: an incremental `key` strategy's columns were the era's
    // effective key — the only key concept state then recorded. Rows
    // without one stay NULL: their key is unknown, not absent.
    let legacy_keys = client
        .query(
            "SELECT model_id, environment, incremental_key FROM model_versions
             WHERE effective_key_json IS NULL
               AND incremental_strategy = 'key'
               AND incremental_key IS NOT NULL",
            &[],
        )
        .map_err(|error| error.to_string())?;
    for row in legacy_keys {
        let raw: String = row.get(2);
        let Some(claim) = incremental_key_claim(&raw) else {
            continue;
        };
        let json = serde_json::to_string(&claim).map_err(|error| error.to_string())?;
        client
            .execute(
                "UPDATE model_versions SET effective_key_json = $1
                 WHERE model_id = $2 AND environment = $3",
                &[&json, &row.get::<_, String>(0), &row.get::<_, String>(1)],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl StateStore for PostgresStateStore {
    fn start_run(&self, run: &RunRecord, plan: &StoredPlan) -> Result<(), EngineError> {
        let plan_json =
            serde_json::to_string(plan).map_err(|error| EngineError::State(error.to_string()))?;
        let run = run.clone();
        self.call(move |client| {
            let environment = run.environment.clone().unwrap_or_default();
            let model_count = run.model_count as i64;
            let failed_count = run.failed_count as i64;
            let status = status_str(run.status);
            client
                .execute(
                    "INSERT INTO runs (run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, plan_json, reference_hash)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                    &[
                        &run.run_id,
                        &run.plan_id,
                        &environment,
                        &run.started_at,
                        &run.finished_at,
                        &status,
                        &model_count,
                        &failed_count,
                        &plan_json,
                        &run.reference_hash,
                    ],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn reopen_run(&self, run_id: &str) -> Result<(), EngineError> {
        let run_id = run_id.to_string();
        self.call(move |client| {
            client
                .execute(
                    "UPDATE runs SET status = 'running', finished_at = NULL WHERE run_id = $1",
                    &[&run_id],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn bind_run_reference_hash(
        &self,
        run_id: &str,
        reference_hash: &str,
    ) -> Result<(), EngineError> {
        let run_id = run_id.to_string();
        let reference_hash = reference_hash.to_string();
        self.call(move |client| {
            client
                .execute(
                    "UPDATE runs SET reference_hash = $2 WHERE run_id = $1",
                    &[&run_id, &reference_hash],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn finish_run(
        &self,
        run_id: &str,
        status: ExecutionStatus,
        finished_at: &str,
        failed_count: usize,
    ) -> Result<(), EngineError> {
        let run_id = run_id.to_string();
        let status = status_str(status);
        let finished_at = finished_at.to_string();
        let failed_count = failed_count as i64;
        self.call(move |client| {
            client
                .execute(
                    "UPDATE runs SET status = $2, finished_at = $3, failed_count = $4 WHERE run_id = $1",
                    &[&run_id, &status, &finished_at, &failed_count],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn record_model(&self, record: &ModelRunRecord) -> Result<(), EngineError> {
        let record = record.clone();
        self.call(move |client| {
            let attempts_json = serde_json::to_string(&record.attempts)
                .map_err(|error| EngineError::State(error.to_string()))?;
            let status = status_str(record.status);
            client
                .execute(
                    "INSERT INTO model_runs
                     (run_id, model_id, materialization, status, started_at, finished_at, sql_hash, target, query_id, error, action, desired_version, attempts_json, error_category)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                     ON CONFLICT (run_id, model_id) DO UPDATE SET
                        materialization = EXCLUDED.materialization,
                        status = EXCLUDED.status,
                        started_at = EXCLUDED.started_at,
                        finished_at = EXCLUDED.finished_at,
                        sql_hash = EXCLUDED.sql_hash,
                        target = EXCLUDED.target,
                        query_id = EXCLUDED.query_id,
                        error = EXCLUDED.error,
                        action = EXCLUDED.action,
                        desired_version = EXCLUDED.desired_version,
                        attempts_json = EXCLUDED.attempts_json,
                        error_category = EXCLUDED.error_category",
                    &[
                        &record.run_id,
                        &record.model_id,
                        &record.materialization,
                        &status,
                        &record.started_at,
                        &record.finished_at,
                        &record.sql_hash,
                        &record.target,
                        &record.query_id,
                        &record.error,
                        &record.action,
                        &record.desired_version,
                        &attempts_json,
                        &record.error_category,
                    ],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn record_seed_run(&self, record: &SeedRunRecord) -> Result<(), EngineError> {
        let record = record.clone();
        self.call(move |client| {
            let attempts_json = serde_json::to_string(&record.attempts)
                .map_err(|error| EngineError::State(error.to_string()))?;
            let status = status_str(record.status);
            client
                .execute(
                    "INSERT INTO seed_runs
                     (run_id, name, status, target, attempts_json, error, error_category, started_at, finished_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                     ON CONFLICT (run_id, name) DO UPDATE SET
                        status = EXCLUDED.status,
                        target = EXCLUDED.target,
                        attempts_json = EXCLUDED.attempts_json,
                        error = EXCLUDED.error,
                        error_category = EXCLUDED.error_category,
                        started_at = EXCLUDED.started_at,
                        finished_at = EXCLUDED.finished_at",
                    &[
                        &record.run_id,
                        &record.name,
                        &status,
                        &record.target,
                        &attempts_json,
                        &record.error,
                        &record.error_category,
                        &record.started_at,
                        &record.finished_at,
                    ],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn record_test(&self, record: &TestRunRecord) -> Result<(), EngineError> {
        let record = record.clone();
        self.call(move |client| {
            let status = status_str(record.status);
            let row_count = record.row_count as i64;
            client
                .execute(
                    "INSERT INTO test_runs
                     (run_id, test_id, status, row_count, query_id, error, error_category, started_at, finished_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                     ON CONFLICT (run_id, test_id) DO UPDATE SET
                        status = EXCLUDED.status,
                        row_count = EXCLUDED.row_count,
                        query_id = EXCLUDED.query_id,
                        error = EXCLUDED.error,
                        error_category = EXCLUDED.error_category,
                        started_at = EXCLUDED.started_at,
                        finished_at = EXCLUDED.finished_at",
                    &[
                        &record.run_id,
                        &record.test_id,
                        &status,
                        &row_count,
                        &record.query_id,
                        &record.error,
                        &record.error_category,
                        &record.started_at,
                        &record.finished_at,
                    ],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn runs(&self) -> Result<Vec<RunSummary>, EngineError> {
        self.call(|client| {
            let rows = client
                .query(
                    "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, reference_hash
                     FROM runs ORDER BY started_at DESC, seq DESC",
                    &[],
                )
                .map_err(map_error)?;
            Ok(rows.iter().map(run_summary).collect())
        })
    }

    fn latest_run(&self, environment: Option<&str>) -> Result<Option<RunSummary>, EngineError> {
        let environment = environment.unwrap_or("").to_string();
        self.call(move |client| {
            let row = client
                .query_opt(
                    "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, reference_hash
                     FROM runs WHERE environment = $1 ORDER BY started_at DESC, seq DESC LIMIT 1",
                    &[&environment],
                )
                .map_err(map_error)?;
            Ok(row.as_ref().map(run_summary))
        })
    }

    fn run(&self, run_id: &str) -> Result<Option<StoredRun>, EngineError> {
        let run_id = run_id.to_string();
        self.call(move |client| {
            let row = client
                .query_opt(
                    "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, plan_json, reference_hash
                     FROM runs WHERE run_id = $1",
                    &[&run_id],
                )
                .map_err(map_error)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let status: String = row.get(5);
            let environment: Option<String> = row.get(2);
            let plan_json: Option<String> = row.get(8);
            Ok(Some(StoredRun {
                record: RunRecord {
                    run_id: row.get(0),
                    plan_id: row.get(1),
                    environment: environment.filter(|value| !value.is_empty()),
                    reference_hash: row.get(9),
                    started_at: row.get(3),
                    finished_at: row.get(4),
                    status: parse_status(&status),
                    model_count: row.get::<usize, i64>(6) as usize,
                    failed_count: row.get::<usize, i64>(7) as usize,
                },
                plan: plan_json.and_then(|json| serde_json::from_str(&json).ok()),
            }))
        })
    }

    fn find_runs(&self, prefix: &str) -> Result<Vec<RunSummary>, EngineError> {
        let prefix = prefix.to_string();
        self.call(move |client| {
            let rows = client
                .query(
                    "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, reference_hash
                     FROM runs WHERE run_id LIKE $1 || '%' ORDER BY started_at DESC, seq DESC",
                    &[&prefix],
                )
                .map_err(map_error)?;
            Ok(rows.iter().map(run_summary).collect())
        })
    }

    fn model_runs(&self, run_id: &str) -> Result<Vec<ModelRunRecord>, EngineError> {
        let run_id = run_id.to_string();
        self.call(move |client| {
            let rows = client
                .query(
                    &format!(
                        "SELECT {MODEL_RUN_COLUMNS} FROM model_runs WHERE run_id = $1 ORDER BY model_id"
                    ),
                    &[&run_id],
                )
                .map_err(map_error)?;
            Ok(rows.iter().map(model_run_from_row).collect())
        })
    }

    fn seed_runs(&self, run_id: &str) -> Result<Vec<SeedRunRecord>, EngineError> {
        let run_id = run_id.to_string();
        self.call(move |client| {
            let rows = client
                .query(
                    "SELECT run_id, name, status, target, attempts_json, error, error_category, started_at, finished_at
                     FROM seed_runs WHERE run_id = $1 ORDER BY name",
                    &[&run_id],
                )
                .map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|row| {
                    let status: String = row.get(2);
                    let attempts: Option<String> = row.get(4);
                    SeedRunRecord {
                        run_id: row.get(0),
                        name: row.get(1),
                        status: parse_status(&status),
                        target: row.get(3),
                        attempts: attempts
                            .and_then(|json| serde_json::from_str(&json).ok())
                            .unwrap_or_default(),
                        error: row.get(5),
                        error_category: row.get(6),
                        started_at: row.get(7),
                        finished_at: row.get(8),
                    }
                })
                .collect())
        })
    }

    fn test_runs(&self, run_id: &str) -> Result<Vec<TestRunRecord>, EngineError> {
        let run_id = run_id.to_string();
        self.call(move |client| {
            let rows = client
                .query(
                    "SELECT run_id, test_id, status, row_count, query_id, error, error_category, started_at, finished_at
                     FROM test_runs WHERE run_id = $1 ORDER BY test_id",
                    &[&run_id],
                )
                .map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|row| {
                    let status: String = row.get(2);
                    TestRunRecord {
                        run_id: row.get(0),
                        test_id: row.get(1),
                        status: parse_status(&status),
                        row_count: row.get::<usize, i64>(3) as u64,
                        query_id: row.get(4),
                        error: row.get(5),
                        error_category: row.get(6),
                        started_at: row.get(7),
                        finished_at: row.get(8),
                    }
                })
                .collect())
        })
    }

    fn record_materialized(&self, record: &MaterializedRecord) -> Result<(), EngineError> {
        let record = record.clone();
        self.call(move |client| {
            let detail = record
                .detail
                .as_ref()
                .map(|detail| serde_json::to_string(detail).unwrap_or_default());
            let contract = record
                .contract
                .as_ref()
                .map(|contract| serde_json::to_string(contract).unwrap_or_default());
            let effective_key = record
                .effective_key
                .as_ref()
                .map(|key| serde_json::to_string(key).unwrap_or_default());
            let environment = record.environment.clone().unwrap_or_default();
            // Ordered upsert: only applied when the incoming record is not
            // older than what is stored — a later-materialised run physically
            // overwrote the table, so an earlier run finishing afterwards
            // must not regress the row. Equal timestamps allow idempotent
            // re-records from the same run.
            client
                .execute(
                    "INSERT INTO model_versions
                     (model_id, environment, version_hash, sql_hash, config_hash, contract_hash,
                      dependency_hash, source_state_hash, compiler_version, target_hash, target,
                      run_id, materialized_at, incremental_strategy, incremental_key, version_detail,
                      adapter, output_identity, contract_json, effective_key_json)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
                     ON CONFLICT (model_id, environment) DO UPDATE SET
                        version_hash = EXCLUDED.version_hash,
                        sql_hash = EXCLUDED.sql_hash,
                        config_hash = EXCLUDED.config_hash,
                        contract_hash = EXCLUDED.contract_hash,
                        dependency_hash = EXCLUDED.dependency_hash,
                        source_state_hash = EXCLUDED.source_state_hash,
                        compiler_version = EXCLUDED.compiler_version,
                        target_hash = EXCLUDED.target_hash,
                        target = EXCLUDED.target,
                        run_id = EXCLUDED.run_id,
                        materialized_at = EXCLUDED.materialized_at,
                        incremental_strategy = EXCLUDED.incremental_strategy,
                        incremental_key = EXCLUDED.incremental_key,
                        version_detail = EXCLUDED.version_detail,
                        adapter = EXCLUDED.adapter,
                        output_identity = EXCLUDED.output_identity,
                        contract_json = EXCLUDED.contract_json,
                        effective_key_json = EXCLUDED.effective_key_json
                     WHERE EXCLUDED.materialized_at >= model_versions.materialized_at",
                    &[
                        &record.model_id,
                        &environment,
                        &record.version.hash,
                        &record.version.sql_hash,
                        &record.version.config_hash,
                        &record.version.contract_hash,
                        &record.version.dependency_hash,
                        &record.version.source_state_hash,
                        &record.version.compiler_version,
                        &record.version.target_hash,
                        &record.target,
                        &record.run_id,
                        &record.materialized_at,
                        &record.incremental_strategy,
                        &record.incremental_key,
                        &detail,
                        &record.adapter,
                        &record.output_identity,
                        &contract,
                        &effective_key,
                    ],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn materialized_version(
        &self,
        model_id: &str,
        environment: Option<&str>,
    ) -> Result<Option<MaterializedRecord>, EngineError> {
        let model_id = model_id.to_string();
        let environment = environment.unwrap_or("").to_string();
        self.call(move |client| {
            let row = client
                .query_opt(
                    &format!(
                        "SELECT {MATERIALIZED_COLUMNS} FROM model_versions WHERE model_id = $1 AND environment = $2"
                    ),
                    &[&model_id, &environment],
                )
                .map_err(map_error)?;
            Ok(row.as_ref().map(materialized_from_row))
        })
    }

    fn materialized_by_hash(
        &self,
        version_hash: &str,
    ) -> Result<Vec<MaterializedRecord>, EngineError> {
        let version_hash = version_hash.to_string();
        self.call(move |client| {
            let rows = client
                .query(
                    &format!(
                        "SELECT {MATERIALIZED_COLUMNS} FROM model_versions WHERE version_hash = $1"
                    ),
                    &[&version_hash],
                )
                .map_err(map_error)?;
            Ok(rows.iter().map(materialized_from_row).collect())
        })
    }

    fn materialized_in(
        &self,
        environment: Option<&str>,
    ) -> Result<Vec<MaterializedRecord>, EngineError> {
        let environment = environment.unwrap_or("").to_string();
        self.call(move |client| {
            let rows = client
                .query(
                    &format!(
                        "SELECT {MATERIALIZED_COLUMNS} FROM model_versions WHERE environment = $1 \
                         ORDER BY model_id"
                    ),
                    &[&environment],
                )
                .map_err(map_error)?;
            Ok(rows.iter().map(materialized_from_row).collect())
        })
    }

    fn record_promotion(
        &self,
        record: &crate::promotion::PromotionRecord,
    ) -> Result<(), EngineError> {
        let record = record.clone();
        self.call(move |client| {
            let json = serde_json::to_string(&record)
                .map_err(|error| EngineError::State(error.to_string()))?;
            let merged = record.merged as i64;
            let dry_run = record.dry_run as i64;
            client
                .execute(
                    "INSERT INTO promotions
                     (promotion_id, candidate_ref, target_ref, merged, dry_run, record_json, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (promotion_id) DO UPDATE SET
                        candidate_ref = EXCLUDED.candidate_ref,
                        target_ref = EXCLUDED.target_ref,
                        merged = EXCLUDED.merged,
                        dry_run = EXCLUDED.dry_run,
                        record_json = EXCLUDED.record_json,
                        created_at = EXCLUDED.created_at",
                    &[
                        &record.promotion_id,
                        &record.candidate_ref,
                        &record.target_ref,
                        &merged,
                        &dry_run,
                        &json,
                        &record.timestamp,
                    ],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn promotions(&self) -> Result<Vec<crate::promotion::PromotionRecord>, EngineError> {
        self.call(|client| {
            let rows = client
                .query(
                    "SELECT record_json FROM promotions ORDER BY created_at DESC, seq DESC",
                    &[],
                )
                .map_err(map_error)?;
            rows.iter()
                .map(|row| {
                    let json: String = row.get(0);
                    serde_json::from_str(&json)
                        .map_err(|error| EngineError::State(error.to_string()))
                })
                .collect()
        })
    }

    fn set_watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
        value: &str,
        run_id: &str,
    ) -> Result<(), EngineError> {
        let model_id = model_id.to_string();
        let environment = environment.unwrap_or("").to_string();
        let value = value.to_string();
        let run_id = run_id.to_string();
        self.call(move |client| {
            let updated_at = now_rfc3339();
            // Ordered by run generation: a watermark is an observation a run
            // made, so the later-started run supersedes. Writers without a
            // `runs` row fall back to update order; a stored run that no
            // longer resolves loses to any identifiable writer.
            client
                .execute(
                    "INSERT INTO incremental_state
                     (model_id, environment, last_value, run_id, updated_at)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (model_id, environment) DO UPDATE SET
                        last_value = EXCLUDED.last_value,
                        run_id = EXCLUDED.run_id,
                        updated_at = EXCLUDED.updated_at
                     WHERE COALESCE(
                               (SELECT started_at FROM runs WHERE run_id = EXCLUDED.run_id),
                               EXCLUDED.updated_at
                           ) >= COALESCE(
                               (SELECT started_at FROM runs WHERE run_id = incremental_state.run_id),
                               ''
                           )",
                    &[&model_id, &environment, &value, &run_id, &updated_at],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
    ) -> Result<Option<String>, EngineError> {
        let model_id = model_id.to_string();
        let environment = environment.unwrap_or("").to_string();
        self.call(move |client| {
            let row = client
                .query_opt(
                    "SELECT last_value FROM incremental_state WHERE model_id = $1 AND environment = $2",
                    &[&model_id, &environment],
                )
                .map_err(map_error)?;
            Ok(row.and_then(|row| row.get::<usize, Option<String>>(0)))
        })
    }

    fn record_seed(&self, record: &SeedRecord) -> Result<(), EngineError> {
        let record = record.clone();
        self.call(move |client| {
            let environment = record.environment.clone().unwrap_or_default();
            client
                .execute(
                    "INSERT INTO seed_loads
                     (name, environment, content_hash, target, run_id, loaded_at)
                     VALUES ($1, $2, $3, $4, $5, $6)
                     ON CONFLICT (name, environment) DO UPDATE SET
                        content_hash = EXCLUDED.content_hash,
                        target = EXCLUDED.target,
                        run_id = EXCLUDED.run_id,
                        loaded_at = EXCLUDED.loaded_at
                     WHERE EXCLUDED.loaded_at >= seed_loads.loaded_at",
                    &[
                        &record.name,
                        &environment,
                        &record.content_hash,
                        &record.target,
                        &record.run_id,
                        &record.loaded_at,
                    ],
                )
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn seed_state(
        &self,
        name: &str,
        environment: Option<&str>,
    ) -> Result<Option<SeedRecord>, EngineError> {
        let name = name.to_string();
        let environment = environment.unwrap_or("").to_string();
        self.call(move |client| {
            let row = client
                .query_opt(
                    "SELECT name, environment, content_hash, target, run_id, loaded_at
                     FROM seed_loads WHERE name = $1 AND environment = $2",
                    &[&name, &environment],
                )
                .map_err(map_error)?;
            Ok(row.as_ref().map(seed_from_row))
        })
    }

    fn seeds_in(&self, environment: Option<&str>) -> Result<Vec<SeedRecord>, EngineError> {
        let environment = environment.unwrap_or("").to_string();
        self.call(move |client| {
            let rows = client
                .query(
                    "SELECT name, environment, content_hash, target, run_id, loaded_at
                     FROM seed_loads WHERE environment = $1 ORDER BY name",
                    &[&environment],
                )
                .map_err(map_error)?;
            Ok(rows.iter().map(seed_from_row).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ModelRunRecord, RunRecord, StoredPlan};

    /// A Postgres to run against is opt-in: `PHLO_TEST_POSTGRES_URL`.
    fn connect() -> Option<PostgresStateStore> {
        let url = std::env::var("PHLO_TEST_POSTGRES_URL").ok()?;
        let store = PostgresStateStore::connect(&url).expect("postgres connects");
        store
            .call(|client| {
                for table in [
                    "runs",
                    "model_runs",
                    "seed_runs",
                    "test_runs",
                    "model_versions",
                    "incremental_state",
                    "seed_loads",
                    "promotions",
                ] {
                    client
                        .execute(&format!("DELETE FROM {table}"), &[])
                        .map_err(map_error)?;
                }
                Ok(())
            })
            .expect("tables clear");
        Some(store)
    }

    fn run(run_id: &str, environment: Option<&str>) -> (RunRecord, StoredPlan) {
        (
            RunRecord {
                run_id: run_id.to_string(),
                plan_id: "plan".to_string(),
                environment: environment.map(str::to_string),
                reference_hash: None,
                started_at: "2026-01-01T00:00:00Z".to_string(),
                finished_at: None,
                status: ExecutionStatus::Running,
                model_count: 1,
                failed_count: 0,
            },
            StoredPlan {
                plan_id: "plan".to_string(),
                environment: environment.map(str::to_string),
                models: Vec::new(),
                seeds: Vec::new(),
                tests: Vec::new(),
            },
        )
    }

    #[test]
    fn postgres_roundtrip_run_and_versions() {
        let Some(store) = connect() else {
            return;
        };
        let (run, plan) = run("run-pg-1", Some("ci/x"));
        store.start_run(&run, &plan).expect("start");
        store
            .record_model(&ModelRunRecord {
                run_id: "run-pg-1".to_string(),
                model_id: "m.a".to_string(),
                materialization: "table".to_string(),
                action: "build".to_string(),
                status: ExecutionStatus::Passed,
                started_at: "s".to_string(),
                finished_at: "f".to_string(),
                sql_hash: "sql".to_string(),
                target: "cat.m.a".to_string(),
                desired_version: "v1".to_string(),
                attempts: Vec::new(),
                query_id: None,
                error: None,
                error_category: None,
            })
            .expect("model");
        store
            .finish_run("run-pg-1", ExecutionStatus::Passed, "f", 0)
            .expect("finish");
        let stored = store.run("run-pg-1").expect("read").expect("run exists");
        assert_eq!(stored.record.status, ExecutionStatus::Passed);
        assert!(stored.plan.is_some());
        let models = store.model_runs("run-pg-1").expect("model runs");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].status, ExecutionStatus::Passed);

        store
            .record_materialized(&MaterializedRecord {
                model_id: "m.a".to_string(),
                environment: Some("ci/x".to_string()),
                version: ModelVersion {
                    hash: "v1".to_string(),
                    ..Default::default()
                },
                detail: None,
                target: "cat.m.a".to_string(),
                incremental_strategy: None,
                incremental_key: None,
                adapter: Some("trino".to_string()),
                output_identity: Some("snap:1".to_string()),
                contract: None,
                effective_key: Some(vec![vec!["sample_id".to_string()]]),
                run_id: "run-pg-1".to_string(),
                materialized_at: "2026-01-01T00:00:01Z".to_string(),
            })
            .expect("materialized");
        let record = store
            .materialized_version("m.a", Some("ci/x"))
            .expect("version")
            .expect("record");
        assert_eq!(record.version.hash, "v1");
        assert_eq!(record.adapter.as_deref(), Some("trino"));
        assert_eq!(record.output_identity.as_deref(), Some("snap:1"));
        assert_eq!(
            record.effective_key.as_deref(),
            Some(&[vec!["sample_id".to_string()]][..])
        );
        assert_eq!(store.materialized_by_hash("v1").expect("by hash").len(), 1);

        // Same-key writes are ordered by materialisation time: a later
        // materialisation supersedes, an earlier one arriving after it is
        // dropped rather than regressing shared state.
        let mut record2 = record.clone();
        record2.version.hash = "v2".to_string();
        record2.adapter = Some("duckdb".to_string());
        record2.materialized_at = "2026-01-01T00:00:02Z".to_string();
        store.record_materialized(&record2).expect("second write");
        let record = store
            .materialized_version("m.a", Some("ci/x"))
            .expect("version")
            .expect("record");
        assert_eq!(record.version.hash, "v2");
        assert_eq!(record.adapter.as_deref(), Some("duckdb"));

        let mut stale = record2.clone();
        stale.version.hash = "v0".to_string();
        stale.materialized_at = "2026-01-01T00:00:00Z".to_string();
        store
            .record_materialized(&stale)
            .expect("stale write accepted");
        let record = store
            .materialized_version("m.a", Some("ci/x"))
            .expect("version")
            .expect("record");
        assert_eq!(record.version.hash, "v2", "the stale write must lose");
    }
}
