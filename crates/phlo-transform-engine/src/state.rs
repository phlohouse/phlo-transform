//! Operational run state.
//!
//! This is queryable run history, not the content-addressed desired-state
//! engine of Phase 3. The trait is intentionally small so other backends can
//! be added later.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;

use phlo_transform_core::{ModelVersion, VersionDetail};

use crate::error::EngineError;
use crate::events::ExecutionStatus;

/// A single run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunRecord {
    pub run_id: String,
    pub plan_id: String,
    pub environment: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: ExecutionStatus,
    pub model_count: usize,
    pub failed_count: usize,
}

/// The plan a run executed, persisted so `--resume`/`--retry-failed` can
/// reconstruct the run's work without re-planning.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredPlan {
    pub plan_id: String,
    pub environment: Option<String>,
    pub models: Vec<StoredPlanModel>,
    pub seeds: Vec<StoredPlanSeed>,
    pub tests: Vec<StoredPlanTest>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredPlanModel {
    pub id: String,
    pub target: String,
    /// The plan action as its stable string (`build`, `skip`, `cached`,
    /// `unknown`).
    pub action: String,
    pub desired_version: String,
    #[serde(default)]
    pub full_rebuild: bool,
    #[serde(default)]
    pub watermark: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredPlanSeed {
    pub name: String,
    pub target: String,
    pub path: std::path::PathBuf,
    pub action: String,
    pub desired_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredPlanTest {
    pub id: String,
    pub targets: Vec<String>,
    pub sources: Vec<String>,
}

/// A run loaded back from the store: its record plus the persisted plan.
#[derive(Clone, Debug)]
pub struct StoredRun {
    pub record: RunRecord,
    /// `None` for runs recorded before plan persistence existed.
    pub plan: Option<StoredPlan>,
}

/// A single model execution within a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelRunRecord {
    pub run_id: String,
    pub model_id: String,
    pub materialization: String,
    /// The plan action that produced this execution (`build`, `skip`,
    /// `cached`, `unknown`).
    pub action: String,
    pub status: ExecutionStatus,
    pub started_at: String,
    /// Empty while the model is still running.
    pub finished_at: String,
    pub sql_hash: String,
    pub target: String,
    /// The desired version this execution was building.
    pub desired_version: String,
    /// Every attempt made, with its own failure classification.
    pub attempts: Vec<crate::failure::Attempt>,
    pub query_id: Option<String>,
    pub error: Option<String>,
    /// Stable `FailureCategory` code for the terminal failure.
    pub error_category: Option<String>,
}

/// A single seed execution within a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SeedRunRecord {
    pub run_id: String,
    pub name: String,
    pub status: ExecutionStatus,
    pub target: String,
    pub attempts: Vec<crate::failure::Attempt>,
    pub error: Option<String>,
    pub error_category: Option<String>,
    pub started_at: String,
    pub finished_at: String,
}

/// A single test execution within a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TestRunRecord {
    pub run_id: String,
    pub test_id: String,
    pub status: ExecutionStatus,
    pub row_count: u64,
    pub query_id: Option<String>,
    pub error: Option<String>,
    /// Stable `FailureCategory` code (`test`, `adapter`, ...).
    pub error_category: Option<String>,
    pub started_at: String,
    pub finished_at: String,
}

/// A compact run summary for history queries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunSummary {
    pub run_id: String,
    pub plan_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: ExecutionStatus,
    pub model_count: usize,
    pub failed_count: usize,
}

/// The version recorded against a physical materialisation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MaterializedRecord {
    pub model_id: String,
    pub environment: Option<String>,
    pub version: ModelVersion,
    /// The named version inputs at materialisation time — which dependency
    /// versions and source states the recorded version was derived from.
    /// `None` for rows written before detail was persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<VersionDetail>,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incremental_strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incremental_key: Option<String>,
    pub run_id: String,
    pub materialized_at: String,
}

/// A seed load recorded against an environment.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SeedRecord {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The seed CSV's content hash at load time.
    pub content_hash: String,
    pub target: String,
    pub run_id: String,
    pub loaded_at: String,
}

