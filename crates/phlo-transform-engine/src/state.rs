//! Operational run state.
//!
//! This is queryable run history, not the content-addressed desired-state
//! engine of Phase 3. The trait is intentionally small so other backends can
//! be added later.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;

use phlo_transform_core::ModelVersion;

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

/// A single model execution within a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelRunRecord {
    pub run_id: String,
    pub model_id: String,
    pub materialization: String,
    pub status: ExecutionStatus,
    pub started_at: String,
    pub finished_at: String,
    pub sql_hash: String,
    pub target: String,
    pub query_id: Option<String>,
    pub error: Option<String>,
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

/// Persists run history.
pub trait StateStore: Send + Sync {
    fn start_run(&self, run: &RunRecord) -> Result<(), EngineError>;
    fn finish_run(
        &self,
        run_id: &str,
        status: ExecutionStatus,
        finished_at: &str,
    ) -> Result<(), EngineError>;
    fn record_model(&self, record: &ModelRunRecord) -> Result<(), EngineError>;
    fn record_test(&self, record: &TestRunRecord) -> Result<(), EngineError>;
    fn runs(&self) -> Result<Vec<RunSummary>, EngineError>;
    /// The most recent run for an environment, if any.
    fn latest_run(&self, environment: Option<&str>) -> Result<Option<RunSummary>, EngineError>;

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
                    failed_count INTEGER NOT NULL
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
                    PRIMARY KEY (run_id, model_id)
                );
                CREATE TABLE IF NOT EXISTS test_runs (
                    run_id TEXT NOT NULL,
                    test_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    row_count INTEGER NOT NULL,
                    query_id TEXT,
                    error TEXT,
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
        "blocked" => ExecutionStatus::Blocked,
        "cancelled" => ExecutionStatus::Cancelled,
        "running" => ExecutionStatus::Running,
        "ready" => ExecutionStatus::Ready,
        _ => ExecutionStatus::Pending,
    }
}

impl StateStore for SqliteStateStore {
    fn start_run(&self, run: &RunRecord) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO runs (run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    run.run_id,
                    run.plan_id,
                    run.environment,
                    run.started_at,
                    run.finished_at,
                    status_str(run.status),
                    run.model_count as i64,
                    run.failed_count as i64,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn finish_run(
        &self,
        run_id: &str,
        status: ExecutionStatus,
        finished_at: &str,
    ) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "UPDATE runs SET status = ?2, finished_at = ?3 WHERE run_id = ?1",
                rusqlite::params![run_id, status_str(status), finished_at],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn record_model(&self, record: &ModelRunRecord) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO model_runs
                 (run_id, model_id, materialization, status, started_at, finished_at, sql_hash, target, query_id, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                 (run_id, test_id, status, row_count, query_id, error, started_at, finished_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    record.run_id,
                    record.test_id,
                    status_str(record.status),
                    record.row_count as i64,
                    record.query_id,
                    record.error,
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

    fn record_materialized(&self, record: &MaterializedRecord) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO model_versions
                 (model_id, environment, version_hash, sql_hash, config_hash, contract_hash,
                  dependency_hash, source_state_hash, compiler_version, target_hash, target,
                  run_id, materialized_at, incremental_strategy, incremental_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
}

const MATERIALIZED_COLUMNS: &str = "model_id, environment, version_hash, sql_hash, config_hash, \
     contract_hash, dependency_hash, source_state_hash, compiler_version, target_hash, target, \
     run_id, materialized_at, incremental_strategy, incremental_key";

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
        store
            .start_run(&RunRecord {
                run_id: "run-1".to_string(),
                plan_id: "plan-1".to_string(),
                environment: Some("ci".to_string()),
                started_at: timestamp.clone(),
                finished_at: None,
                status: ExecutionStatus::Running,
                model_count: 2,
                failed_count: 0,
            })
            .unwrap();
        store
            .record_model(&ModelRunRecord {
                run_id: "run-1".to_string(),
                model_id: "assay.raw".to_string(),
                materialization: "view".to_string(),
                status: ExecutionStatus::Passed,
                started_at: timestamp.clone(),
                finished_at: timestamp.clone(),
                sql_hash: "abc".to_string(),
                target: "assay.raw".to_string(),
                query_id: Some("q1".to_string()),
                error: None,
            })
            .unwrap();
        store
            .finish_run("run-1", ExecutionStatus::Passed, &timestamp)
            .unwrap();

        let runs = store.runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "run-1");
        assert_eq!(runs[0].status, ExecutionStatus::Passed);
    }
}
