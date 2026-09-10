//! Operational run state.
//!
//! This is queryable run history, not the content-addressed desired-state
//! engine of Phase 3. The trait is intentionally small so other backends can
//! be added later.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;

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
                ",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
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