/// Persists run history and in-progress execution state.
///
/// The store is written incrementally — the run and each node's status are
/// recorded as they transition — so a killed process leaves an accurate
/// "still running" trace rather than a silent gap or a false success.
pub trait StateStore: Send + Sync {
    /// Record a run starting, with the plan it is executing.
    fn start_run(&self, run: &RunRecord, plan: &StoredPlan) -> Result<(), EngineError>;
    /// Re-open an interrupted run for continuation (`--resume`): status back
    /// to `running`, `finished_at` cleared, original `started_at` kept.
    fn reopen_run(&self, run_id: &str) -> Result<(), EngineError>;
    fn finish_run(
        &self,
        run_id: &str,
        status: ExecutionStatus,
        finished_at: &str,
        failed_count: usize,
    ) -> Result<(), EngineError>;
    /// Upsert a model's execution record — called on each transition.
    fn record_model(&self, record: &ModelRunRecord) -> Result<(), EngineError>;
    fn record_seed_run(&self, record: &SeedRunRecord) -> Result<(), EngineError>;
    fn record_test(&self, record: &TestRunRecord) -> Result<(), EngineError>;
    fn runs(&self) -> Result<Vec<RunSummary>, EngineError>;
    /// The most recent run for an environment, if any.
    fn latest_run(&self, environment: Option<&str>) -> Result<Option<RunSummary>, EngineError>;
    /// A run plus its persisted plan, by id.
    fn run(&self, run_id: &str) -> Result<Option<StoredRun>, EngineError>;
    /// Runs whose id starts with `prefix` — for `--resume <short-id>`.
    fn find_runs(&self, prefix: &str) -> Result<Vec<RunSummary>, EngineError>;
    /// Every model execution record of a run.
    fn model_runs(&self, run_id: &str) -> Result<Vec<ModelRunRecord>, EngineError>;
    /// Every seed execution record of a run.
    fn seed_runs(&self, run_id: &str) -> Result<Vec<SeedRunRecord>, EngineError>;
    /// Every test execution record of a run.
    fn test_runs(&self, run_id: &str) -> Result<Vec<TestRunRecord>, EngineError>;

    /// Record the version attached to a successful materialisation.
    fn record_materialized(&self, record: &MaterializedRecord) -> Result<(), EngineError>;
    /// The materialised version for a model in an environment.
    fn materialized_version(
        &self,
        model_id: &str,
        environment: Option<&str>,
    ) -> Result<Option<MaterializedRecord>, EngineError>;
    /// All materialisations of a given version hash (for cache reuse checks).
    fn materialized_by_hash(
        &self,
        version_hash: &str,
    ) -> Result<Vec<MaterializedRecord>, EngineError>;
    /// Every materialised model in an environment — used by branch diffs to
    /// find datasets that exist on a ref but are no longer in the workspace.
    fn materialized_in(
        &self,
        environment: Option<&str>,
    ) -> Result<Vec<MaterializedRecord>, EngineError>;

    /// Record a promotion for audit and later APIs.
    fn record_promotion(
        &self,
        record: &crate::promotion::PromotionRecord,
    ) -> Result<(), EngineError>;
    /// Recorded promotions, newest first.
    fn promotions(&self) -> Result<Vec<crate::promotion::PromotionRecord>, EngineError>;

    /// Record a successful time-window watermark. Only called on success.
    fn set_watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
        value: &str,
        run_id: &str,
    ) -> Result<(), EngineError>;

    /// The last successful time-window watermark for a model/environment.
    fn watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
    ) -> Result<Option<String>, EngineError>;

    /// Record a successful seed load.
    fn record_seed(&self, record: &SeedRecord) -> Result<(), EngineError>;

    /// The latest seed load for an environment.
    fn seed_state(
        &self,
        name: &str,
        environment: Option<&str>,
    ) -> Result<Option<SeedRecord>, EngineError>;
    /// Every seed load recorded for an environment.
    fn seeds_in(&self, environment: Option<&str>) -> Result<Vec<SeedRecord>, EngineError>;
}

/// SQLite-backed local state store.
pub struct SqliteStateStore {
    connection: Mutex<Connection>,
}

