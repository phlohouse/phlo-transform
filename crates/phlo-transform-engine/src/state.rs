//! Operational run state.
//!
//! This is queryable run history, not the content-addressed desired-state
//! engine of Phase 3. The trait is intentionally small so other backends can
//! be added later.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;

use phlo_transform_core::semantic::ModelContract;
use phlo_transform_core::{ModelVersion, VersionDetail};

use crate::error::EngineError;
use crate::events::ExecutionStatus;

/// A single run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunRecord {
    pub run_id: String,
    pub plan_id: String,
    pub environment: Option<String>,
    /// The Nessie commit the environment reference resolved to when the run
    /// started — the commit this run's evidence actually validated. `None`
    /// for runs outside a Nessie reference flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_hash: Option<String>,
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
    /// Environment label the run was recorded under (`None` = default).
    pub environment: Option<String>,
    /// The Nessie commit this run validated, when the run was bound to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_hash: Option<String>,
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
    /// The adapter that produced the materialisation — execution semantics
    /// differ across adapters, so a version hash alone is not proof the same
    /// bytes are there. `None` for rows recorded before adapter identity was
    /// tracked; such rows cannot back a cross-environment cache hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The strong physical identity of `target` captured right after the
    /// write — the adapter's `output_identity` (an Iceberg snapshot id, or a
    /// comparable provable identity). `None` for legacy rows or adapters
    /// that cannot prove physical identity; such rows cannot back a
    /// cross-environment cache hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_identity: Option<String>,
    /// The model's contract at materialisation time — lets promotion diff
    /// the candidate's contract against what the base environment recorded
    /// instead of comparing opaque contract hashes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<ModelContract>,
    /// The effective row-identity key at materialisation time — the union
    /// of the `key` incremental strategy's columns and each unique
    /// assertion's columns, one set per claim. `Some(vec![])` means the
    /// record proves the model had no key; `None` means nothing about the
    /// key was persisted (rows written before the column existed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_key: Option<Vec<Vec<String>>>,
    pub run_id: String,
    pub materialized_at: String,
}

impl MaterializedRecord {
    /// What this record proves about the row-identity key it was
    /// materialised with.
    ///
    /// - a persisted `effective_key` (including an empty set — recorded
    ///   keyless) is known evidence;
    /// - a legacy row with an incremental `key` strategy reconstructs its
    ///   columns: before `effective_key` existed that strategy was the only
    ///   key concept state recorded;
    /// - anything else is `Unknown` — its historical key cannot be told
    ///   from "no key", and promotion evidence must not treat it as
    ///   keyless.
    pub fn recorded_key(&self) -> crate::contracts::RecordedKey {
        use crate::contracts::RecordedKey;
        if let Some(key) = &self.effective_key {
            let mut key = key.clone();
            for claim in &mut key {
                claim.sort();
            }
            key.sort();
            return RecordedKey::Known((!key.is_empty()).then_some(key));
        }
        if self.incremental_strategy.as_deref() == Some("key") {
            if let Some(claim) = self
                .incremental_key
                .as_deref()
                .and_then(incremental_key_claim)
            {
                return RecordedKey::Known(Some(claim));
            }
        }
        RecordedKey::Unknown
    }
}

/// The canonical key claim a persisted `incremental_key` string encodes —
/// the comma-joined columns, sorted as `effective_key` claims are. `None`
/// for an empty column list.
pub(crate) fn incremental_key_claim(raw: &str) -> Option<Vec<Vec<String>>> {
    let mut columns: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|column| !column.is_empty())
        .map(str::to_string)
        .collect();
    columns.sort();
    (!columns.is_empty()).then_some(vec![columns])
}

/// The kind of audit evidence a record carries — which typed document the
/// payload decodes as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A candidate environment's provisioning record (`environment*.json`).
    Environment,
    /// A branch-diff audit report (`branch_diff.json`).
    BranchDiff,
    /// A lineage-diff audit (`lineage_diff.json`).
    LineageDiff,
}

impl EvidenceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::BranchDiff => "branch_diff",
            Self::LineageDiff => "lineage_diff",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "environment" => Some(Self::Environment),
            "branch_diff" => Some(Self::BranchDiff),
            "lineage_diff" => Some(Self::LineageDiff),
            _ => None,
        }
    }
}