impl SqliteStateStore {
    /// Open or create a state database at the given path.
    pub fn open(path: &Path) -> Result<Self, EngineError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| EngineError::State(error.to_string()))?;
        }
        let connection =
            Connection::open(path).map_err(|error| EngineError::State(error.to_string()))?;
        Self::from_connection(connection)
    }

    /// Open an in-memory database (used by tests).
    pub fn in_memory() -> Result<Self, EngineError> {
        let connection =
            Connection::open_in_memory().map_err(|error| EngineError::State(error.to_string()))?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, EngineError> {
        connection
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS runs (
                    run_id TEXT PRIMARY KEY,
                    plan_id TEXT NOT NULL,
                    environment TEXT,
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    status TEXT NOT NULL,
                    model_count INTEGER NOT NULL,
                    failed_count INTEGER NOT NULL,
                    plan_json TEXT
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
                    row_count INTEGER NOT NULL,
                    query_id TEXT,
                    error TEXT,
                    error_category TEXT,
                    started_at TEXT NOT NULL,
                    finished_at TEXT NOT NULL,
                    PRIMARY KEY (run_id, test_id)
                );
                CREATE TABLE IF NOT EXISTS model_versions (
                    model_id TEXT NOT NULL,
                    environment TEXT NOT NULL,
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
                    PRIMARY KEY (model_id, environment)
                );
                CREATE TABLE IF NOT EXISTS incremental_state (
                    model_id TEXT NOT NULL,
                    environment TEXT NOT NULL,
                    last_value TEXT,
                    run_id TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY (model_id, environment)
                );
                CREATE TABLE IF NOT EXISTS seed_loads (
                    name TEXT NOT NULL,
                    environment TEXT NOT NULL,
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
                    merged INTEGER NOT NULL,
                    dry_run INTEGER NOT NULL,
                    record_json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                ",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        // Best-effort migration for databases created before these columns.
        let _ = connection.execute(
            "ALTER TABLE model_versions ADD COLUMN incremental_strategy TEXT",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE model_versions ADD COLUMN incremental_key TEXT",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE model_versions ADD COLUMN version_detail TEXT",
            [],
        );
        // Run-progress columns added for resumable runs.
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN plan_json TEXT", []);
        for column in [
            "ALTER TABLE model_runs ADD COLUMN action TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE model_runs ADD COLUMN desired_version TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE model_runs ADD COLUMN attempts_json TEXT",
            "ALTER TABLE model_runs ADD COLUMN error_category TEXT",
            "ALTER TABLE test_runs ADD COLUMN error_category TEXT",
        ] {
            let _ = connection.execute(column, []);
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, EngineError> {
        self.connection
            .lock()
            .map_err(|_| EngineError::State("state database lock poisoned".to_string()))
    }
}

fn status_str(status: ExecutionStatus) -> &'static str {
    status.label()
}

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

/// The `model_runs` read-back column list, in order.
const MODEL_RUN_COLUMNS: &str = "run_id, model_id, materialization, status, started_at, \
     finished_at, sql_hash, target, query_id, error, action, desired_version, attempts_json, \
     error_category";

fn model_run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelRunRecord> {
    let status: String = row.get(3)?;
    let attempts: Option<String> = row.get(12)?;
    Ok(ModelRunRecord {
        run_id: row.get(0)?,
        model_id: row.get(1)?,
        materialization: row.get(2)?,
        status: parse_status(&status),
        started_at: row.get(4)?,
        finished_at: row.get(5)?,
        sql_hash: row.get(6)?,
        target: row.get(7)?,
        query_id: row.get(8)?,
        error: row.get(9)?,
        action: row.get(10)?,
        desired_version: row.get(11)?,
        attempts: attempts
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
        error_category: row.get(13)?,
    })
}

impl StateStore for SqliteStateStore {
    fn start_run(&self, run: &RunRecord, plan: &StoredPlan) -> Result<(), EngineError> {
        let plan_json =
            serde_json::to_string(plan).map_err(|error| EngineError::State(error.to_string()))?;
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO runs (run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, plan_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    run.run_id,
                    run.plan_id,
                    run.environment.clone().unwrap_or_default(),
                    run.started_at,
                    run.finished_at,
                    status_str(run.status),
                    run.model_count as i64,
                    run.failed_count as i64,
                    plan_json,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn reopen_run(&self, run_id: &str) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "UPDATE runs SET status = 'running', finished_at = NULL WHERE run_id = ?1",
                rusqlite::params![run_id],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn finish_run(
        &self,
        run_id: &str,
        status: ExecutionStatus,
        finished_at: &str,
        failed_count: usize,
    ) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "UPDATE runs SET status = ?2, finished_at = ?3, failed_count = ?4 WHERE run_id = ?1",
                rusqlite::params![run_id, status_str(status), finished_at, failed_count as i64],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn record_model(&self, record: &ModelRunRecord) -> Result<(), EngineError> {
        let attempts_json = serde_json::to_string(&record.attempts)
            .map_err(|error| EngineError::State(error.to_string()))?;
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO model_runs
                 (run_id, model_id, materialization, status, started_at, finished_at, sql_hash, target, query_id, error, action, desired_version, attempts_json, error_category)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                rusqlite::params![
                    record.run_id,
                    record.model_id,
                    record.materialization,
                    status_str(record.status),
                    record.started_at,
                    record.finished_at,
                    record.sql_hash,
                    record.target,
                    record.query_id,
                    record.error,
                    record.action,
                    record.desired_version,
                    attempts_json,
                    record.error_category,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn record_seed_run(&self, record: &SeedRunRecord) -> Result<(), EngineError> {
        let attempts_json = serde_json::to_string(&record.attempts)
            .map_err(|error| EngineError::State(error.to_string()))?;
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO seed_runs
                 (run_id, name, status, target, attempts_json, error, error_category, started_at, finished_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    record.run_id,
                    record.name,
                    status_str(record.status),
                    record.target,
                    attempts_json,
                    record.error,
                    record.error_category,
                    record.started_at,
                    record.finished_at,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn record_test(&self, record: &TestRunRecord) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO test_runs
                 (run_id, test_id, status, row_count, query_id, error, error_category, started_at, finished_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    record.run_id,
                    record.test_id,
                    status_str(record.status),
                    record.row_count as i64,
                    record.query_id,
                    record.error,
                    record.error_category,
                    record.started_at,
                    record.finished_at,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn runs(&self) -> Result<Vec<RunSummary>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, plan_id, started_at, finished_at, status, model_count, failed_count
                 FROM runs ORDER BY started_at DESC",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                let status: String = row.get(4)?;
                Ok(RunSummary {
                    run_id: row.get(0)?,
                    plan_id: row.get(1)?,
                    started_at: row.get(2)?,
                    finished_at: row.get(3)?,
                    status: parse_status(&status),
                    model_count: row.get::<_, i64>(5)? as usize,
                    failed_count: row.get::<_, i64>(6)? as usize,
                })
            })
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))
    }

    fn latest_run(&self, environment: Option<&str>) -> Result<Option<RunSummary>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, plan_id, started_at, finished_at, status, model_count, failed_count
                 FROM runs WHERE environment = ?1 ORDER BY started_at DESC LIMIT 1",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut rows = statement
            .query(rusqlite::params![environment.unwrap_or("")])
            .map_err(|error| EngineError::State(error.to_string()))?;
        match rows
            .next()
            .map_err(|error| EngineError::State(error.to_string()))?
        {
            Some(row) => {
                let map = |error: rusqlite::Error| EngineError::State(error.to_string());
                let status: String = row.get(4).map_err(map)?;
                Ok(Some(RunSummary {
                    run_id: row.get(0).map_err(map)?,
                    plan_id: row.get(1).map_err(map)?,
                    started_at: row.get(2).map_err(map)?,
                    finished_at: row.get(3).map_err(map)?,
                    status: parse_status(&status),
                    model_count: row.get::<_, i64>(5).map_err(map)? as usize,
                    failed_count: row.get::<_, i64>(6).map_err(map)? as usize,
                }))
            }
            None => Ok(None),
        }
    }

    fn run(&self, run_id: &str) -> Result<Option<StoredRun>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, plan_json
                 FROM runs WHERE run_id = ?1",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut rows = statement
            .query(rusqlite::params![run_id])
            .map_err(|error| EngineError::State(error.to_string()))?;
        let Some(row) = rows
            .next()
            .map_err(|error| EngineError::State(error.to_string()))?
        else {
            return Ok(None);
        };
        let map = |error: rusqlite::Error| EngineError::State(error.to_string());
        let status: String = row.get(5).map_err(map)?;
        let environment: Option<String> = row.get(2).map_err(map)?;
        let plan_json: Option<String> = row.get(8).map_err(map)?;
        Ok(Some(StoredRun {
            record: RunRecord {
                run_id: row.get(0).map_err(map)?,
                plan_id: row.get(1).map_err(map)?,
                environment: environment.filter(|value| !value.is_empty()),
                started_at: row.get(3).map_err(map)?,
                finished_at: row.get(4).map_err(map)?,
                status: parse_status(&status),
                model_count: row.get::<_, i64>(6).map_err(map)? as usize,
                failed_count: row.get::<_, i64>(7).map_err(map)? as usize,
            },
            plan: plan_json.and_then(|json| serde_json::from_str(&json).ok()),
        }))
    }

    fn find_runs(&self, prefix: &str) -> Result<Vec<RunSummary>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, plan_id, started_at, finished_at, status, model_count, failed_count
                 FROM runs WHERE run_id LIKE ?1 || '%' ORDER BY started_at DESC",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![prefix], |row| {
                let status: String = row.get(4)?;
                Ok(RunSummary {
                    run_id: row.get(0)?,
                    plan_id: row.get(1)?,
                    started_at: row.get(2)?,
                    finished_at: row.get(3)?,
                    status: parse_status(&status),
                    model_count: row.get::<_, i64>(5)? as usize,
                    failed_count: row.get::<_, i64>(6)? as usize,
                })
            })
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))
    }

    fn model_runs(&self, run_id: &str) -> Result<Vec<ModelRunRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {MODEL_RUN_COLUMNS} FROM model_runs WHERE run_id = ?1 ORDER BY model_id"
            ))
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![run_id], model_run_from_row)
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))
    }

    fn seed_runs(&self, run_id: &str) -> Result<Vec<SeedRunRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, name, status, target, attempts_json, error, error_category, started_at, finished_at
                 FROM seed_runs WHERE run_id = ?1 ORDER BY name",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![run_id], |row| {
                let status: String = row.get(2)?;
                let attempts: Option<String> = row.get(4)?;
                Ok(SeedRunRecord {
                    run_id: row.get(0)?,
                    name: row.get(1)?,
                    status: parse_status(&status),
                    target: row.get(3)?,
                    attempts: attempts
                        .and_then(|json| serde_json::from_str(&json).ok())
                        .unwrap_or_default(),
                    error: row.get(5)?,
                    error_category: row.get(6)?,
                    started_at: row.get(7)?,
                    finished_at: row.get(8)?,
                })
            })
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))
    }

    fn test_runs(&self, run_id: &str) -> Result<Vec<TestRunRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, test_id, status, row_count, query_id, error, error_category, started_at, finished_at
                 FROM test_runs WHERE run_id = ?1 ORDER BY test_id",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![run_id], |row| {
                let status: String = row.get(2)?;
                Ok(TestRunRecord {
                    run_id: row.get(0)?,
                    test_id: row.get(1)?,
                    status: parse_status(&status),
                    row_count: row.get::<_, i64>(3)? as u64,
                    query_id: row.get(4)?,
                    error: row.get(5)?,
                    error_category: row.get(6)?,
                    started_at: row.get(7)?,
                    finished_at: row.get(8)?,
                })
            })
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))
    }

    fn record_materialized(&self, record: &MaterializedRecord) -> Result<(), EngineError> {
        let connection = self.lock()?;
        let detail = record
            .detail
            .as_ref()
            .map(|detail| serde_json::to_string(detail).unwrap_or_default());
        connection
            .execute(
                "INSERT OR REPLACE INTO model_versions
                 (model_id, environment, version_hash, sql_hash, config_hash, contract_hash,
                  dependency_hash, source_state_hash, compiler_version, target_hash, target,
                  run_id, materialized_at, incremental_strategy, incremental_key, version_detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                rusqlite::params![
                    record.model_id,
                    record.environment.clone().unwrap_or_default(),
                    record.version.hash,
                    record.version.sql_hash,
                    record.version.config_hash,
                    record.version.contract_hash,
                    record.version.dependency_hash,
                    record.version.source_state_hash,
                    record.version.compiler_version,
                    record.version.target_hash,
                    record.target,
                    record.run_id,
                    record.materialized_at,
                    record.incremental_strategy,
                    record.incremental_key,
                    detail,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn materialized_version(
        &self,
        model_id: &str,
        environment: Option<&str>,
    ) -> Result<Option<MaterializedRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {MATERIALIZED_COLUMNS} FROM model_versions WHERE model_id = ?1 AND environment = ?2"
            ))
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut rows = statement
            .query(rusqlite::params![model_id, environment.unwrap_or("")])
            .map_err(|error| EngineError::State(error.to_string()))?;
        match rows
            .next()
            .map_err(|error| EngineError::State(error.to_string()))?
        {
            Some(row) => Ok(Some(
                materialized_from_row(row)
                    .map_err(|error| EngineError::State(error.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    fn materialized_by_hash(
        &self,
        version_hash: &str,
    ) -> Result<Vec<MaterializedRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {MATERIALIZED_COLUMNS} FROM model_versions WHERE version_hash = ?1"
            ))
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![version_hash], materialized_from_row)
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))
    }

    fn materialized_in(
        &self,
        environment: Option<&str>,
    ) -> Result<Vec<MaterializedRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {MATERIALIZED_COLUMNS} FROM model_versions WHERE environment = ?1 \
                 ORDER BY model_id"
            ))
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(
                rusqlite::params![environment.unwrap_or("")],
                materialized_from_row,
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))
    }

    fn record_promotion(
        &self,
        record: &crate::promotion::PromotionRecord,
    ) -> Result<(), EngineError> {
        let json =
            serde_json::to_string(record).map_err(|error| EngineError::State(error.to_string()))?;
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO promotions
                 (promotion_id, candidate_ref, target_ref, merged, dry_run, record_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    record.promotion_id,
                    record.candidate_ref,
                    record.target_ref,
                    record.merged as i64,
                    record.dry_run as i64,
                    json,
                    record.timestamp,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn promotions(&self) -> Result<Vec<crate::promotion::PromotionRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT record_json FROM promotions ORDER BY created_at DESC")
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.map(|row| {
            let json = row.map_err(|error| EngineError::State(error.to_string()))?;
            serde_json::from_str(&json).map_err(|error| EngineError::State(error.to_string()))
        })
        .collect()
    }

    fn set_watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
        value: &str,
        run_id: &str,
    ) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO incremental_state
                 (model_id, environment, last_value, run_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    model_id,
                    environment.unwrap_or(""),
                    value,
                    run_id,
                    crate::util::now_rfc3339(),
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
    ) -> Result<Option<String>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT last_value FROM incremental_state WHERE model_id = ?1 AND environment = ?2",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut rows = statement
            .query(rusqlite::params![model_id, environment.unwrap_or("")])
            .map_err(|error| EngineError::State(error.to_string()))?;
        match rows
            .next()
            .map_err(|error| EngineError::State(error.to_string()))?
        {
            Some(row) => Ok(row
                .get::<_, Option<String>>(0)
                .map_err(|error| EngineError::State(error.to_string()))?),
            None => Ok(None),
        }
    }

    fn record_seed(&self, record: &SeedRecord) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO seed_loads
                 (name, environment, content_hash, target, run_id, loaded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    record.name,
                    record.environment.clone().unwrap_or_default(),
                    record.content_hash,
                    record.target,
                    record.run_id,
                    record.loaded_at,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn seed_state(
        &self,
        name: &str,
        environment: Option<&str>,
    ) -> Result<Option<SeedRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT name, environment, content_hash, target, run_id, loaded_at
                 FROM seed_loads WHERE name = ?1 AND environment = ?2",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut rows = statement
            .query(rusqlite::params![name, environment.unwrap_or("")])
            .map_err(|error| EngineError::State(error.to_string()))?;
        match rows
            .next()
            .map_err(|error| EngineError::State(error.to_string()))?
        {
            Some(row) => {
                let env: String = row
                    .get(1)
                    .map_err(|error| EngineError::State(error.to_string()))?;
                Ok(Some(SeedRecord {
                    name: row
                        .get(0)
                        .map_err(|error| EngineError::State(error.to_string()))?,
                    environment: if env.is_empty() { None } else { Some(env) },
                    content_hash: row
                        .get(2)
                        .map_err(|error| EngineError::State(error.to_string()))?,
                    target: row
                        .get(3)
                        .map_err(|error| EngineError::State(error.to_string()))?,
                    run_id: row
                        .get(4)
                        .map_err(|error| EngineError::State(error.to_string()))?,
                    loaded_at: row
                        .get(5)
                        .map_err(|error| EngineError::State(error.to_string()))?,
                }))
            }
            None => Ok(None),
        }
    }

    fn seeds_in(&self, environment: Option<&str>) -> Result<Vec<SeedRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT name, environment, content_hash, target, run_id, loaded_at
                 FROM seed_loads WHERE environment = ?1 ORDER BY name",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![environment.unwrap_or("")], |row| {
                let env: String = row.get(1)?;
                Ok(SeedRecord {
                    name: row.get(0)?,
                    environment: if env.is_empty() { None } else { Some(env) },
                    content_hash: row.get(2)?,
                    target: row.get(3)?,
                    run_id: row.get(4)?,
                    loaded_at: row.get(5)?,
                })
            })
            .map_err(|error| EngineError::State(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))
    }
}