/// One immutable audit-evidence record — the portable, shared form of the
/// workspace's promotion artifacts. The store is the authority: the
/// `.phlo/transform/*.json` files are exports of (and import sources for)
/// these records, so evidence produced on one machine is visible to a
/// promotion evaluated on another against the same store.
///
/// `payload` is the typed document the artifact file also exports
/// (`EnvironmentSetup`, `BranchDiffReport`, `LineageDiffArtifact`); the
/// denormalised binding columns exist for lookup and inspection — the
/// payload remains authoritative.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EvidenceRecord {
    pub evidence_id: String,
    pub kind: EvidenceKind,
    /// The candidate ref the evidence speaks for.
    pub subject: String,
    /// The other side of a pair-scoped record (a branch diff's base ref);
    /// empty when the kind is not pair-scoped.
    pub target_ref: String,
    /// The subject's commit the evidence was produced at, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_hash: Option<String>,
    /// The target's commit the evidence was produced at, when bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_hash: Option<String>,
    /// The definitional fingerprint the evidence carries (a lineage diff's
    /// candidate graph fingerprint), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub payload: serde_json::Value,
    /// The producing run, when the evidence is run-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// When the evidence was produced — RFC 3339. For diffs this is the
    /// report's own finish time, so an imported artifact and a stored
    /// record order identically.
    pub created_at: String,
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
    /// Bind the run to the reference head it validated. Recorded by the
    /// caller after a run completes — the post-run head, never the pre-run
    /// snapshot, since a Nessie candidate advances with every write.
    fn bind_run_reference_hash(
        &self,
        run_id: &str,
        reference_hash: &str,
    ) -> Result<(), EngineError>;
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
    /// Ordered by `materialized_at`: a record older than the stored one is
    /// dropped, so concurrent runs completing out of order cannot regress
    /// shared state.
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
    /// All materialisations of any of the given version hashes, keyed by
    /// hash — one query for what a per-model loop would otherwise fetch in
    /// N round trips.
    fn materialized_by_hashes(
        &self,
        version_hashes: &[String],
    ) -> Result<BTreeMap<String, Vec<MaterializedRecord>>, EngineError> {
        let mut out = BTreeMap::new();
        for hash in version_hashes {
            out.insert(hash.clone(), self.materialized_by_hash(hash)?);
        }
        Ok(out)
    }
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

    /// Append an immutable audit-evidence record. Records are never
    /// updated — a later audit of the same subject appends a new record —
    /// so shared state carries the history, not just the latest claim.
    ///
    /// The default rejects: a store that cannot persist evidence must say
    /// so rather than silently dropping the audit trail.
    fn record_evidence(&self, _record: &EvidenceRecord) -> Result<(), EngineError> {
        Err(EngineError::State(
            "this state store does not persist audit evidence".to_string(),
        ))
    }
    /// Every record of `kind` for a subject, newest first.
    fn evidence_for(
        &self,
        _kind: EvidenceKind,
        _subject: &str,
    ) -> Result<Vec<EvidenceRecord>, EngineError> {
        Err(EngineError::State(
            "this state store does not persist audit evidence".to_string(),
        ))
    }
    /// The newest record of `kind` for each (subject, target) pair that
    /// has one — a candidate audited against two targets keeps both
    /// current records.
    fn latest_evidence(&self, _kind: EvidenceKind) -> Result<Vec<EvidenceRecord>, EngineError> {
        Err(EngineError::State(
            "this state store does not persist audit evidence".to_string(),
        ))
    }
    /// Drop a subject's evidence of `kind` — used when the binding it
    /// describes no longer exists (a deleted environment branch). This is
    /// removal, not mutation: evidence is immutable while its subject
    /// exists. Promotion history lives in `promotions` and is untouched.
    fn remove_evidence(&self, _kind: EvidenceKind, _subject: &str) -> Result<(), EngineError> {
        Err(EngineError::State(
            "this state store does not persist audit evidence".to_string(),
        ))
    }

    /// Record a successful time-window watermark. Only called on success.
    /// Ordered by run generation: a later-started run's observation
    /// supersedes an earlier run's, whatever order the writes land in.
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

    /// Record a successful seed load. Ordered by `loaded_at`, like
    /// `record_materialized`.
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
        // Two writers on one database file should contend, not corrupt:
        // wait for the lock rather than failing the run with SQLITE_BUSY.
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| EngineError::State(error.to_string()))?;
        // Ordered upserts compare the incoming timestamp against the stored
        // one inside the statement, under the write lock — so the comparison
        // has to happen in SQL. Expose `cmp_rfc3339` so legacy variable-width
        // timestamps order chronologically rather than as bytes.
        connection
            .create_scalar_function(
                "phlo_cmp_rfc3339",
                2,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8
                    | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
                |ctx| {
                    let a: String = ctx.get(0)?;
                    let b: String = ctx.get(1)?;
                    Ok(match crate::util::cmp_rfc3339(&a, &b) {
                        std::cmp::Ordering::Less => -1,
                        std::cmp::Ordering::Equal => 0,
                        std::cmp::Ordering::Greater => 1,
                    })
                },
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
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
                    plan_json TEXT,
                    reference_hash TEXT
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
                    adapter TEXT,
                    output_identity TEXT,
                    contract_json TEXT,
                    effective_key_json TEXT,
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
                CREATE TABLE IF NOT EXISTS evidence (
                    evidence_id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    target_ref TEXT NOT NULL DEFAULT '',
                    candidate_hash TEXT,
                    target_hash TEXT,
                    fingerprint TEXT,
                    payload TEXT NOT NULL,
                    run_id TEXT,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS evidence_subject ON evidence (kind, subject, created_at);
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
        let _ = connection.execute("ALTER TABLE model_versions ADD COLUMN adapter TEXT", []);
        let _ = connection.execute(
            "ALTER TABLE model_versions ADD COLUMN output_identity TEXT",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE model_versions ADD COLUMN contract_json TEXT",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE model_versions ADD COLUMN effective_key_json TEXT",
            [],
        );
        // Run-progress columns added for resumable runs.
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN plan_json TEXT", []);
        // The Nessie commit a run validated — added for promotion provenance.
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN reference_hash TEXT", []);
        for column in [
            "ALTER TABLE model_runs ADD COLUMN action TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE model_runs ADD COLUMN desired_version TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE model_runs ADD COLUMN attempts_json TEXT",
            "ALTER TABLE model_runs ADD COLUMN error_category TEXT",
            "ALTER TABLE test_runs ADD COLUMN error_category TEXT",
        ] {
            let _ = connection.execute(column, []);
        }
        // Backfill `effective_key_json` for rows written before the column
        // existed: an incremental `key` strategy's columns were the era's
        // effective key — the only key concept state then recorded. Rows
        // without one stay NULL: their key is unknown, not absent.
        let legacy_keys = {
            let mut statement = connection
                .prepare(
                    "SELECT model_id, environment, incremental_key FROM model_versions
                     WHERE effective_key_json IS NULL
                       AND incremental_strategy = 'key'
                       AND incremental_key IS NOT NULL",
                )
                .map_err(|error| EngineError::State(error.to_string()))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| EngineError::State(error.to_string()))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|error| EngineError::State(error.to_string()))?;
            rows
        };
        for (model_id, environment, raw) in legacy_keys {
            let Some(claim) = incremental_key_claim(&raw) else {
                continue;
            };
            let json = serde_json::to_string(&claim)
                .map_err(|error| EngineError::State(error.to_string()))?;
            connection
                .execute(
                    "UPDATE model_versions SET effective_key_json = ?1
                     WHERE model_id = ?2 AND environment = ?3",
                    rusqlite::params![json, model_id, environment],
                )
                .map_err(|error| EngineError::State(error.to_string()))?;
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

pub(crate) fn status_str(status: ExecutionStatus) -> &'static str {
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
                "INSERT INTO runs (run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, plan_json, reference_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                    run.reference_hash,
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

    fn bind_run_reference_hash(
        &self,
        run_id: &str,
        reference_hash: &str,
    ) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "UPDATE runs SET reference_hash = ?2 WHERE run_id = ?1",
                rusqlite::params![run_id, reference_hash],
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
                "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, reference_hash
                 FROM runs ORDER BY started_at DESC, rowid DESC",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map([], run_summary_from_row)
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut runs = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))?;
        newest_first(&mut runs, |run| &run.started_at);
        Ok(runs)
    }

    fn latest_run(&self, environment: Option<&str>) -> Result<Option<RunSummary>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, reference_hash
                 FROM runs WHERE environment = ?1 ORDER BY started_at DESC, rowid DESC",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(
                rusqlite::params![environment.unwrap_or("")],
                run_summary_from_row,
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        // No `LIMIT 1` — the freshest row is chosen after the parsed-time
        // re-sort, so a variable-width legacy timestamp cannot shadow a
        // newer run.
        let mut runs = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))?;
        newest_first(&mut runs, |run| &run.started_at);
        Ok(runs.into_iter().next())
    }

    fn run(&self, run_id: &str) -> Result<Option<StoredRun>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, plan_json, reference_hash
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
                reference_hash: row.get(9).map_err(map)?,
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
                "SELECT run_id, plan_id, environment, started_at, finished_at, status, model_count, failed_count, reference_hash
                 FROM runs WHERE run_id LIKE ?1 || '%' ORDER BY started_at DESC, rowid DESC",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![prefix], run_summary_from_row)
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut runs = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))?;
        newest_first(&mut runs, |run| &run.started_at);
        Ok(runs)
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
        let contract = record
            .contract
            .as_ref()
            .map(|contract| serde_json::to_string(contract).unwrap_or_default());
        let effective_key = record
            .effective_key
            .as_ref()
            .map(|key| serde_json::to_string(key).unwrap_or_default());
        // Ordered upsert: a record is only applied when it is not older than
        // what is already stored. Two concurrent runs can finish out of
        // order — the run that materialised later physically overwrote the
        // table, so its record must win. Equal timestamps (idempotent
        // re-records) are allowed; the row lands as one coherent record.
        connection
            .execute(
                "INSERT INTO model_versions
                 (model_id, environment, version_hash, sql_hash, config_hash, contract_hash,
                  dependency_hash, source_state_hash, compiler_version, target_hash, target,
                  run_id, materialized_at, incremental_strategy, incremental_key, version_detail,
                  adapter, output_identity, contract_json, effective_key_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
                 ON CONFLICT (model_id, environment) DO UPDATE SET
                    version_hash = excluded.version_hash,
                    sql_hash = excluded.sql_hash,
                    config_hash = excluded.config_hash,
                    contract_hash = excluded.contract_hash,
                    dependency_hash = excluded.dependency_hash,
                    source_state_hash = excluded.source_state_hash,
                    compiler_version = excluded.compiler_version,
                    target_hash = excluded.target_hash,
                    target = excluded.target,
                    run_id = excluded.run_id,
                    materialized_at = excluded.materialized_at,
                    incremental_strategy = excluded.incremental_strategy,
                    incremental_key = excluded.incremental_key,
                    version_detail = excluded.version_detail,
                    adapter = excluded.adapter,
                    output_identity = excluded.output_identity,
                    contract_json = excluded.contract_json,
                    effective_key_json = excluded.effective_key_json
                 WHERE phlo_cmp_rfc3339(excluded.materialized_at, model_versions.materialized_at) >= 0",
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
                    record.adapter,
                    record.output_identity,
                    contract,
                    effective_key,
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

    fn materialized_by_hashes(
        &self,
        version_hashes: &[String],
    ) -> Result<BTreeMap<String, Vec<MaterializedRecord>>, EngineError> {
        let mut out: BTreeMap<String, Vec<MaterializedRecord>> = version_hashes
            .iter()
            .map(|hash| (hash.clone(), Vec::new()))
            .collect();
        if out.is_empty() {
            return Ok(out);
        }
        let connection = self.lock()?;
        let placeholders = version_hashes
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = connection
            .prepare(&format!(
                "SELECT {MATERIALIZED_COLUMNS} FROM model_versions WHERE version_hash IN ({placeholders})"
            ))
            .map_err(|error| EngineError::State(error.to_string()))?;
        let params: Vec<&dyn rusqlite::ToSql> = version_hashes
            .iter()
            .map(|hash| hash as &dyn rusqlite::ToSql)
            .collect();
        let records = statement
            .query_map(params.as_slice(), materialized_from_row)
            .map_err(|error| EngineError::State(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))?;
        for record in records {
            out.entry(record.version.hash.clone())
                .or_default()
                .push(record);
        }
        Ok(out)
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
            .prepare("SELECT record_json FROM promotions ORDER BY created_at DESC, rowid DESC")
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut promotions = rows
            .map(|row| {
                let json = row.map_err(|error| EngineError::State(error.to_string()))?;
                serde_json::from_str(&json).map_err(|error| EngineError::State(error.to_string()))
            })
            .collect::<Result<Vec<crate::promotion::PromotionRecord>, _>>()?;
        newest_first(&mut promotions, |record| &record.timestamp);
        Ok(promotions)
    }

    fn record_evidence(&self, record: &EvidenceRecord) -> Result<(), EngineError> {
        let payload = serde_json::to_string(&record.payload)
            .map_err(|error| EngineError::State(error.to_string()))?;
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO evidence
                 (evidence_id, kind, subject, target_ref, candidate_hash, target_hash, fingerprint, payload, run_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    record.evidence_id,
                    record.kind.as_str(),
                    record.subject,
                    record.target_ref,
                    record.candidate_hash,
                    record.target_hash,
                    record.fingerprint,
                    payload,
                    record.run_id,
                    record.created_at,
                ],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn evidence_for(
        &self,
        kind: EvidenceKind,
        subject: &str,
    ) -> Result<Vec<EvidenceRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT evidence_id, kind, subject, target_ref, candidate_hash, target_hash, fingerprint, payload, run_id, created_at
                 FROM evidence WHERE kind = ?1 AND subject = ?2
                 ORDER BY created_at DESC, rowid DESC",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![kind.as_str(), subject], evidence_from_row)
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut records = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))?;
        newest_first(&mut records, |record| &record.created_at);
        Ok(records)
    }

    fn latest_evidence(&self, kind: EvidenceKind) -> Result<Vec<EvidenceRecord>, EngineError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT evidence_id, kind, subject, target_ref, candidate_hash, target_hash, fingerprint, payload, run_id, created_at
                 FROM evidence WHERE kind = ?1
                 ORDER BY created_at DESC, rowid DESC",
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params![kind.as_str()], evidence_from_row)
            .map_err(|error| EngineError::State(error.to_string()))?;
        let mut records = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::State(error.to_string()))?;
        newest_first(&mut records, |record| &record.created_at);
        let mut seen = std::collections::BTreeSet::new();
        let mut latest = Vec::new();
        for record in records {
            // Latest per (subject, target) pair — a candidate audited
            // against `main` and `release` keeps both current records.
            if seen.insert((record.subject.clone(), record.target_ref.clone())) {
                latest.push(record);
            }
        }
        Ok(latest)
    }

    fn remove_evidence(&self, kind: EvidenceKind, subject: &str) -> Result<(), EngineError> {
        let connection = self.lock()?;
        connection
            .execute(
                "DELETE FROM evidence WHERE kind = ?1 AND subject = ?2",
                rusqlite::params![kind.as_str(), subject],
            )
            .map_err(|error| EngineError::State(error.to_string()))?;
        Ok(())
    }

    fn set_watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
        value: &str,
        run_id: &str,
    ) -> Result<(), EngineError> {
        let connection = self.lock()?;
        // Ordered by run generation, not wall-clock write order: a watermark
        // is an observation made by a run, so the later-started run's value
        // supersedes. Runs without a `runs` row fall back to update order
        // (legacy/manual writes), and a stored run that no longer resolves
        // loses to any identifiable writer.
        connection
            .execute(
                "INSERT INTO incremental_state
                 (model_id, environment, last_value, run_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (model_id, environment) DO UPDATE SET
                    last_value = excluded.last_value,
                    run_id = excluded.run_id,
                    updated_at = excluded.updated_at
                 WHERE phlo_cmp_rfc3339(
                           COALESCE(
                               (SELECT started_at FROM runs WHERE run_id = excluded.run_id),
                               excluded.updated_at
                           ),
                           COALESCE(
                               (SELECT started_at FROM runs WHERE run_id = incremental_state.run_id),
                               ''
                           )
                       ) >= 0",
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
                "INSERT INTO seed_loads
                 (name, environment, content_hash, target, run_id, loaded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (name, environment) DO UPDATE SET
                    content_hash = excluded.content_hash,
                    target = excluded.target,
                    run_id = excluded.run_id,
                    loaded_at = excluded.loaded_at
                 WHERE phlo_cmp_rfc3339(excluded.loaded_at, seed_loads.loaded_at) >= 0",
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
     run_id, materialized_at, incremental_strategy, incremental_key, version_detail, adapter, \
     output_identity, contract_json, effective_key_json";

/// Re-order rows the query pre-sorted by timestamp text into true
/// chronological order: rows written before timestamps went fixed-width
/// can carry variable-width fractions that sort wrong as bytes. The sort
/// is stable, so rows at the same instant keep the query's `rowid` order.
fn newest_first<T>(rows: &mut [T], timestamp: impl Fn(&T) -> &str) {
    rows.sort_by(|a, b| crate::util::cmp_rfc3339(timestamp(b), timestamp(a)));
}

fn run_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSummary> {
    let status: String = row.get(5)?;
    let environment: String = row.get(2)?;
    Ok(RunSummary {
        run_id: row.get(0)?,
        plan_id: row.get(1)?,
        environment: if environment.is_empty() {
            None
        } else {
            Some(environment)
        },
        reference_hash: row.get(8)?,
        started_at: row.get(3)?,
        finished_at: row.get(4)?,
        status: parse_status(&status),
        model_count: row.get::<_, i64>(6)? as usize,
        failed_count: row.get::<_, i64>(7)? as usize,
    })
}

fn evidence_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvidenceRecord> {
    let kind: String = row.get(1)?;
    let payload: String = row.get(7)?;
    let corrupt = |error: String| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, error.into())
    };
    Ok(EvidenceRecord {
        evidence_id: row.get(0)?,
        // An unrecognised kind or an undecodable payload is corrupt
        // evidence — erroring the read fails closed rather than letting a
        // row masquerade as a kind it is not.
        kind: EvidenceKind::parse(&kind)
            .ok_or_else(|| corrupt(format!("unknown evidence kind `{kind}`")))?,
        subject: row.get(2)?,
        target_ref: row.get(3)?,
        candidate_hash: row.get(4)?,
        target_hash: row.get(5)?,
        fingerprint: row.get(6)?,
        payload: serde_json::from_str(&payload).map_err(|error| corrupt(error.to_string()))?,
        run_id: row.get(8)?,
        created_at: row.get(9)?,
    })
}

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
        adapter: row.get(16)?,
        output_identity: row.get(17)?,
        contract: row
            .get::<_, Option<String>>(18)?
            .and_then(|json| serde_json::from_str(&json).ok()),
        effective_key: row
            .get::<_, Option<String>>(19)?
            .and_then(|json| serde_json::from_str(&json).ok()),
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
                    reference_hash: None,
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

    fn materialized(model_id: &str, hash: &str, at: &str, run_id: &str) -> MaterializedRecord {
        MaterializedRecord {
            model_id: model_id.to_string(),
            environment: Some("prod".to_string()),
            version: ModelVersion {
                hash: hash.to_string(),
                ..Default::default()
            },
            detail: None,
            target: "cat.assay.results".to_string(),
            incremental_strategy: None,
            incremental_key: None,
            adapter: Some("trino".to_string()),
            output_identity: Some(format!("snap:{hash}")),
            contract: None,
            effective_key: Some(Vec::new()),
            run_id: run_id.to_string(),
            materialized_at: at.to_string(),
        }
    }

    /// Two writers on one database file, same model+environment, completing
    /// out of order: the record from the *earlier* materialisation must not
    /// overwrite the later one.
    #[test]
    fn out_of_order_materialized_writes_do_not_regress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let writer_a = SqliteStateStore::open(&path).unwrap();
        let writer_b = SqliteStateStore::open(&path).unwrap();

        let mut winning = materialized("assay.results", "v2", "2026-01-01T00:00:02Z", "run-b");
        winning.effective_key = Some(vec![vec!["sample_id".to_string()]]);
        writer_b.record_materialized(&winning).unwrap();
        writer_a
            .record_materialized(&materialized(
                "assay.results",
                "v1",
                "2026-01-01T00:00:01Z",
                "run-a",
            ))
            .unwrap();
        let record = writer_a
            .materialized_version("assay.results", Some("prod"))
            .unwrap()
            .expect("record");
        assert_eq!(record.version.hash, "v2", "the stale write must lose");
        assert_eq!(record.run_id, "run-b");
        // The persisted effective key survives the write and the read.
        assert_eq!(
            record.effective_key.as_deref(),
            Some(&[vec!["sample_id".to_string()]][..])
        );

        // An equal or later timestamp still applies — same-run re-records
        // and genuinely newer writes are unaffected.
        writer_a
            .record_materialized(&materialized(
                "assay.results",
                "v3",
                "2026-01-01T00:00:03Z",
                "run-a",
            ))
            .unwrap();
        let record = writer_b
            .materialized_version("assay.results", Some("prod"))
            .unwrap()
            .expect("record");
        assert_eq!(record.version.hash, "v3");
    }

    /// Reopening a database backfills `effective_key_json` for legacy rows
    /// that recorded an incremental `key` strategy — the era's only key
    /// concept — and leaves rows with no key evidence NULL (unknown, not
    /// keyless).
    #[test]
    fn reopening_backfills_legacy_incremental_keys() {
        use crate::contracts::RecordedKey;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        {
            let store = SqliteStateStore::open(&path).unwrap();
            // A legacy incremental-key row: nothing in `effective_key`,
            // the incremental fields carry the era's key.
            let mut keyed = materialized("assay.keyed", "v1", "2026-01-01T00:00:01Z", "run-1");
            keyed.incremental_strategy = Some("key".to_string());
            keyed.incremental_key = Some("b, a".to_string());
            keyed.effective_key = None;
            store.record_materialized(&keyed).unwrap();
            // A legacy row with no key evidence at all stays NULL.
            let mut bare = materialized("assay.bare", "v1", "2026-01-01T00:00:01Z", "run-1");
            bare.effective_key = None;
            store.record_materialized(&bare).unwrap();
        }

        let store = SqliteStateStore::open(&path).unwrap();
        let keyed = store
            .materialized_version("assay.keyed", Some("prod"))
            .unwrap()
            .expect("keyed record");
        assert_eq!(
            keyed.effective_key.as_deref(),
            Some(&[vec!["a".to_string(), "b".to_string()]][..]),
            "the backfill writes the canonical sorted claim"
        );
        assert_eq!(
            keyed.recorded_key().as_deref(),
            Some(&[vec!["a".to_string(), "b".to_string()]][..])
        );

        let bare = store
            .materialized_version("assay.bare", Some("prod"))
            .unwrap()
            .expect("bare record");
        assert_eq!(bare.effective_key, None, "no evidence to backfill from");
        assert_eq!(bare.recorded_key(), RecordedKey::Unknown);
    }

    #[test]
    fn out_of_order_seed_writes_do_not_regress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let writer_a = SqliteStateStore::open(&path).unwrap();
        let writer_b = SqliteStateStore::open(&path).unwrap();
        let seed = |hash: &str, at: &str, run_id: &str| SeedRecord {
            name: "raw.events".to_string(),
            environment: Some("prod".to_string()),
            content_hash: hash.to_string(),
            target: "cat.raw.events".to_string(),
            run_id: run_id.to_string(),
            loaded_at: at.to_string(),
        };

        writer_b
            .record_seed(&seed("h2", "2026-01-01T00:00:02Z", "run-b"))
            .unwrap();
        writer_a
            .record_seed(&seed("h1", "2026-01-01T00:00:01Z", "run-a"))
            .unwrap();
        let record = writer_a
            .seed_state("raw.events", Some("prod"))
            .unwrap()
            .expect("seed");
        assert_eq!(record.content_hash, "h2");
    }

    /// Watermarks order by run generation, not write order: a watermark is
    /// an observation a run made, so the later-started run supersedes even
    /// when it finishes first.
    #[test]
    fn watermarks_follow_run_generation() {
        let store = SqliteStateStore::in_memory().unwrap();
        let run = |run_id: &str, started_at: &str| {
            store
                .start_run(
                    &RunRecord {
                        run_id: run_id.to_string(),
                        plan_id: "plan".to_string(),
                        environment: Some("prod".to_string()),
                        started_at: started_at.to_string(),
                        finished_at: None,
                        status: ExecutionStatus::Running,
                        model_count: 1,
                        failed_count: 0,
                        reference_hash: None,
                    },
                    &StoredPlan {
                        plan_id: "plan".to_string(),
                        environment: Some("prod".to_string()),
                        models: Vec::new(),
                        seeds: Vec::new(),
                        tests: Vec::new(),
                    },
                )
                .unwrap();
        };
        run("run-old", "2026-01-01T00:00:01Z");
        run("run-new", "2026-01-01T00:00:02Z");

        store
            .set_watermark("assay.results", Some("prod"), "2026-06-01", "run-new")
            .unwrap();
        // The older run's observation arrives late — it must not rewind the
        // newer run's mark.
        store
            .set_watermark("assay.results", Some("prod"), "2026-01-01", "run-old")
            .unwrap();
        assert_eq!(
            store.watermark("assay.results", Some("prod")).unwrap(),
            Some("2026-06-01".to_string())
        );
        // The newer generation still advances.
        store
            .set_watermark("assay.results", Some("prod"), "2026-12-01", "run-new")
            .unwrap();
        assert_eq!(
            store.watermark("assay.results", Some("prod")).unwrap(),
            Some("2026-12-01".to_string())
        );
    }

    /// Mixed-version writers disagree on timestamp shape: legacy rows carry
    /// bare `Z` or variable-width fractions, new writes fixed-width
    /// nanoseconds. Byte-wise a legacy `...00.5Z` sorts *after* a newer
    /// `...00.501000000Z` (`Z` > `0`), so the ordered upserts must compare
    /// parsed instants — this covers all three timestamp-gated writes.
    #[test]
    fn mixed_format_timestamp_writes_order_by_instant() {
        let store = SqliteStateStore::in_memory().unwrap();

        // model_versions: a legacy-format row wins its insert, then a
        // same-second newer write — byte-wise smaller — must still apply.
        store
            .record_materialized(&materialized(
                "assay.results",
                "v-legacy",
                "2026-01-01T00:00:00.5Z",
                "run-old",
            ))
            .unwrap();
        store
            .record_materialized(&materialized(
                "assay.results",
                "v-new",
                "2026-01-01T00:00:00.501000000Z",
                "run-new",
            ))
            .unwrap();
        assert_eq!(
            store
                .materialized_version("assay.results", Some("prod"))
                .unwrap()
                .expect("record")
                .version
                .hash,
            "v-new",
            "a newer instant wins regardless of timestamp shape"
        );
        // The stale direction still loses.
        store
            .record_materialized(&materialized(
                "assay.results",
                "v-stale",
                "2026-01-01T00:00:00.499000000Z",
                "run-stale",
            ))
            .unwrap();
        assert_eq!(
            store
                .materialized_version("assay.results", Some("prod"))
                .unwrap()
                .expect("record")
                .version
                .hash,
            "v-new"
        );
        // An equal instant written differently re-records idempotently.
        store
            .record_materialized(&materialized(
                "assay.results",
                "v-same",
                "2026-01-01T00:00:00.501000000Z",
                "run-same",
            ))
            .unwrap();
        assert_eq!(
            store
                .materialized_version("assay.results", Some("prod"))
                .unwrap()
                .expect("record")
                .version
                .hash,
            "v-same"
        );
        // Bare-`Z` legacy vs fractional new format on another key.
        store
            .record_materialized(&materialized(
                "assay.other",
                "v-legacy",
                "2026-01-01T00:00:01Z",
                "run-old",
            ))
            .unwrap();
        store
            .record_materialized(&materialized(
                "assay.other",
                "v-new",
                "2026-01-01T00:00:01.25Z",
                "run-new",
            ))
            .unwrap();
        assert_eq!(
            store
                .materialized_version("assay.other", Some("prod"))
                .unwrap()
                .expect("record")
                .version
                .hash,
            "v-new"
        );

        // seed_loads: the same ordering through `loaded_at`.
        let seed = |hash: &str, at: &str, run_id: &str| SeedRecord {
            name: "raw.events".to_string(),
            environment: Some("prod".to_string()),
            content_hash: hash.to_string(),
            target: "cat.raw.events".to_string(),
            run_id: run_id.to_string(),
            loaded_at: at.to_string(),
        };
        store
            .record_seed(&seed("h-legacy", "2026-01-01T00:00:00.5Z", "run-old"))
            .unwrap();
        store
            .record_seed(&seed("h-new", "2026-01-01T00:00:00.501000000Z", "run-new"))
            .unwrap();
        assert_eq!(
            store
                .seed_state("raw.events", Some("prod"))
                .unwrap()
                .expect("seed")
                .content_hash,
            "h-new"
        );

        // incremental_state: the ordering reads `runs.started_at` — a
        // legacy-shaped run start must not beat a newer run's
        // different-shaped start.
        let run = |run_id: &str, started_at: &str| {
            store
                .start_run(
                    &RunRecord {
                        run_id: run_id.to_string(),
                        plan_id: "plan".to_string(),
                        environment: Some("prod".to_string()),
                        started_at: started_at.to_string(),
                        finished_at: None,
                        status: ExecutionStatus::Running,
                        model_count: 1,
                        failed_count: 0,
                        reference_hash: None,
                    },
                    &StoredPlan {
                        plan_id: "plan".to_string(),
                        environment: Some("prod".to_string()),
                        models: Vec::new(),
                        seeds: Vec::new(),
                        tests: Vec::new(),
                    },
                )
                .unwrap();
        };
        run("run-old", "2026-01-01T00:00:00.5Z");
        run("run-new", "2026-01-01T00:00:00.501000000Z");
        store
            .set_watermark("assay.results", Some("prod"), "old-mark", "run-old")
            .unwrap();
        store
            .set_watermark("assay.results", Some("prod"), "new-mark", "run-new")
            .unwrap();
        assert_eq!(
            store.watermark("assay.results", Some("prod")).unwrap(),
            Some("new-mark".to_string()),
            "the newer run's mark must win across timestamp shapes"
        );
    }

    fn evidence(id: &str, kind: EvidenceKind, subject: &str, created_at: &str) -> EvidenceRecord {
        EvidenceRecord {
            evidence_id: id.to_string(),
            kind,
            subject: subject.to_string(),
            target_ref: "main".to_string(),
            candidate_hash: Some("candhash".to_string()),
            target_hash: Some("targ_hash".to_string()),
            fingerprint: Some("fp".to_string()),
            payload: serde_json::json!({"marker": id}),
            run_id: Some("run-1".to_string()),
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn evidence_orders_same_second_records_chronologically() {
        let store = SqliteStateStore::in_memory().unwrap();
        // Timestamps written before the format went fixed-width carry
        // variable-width fractions: `.5Z` (500ms) sorts after `.52Z`
        // (520ms) as text — `Z` > `2` — and a bare `Z` sorts after every
        // fraction. Byte order alone would return these oldest-first.
        for (id, created_at) in [
            ("e-mid", "2026-01-01T00:00:00.5Z"),
            ("e-new", "2026-01-01T00:00:00.52Z"),
            ("e-zero", "2026-01-01T00:00:00Z"),
        ] {
            store
                .record_evidence(&evidence(id, EvidenceKind::BranchDiff, "ci/x", created_at))
                .unwrap();
        }
        let records = store
            .evidence_for(EvidenceKind::BranchDiff, "ci/x")
            .unwrap();
        let ids: Vec<&str> = records
            .iter()
            .map(|record| record.evidence_id.as_str())
            .collect();
        assert_eq!(ids, ["e-new", "e-mid", "e-zero"]);
        // `latest_evidence` dedups to the same freshest record.
        assert_eq!(
            store.latest_evidence(EvidenceKind::BranchDiff).unwrap()[0].evidence_id,
            "e-new"
        );
    }

    #[test]
    fn latest_run_orders_same_second_starts_chronologically() {
        let store = SqliteStateStore::in_memory().unwrap();
        let plan = StoredPlan {
            plan_id: "plan".to_string(),
            environment: Some("ci/x".to_string()),
            models: Vec::new(),
            seeds: Vec::new(),
            tests: Vec::new(),
        };
        for (run_id, started_at) in [
            ("run-mid", "2026-01-01T00:00:00.5Z"),
            ("run-new", "2026-01-01T00:00:00.52Z"),
            ("run-zero", "2026-01-01T00:00:00Z"),
        ] {
            store
                .start_run(
                    &RunRecord {
                        run_id: run_id.to_string(),
                        plan_id: "plan".to_string(),
                        environment: Some("ci/x".to_string()),
                        reference_hash: None,
                        started_at: started_at.to_string(),
                        finished_at: None,
                        status: ExecutionStatus::Running,
                        model_count: 0,
                        failed_count: 0,
                    },
                    &plan,
                )
                .unwrap();
        }
        assert_eq!(
            store.latest_run(Some("ci/x")).unwrap().unwrap().run_id,
            "run-new"
        );
    }

    #[test]
    fn evidence_appends_and_reads_newest_first() {
        let store = SqliteStateStore::in_memory().unwrap();
        store
            .record_evidence(&evidence(
                "e1",
                EvidenceKind::BranchDiff,
                "ci/x",
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();
        store
            .record_evidence(&evidence(
                "e2",
                EvidenceKind::BranchDiff,
                "ci/x",
                "2026-01-02T00:00:00Z",
            ))
            .unwrap();

        // Append-only: both audits are retained, newest first.
        let records = store
            .evidence_for(EvidenceKind::BranchDiff, "ci/x")
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].evidence_id, "e2");
        assert_eq!(records[1].evidence_id, "e1");
        // A re-audit does not mutate the earlier record.
        assert_eq!(records[1].payload["marker"], "e1");

        // Other kinds and subjects are separate.
        assert!(store
            .evidence_for(EvidenceKind::LineageDiff, "ci/x")
            .unwrap()
            .is_empty());
        assert!(store
            .evidence_for(EvidenceKind::BranchDiff, "ci/y")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn evidence_latest_is_per_subject_and_target_pair() {
        let store = SqliteStateStore::in_memory().unwrap();
        store
            .record_evidence(&evidence(
                "e1",
                EvidenceKind::BranchDiff,
                "ci/x",
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();
        // ci/x re-audited against a different target — both stay current.
        let mut release = evidence(
            "e2",
            EvidenceKind::BranchDiff,
            "ci/x",
            "2026-01-02T00:00:00Z",
        );
        release.target_ref = "release".to_string();
        store.record_evidence(&release).unwrap();
        store
            .record_evidence(&evidence(
                "e3",
                EvidenceKind::BranchDiff,
                "ci/y",
                "2026-01-03T00:00:00Z",
            ))
            .unwrap();

        let latest = store.latest_evidence(EvidenceKind::BranchDiff).unwrap();
        let pairs: Vec<(&str, &str)> = latest
            .iter()
            .map(|record| (record.subject.as_str(), record.target_ref.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![("ci/y", "main"), ("ci/x", "release"), ("ci/x", "main")]
        );
    }

    #[test]
    fn evidence_ids_are_unique() {
        let store = SqliteStateStore::in_memory().unwrap();
        store
            .record_evidence(&evidence(
                "e1",
                EvidenceKind::Environment,
                "ci/x",
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();
        // A colliding id is rejected — evidence never overwrites in place.
        let error = store
            .record_evidence(&evidence(
                "e1",
                EvidenceKind::Environment,
                "ci/x",
                "2026-01-02T00:00:00Z",
            ))
            .expect_err("duplicate evidence id must fail");
        assert!(matches!(error, EngineError::State(_)), "{error}");
    }

    #[test]
    fn evidence_removal_is_per_kind_and_subject() {
        let store = SqliteStateStore::in_memory().unwrap();
        store
            .record_evidence(&evidence(
                "e1",
                EvidenceKind::Environment,
                "ci/x",
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();
        store
            .record_evidence(&evidence(
                "e2",
                EvidenceKind::BranchDiff,
                "ci/x",
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();
        store
            .record_evidence(&evidence(
                "e3",
                EvidenceKind::Environment,
                "ci/y",
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();

        // Deleting ci/x's environment binding leaves its diffs and other
        // candidates' environment evidence alone.
        store
            .remove_evidence(EvidenceKind::Environment, "ci/x")
            .unwrap();
        assert!(store
            .evidence_for(EvidenceKind::Environment, "ci/x")
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .evidence_for(EvidenceKind::BranchDiff, "ci/x")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .evidence_for(EvidenceKind::Environment, "ci/y")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn evidence_is_visible_across_connections() {
        // Two store handles on one database file stand in for two processes
        // sharing state: what one records, the other reads.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let writer = SqliteStateStore::open(&path).unwrap();
        writer
            .record_evidence(&evidence(
                "e1",
                EvidenceKind::Environment,
                "ci/x",
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();

        let reader = SqliteStateStore::open(&path).unwrap();
        let records = reader
            .evidence_for(EvidenceKind::Environment, "ci/x")
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].payload["marker"], "e1");
        assert_eq!(records[0].candidate_hash.as_deref(), Some("candhash"));
    }

    #[test]
    fn a_corrupt_evidence_row_fails_the_read() {
        // A row whose payload cannot be decoded is corrupt evidence — the
        // read errors rather than silently dropping it. (Unknown kinds are
        // filtered by the query itself, so a newer binary's records are
        // invisible to an older one rather than fatal.)
        let store = SqliteStateStore::in_memory().unwrap();
        {
            let connection = store.lock().unwrap();
            connection
                .execute(
                    "INSERT INTO evidence
                     (evidence_id, kind, subject, target_ref, payload, created_at)
                     VALUES ('bad', 'environment', 'ci/x', '', 'not json', 't')",
                    [],
                )
                .unwrap();
        }
        let error = store
            .evidence_for(EvidenceKind::Environment, "ci/x")
            .expect_err("an undecodable payload is corrupt evidence");
        assert!(matches!(error, EngineError::State(_)), "{error}");
        // …and it does not poison unrelated subjects.
        assert!(store
            .evidence_for(EvidenceKind::Environment, "ci/y")
            .unwrap()
            .is_empty());
    }
}