const MATERIALIZED_COLUMNS: &str = "model_id, environment, version_hash, sql_hash, config_hash, \
     contract_hash, dependency_hash, source_state_hash, compiler_version, target_hash, target, \
     run_id, materialized_at, incremental_strategy, incremental_key, version_detail";

fn materialized_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MaterializedRecord> {
    let environment: String = row.get(1)?;
    Ok(MaterializedRecord {
        model_id: row.get(0)?,
        environment: if environment.is_empty() {
            None
        } else {
            Some(environment)
        },
        version: ModelVersion {
            hash: row.get(2)?,
            sql_hash: row.get(3)?,
            config_hash: row.get(4)?,
            contract_hash: row.get(5)?,
            dependency_hash: row.get(6)?,
            source_state_hash: row.get(7)?,
            compiler_version: row.get(8)?,
            target_hash: row.get(9)?,
        },
        detail: row
            .get::<_, Option<String>>(15)?
            .and_then(|json| serde_json::from_str(&json).ok()),
        target: row.get(10)?,
        incremental_strategy: row.get(13)?,
        incremental_key: row.get(14)?,
        run_id: row.get(11)?,
        materialized_at: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::now_rfc3339;

    #[test]
    fn persists_and_reads_run_history() {
        let store = SqliteStateStore::in_memory().unwrap();
        let timestamp = now_rfc3339();
        let stored_plan = StoredPlan {
            plan_id: "plan-1".to_string(),
            environment: Some("ci".to_string()),
            models: vec![StoredPlanModel {
                id: "assay.raw".to_string(),
                target: "assay.raw".to_string(),
                action: "build".to_string(),
                desired_version: "v1".to_string(),
                full_rebuild: false,
                watermark: None,
            }],
            seeds: Vec::new(),
            tests: Vec::new(),
        };
        store
            .start_run(
                &RunRecord {
                    run_id: "run-1".to_string(),
                    plan_id: "plan-1".to_string(),
                    environment: Some("ci".to_string()),
                    started_at: timestamp.clone(),
                    finished_at: None,
                    status: ExecutionStatus::Running,
                    model_count: 2,
                    failed_count: 0,
                },
                &stored_plan,
            )
            .unwrap();
        store
            .record_model(&ModelRunRecord {
                run_id: "run-1".to_string(),
                model_id: "assay.raw".to_string(),
                materialization: "view".to_string(),
                action: "build".to_string(),
                status: ExecutionStatus::Passed,
                started_at: timestamp.clone(),
                finished_at: timestamp.clone(),
                sql_hash: "abc".to_string(),
                target: "assay.raw".to_string(),
                desired_version: "v1".to_string(),
                attempts: Vec::new(),
                query_id: Some("q1".to_string()),
                error: None,
                error_category: None,
            })
            .unwrap();
        store
            .finish_run("run-1", ExecutionStatus::Passed, &timestamp, 0)
            .unwrap();

        let runs = store.runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "run-1");
        assert_eq!(runs[0].status, ExecutionStatus::Passed);

        // The persisted plan and per-model progress round-trip for resume.
        let stored = store.run("run-1").unwrap().expect("run exists");
        assert_eq!(stored.plan, Some(stored_plan));
        let models = store.model_runs("run-1").unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].desired_version, "v1");
        assert_eq!(store.find_runs("run").unwrap().len(), 1);
        assert!(store.find_runs("nope").unwrap().is_empty());
    }
}
