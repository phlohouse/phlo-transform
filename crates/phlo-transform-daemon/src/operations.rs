//! Long-running operation registry for the machine-facing API.
//!
//! Mutating or long-running work (`run`, `test`, `promote`, `reload`) is
//! submitted as an operation handle instead of blocking a request. Each
//! operation has a stable lifecycle (queued → running →
//! succeeded/failed/cancelled), a cooperative `CancelHandle`, and optional
//! idempotency: resubmitting a body with the same `idempotency_key` (or
//! `Idempotency-Key` header) returns the existing handle instead of running
//! twice.
//!
//! At most one warehouse-mutating operation (`run`, `promote`) executes at a
//! time; concurrent submissions are rejected with `API008`.
//!
//! When a journal path is configured, every state transition is appended to
//! `.phlo/transform/operations.jsonl` and folded back on `open` — operation
//! history and idempotency keys survive a daemon restart, so a retried
//! submission replays its original handle instead of executing twice.
//! Operations that were still in flight when the daemon stopped are restored
//! as `failed` with the `interrupted` code: their cancel handles cannot be
//! reconstructed and their effects cannot be assumed.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use phlo_transform_engine::util::now_rfc3339;
use phlo_transform_engine::CancelHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl OperationStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            OperationStatus::Succeeded | OperationStatus::Failed | OperationStatus::Cancelled
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: String,
    pub kind: String,
    pub status: OperationStatus,
    pub submitted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// The validated operation parameters (defaults applied).
    pub params: Value,
    /// The idempotency key the operation was submitted with — persisted so
    /// a replay after a daemon restart still returns this handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Canonical fingerprint of the request — `sha256` over
    /// `{kind, params}`. A resubmission with the same key but a different
    /// request is a conflict, not a replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_hash: Option<String>,
    /// The engine report on success — the same DTO the CLI's `--json`
    /// prints (`run.json`, gate report + promotion record, test report).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OperationError>,
}

struct OperationEntry {
    record: OperationRecord,
    cancel: CancelHandle,
}

#[derive(Default)]
pub struct OperationStore {
    ops: RwLock<BTreeMap<String, OperationEntry>>,
    /// `idempotency_key` -> operation id. Keys are global to
    /// `POST /v1/operations`: kind lives inside the request fingerprint,
    /// so the same key under a different kind is a conflict, not a new
    /// operation in another bucket.
    idempotent: Mutex<BTreeMap<String, String>>,
    /// At most one warehouse-mutating operation at a time.
    busy: AtomicBool,
    /// Append-only record journal; `None` for tests without durability.
    journal: Option<PathBuf>,
}

/// Kinds that write to the warehouse or mutate branch state — serialized
/// through the busy gate.
const MUTATING: &[&str] = &["run", "resume", "retry_failed", "promote"];
/// Every kind the API accepts.
pub const KINDS: &[&str] = &["run", "resume", "retry_failed", "test", "promote", "reload"];

/// Per-kind parameter schemas. `deny_unknown_fields` keeps the contract
/// strict: a misspelled key is a 400, never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunParams {
    #[serde(default)]
    pub selectors: Vec<String>,
    /// The environment to run against — the candidate Nessie reference
    /// (the CLI's `--ref`). When the daemon has Nessie + adapter handles,
    /// the environment is provisioned (branch + catalog) and the workspace
    /// is recompiled against the candidate catalog before planning; without
    /// them it is only a state-record label.
    pub environment: Option<String>,
    /// The base ref a new candidate branch is cut from (the CLI's
    /// `--from`, default `main`).
    pub base: Option<String>,
    #[serde(default)]
    pub force: bool,
    pub run_tests: Option<bool>,
}

/// `resume`/`retry_failed` share one parameter shape: the run to continue.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinueParams {
    /// Run id or unique prefix (the CLI's `--resume`/`--retry-failed`).
    /// The stored run's environment is authoritative — the continuation
    /// targets whatever environment the original run ran against.
    pub run: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestParams {
    #[serde(default)]
    pub selectors: Vec<String>,
    /// The environment whose catalog the tests run against — the CLI's
    /// `--environment`/`--ref`. Resolved through the shared environment
    /// context like a run: with Nessie handles the workspace compiles
    /// against the candidate's catalog; without them it is a label only.
    pub environment: Option<String>,
    /// The base ref the environment resolves against (the CLI's `--from`).
    pub base: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromoteParams {
    pub candidate: String,
    pub to: String,
    /// Evaluate the gates and stop — the CLI's `promote --check`.
    #[serde(default)]
    pub check: bool,
    /// Fail unless a fresh value-level diff artifact authorises the pair —
    /// the CLI's `--require-diff`.
    #[serde(default)]
    pub require_diff: bool,
    #[serde(default)]
    pub allow_breaking_schema: bool,
    /// Drop the candidate catalog/branch after a successful merge.
    #[serde(default)]
    pub cleanup: bool,
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadParams {}

/// What a submission validated into.
pub enum Params {
    Run(RunParams),
    Resume(ContinueParams),
    RetryFailed(ContinueParams),
    Test(TestParams),
    Promote(PromoteParams),
    Reload(ReloadParams),
}

impl Params {
    pub fn kind(&self) -> &'static str {
        match self {
            Params::Run(_) => "run",
            Params::Resume(_) => "resume",
            Params::RetryFailed(_) => "retry_failed",
            Params::Test(_) => "test",
            Params::Promote(_) => "promote",
            Params::Reload(_) => "reload",
        }
    }

    /// The parameters echoed back in the operation record.
    pub fn echo(&self) -> Value {
        match self {
            Params::Run(p) => json!({
                "selectors": p.selectors,
                "environment": p.environment,
                "base": p.base,
                "force": p.force,
                "run_tests": p.run_tests,
            }),
            Params::Resume(p) | Params::RetryFailed(p) => json!({ "run": p.run }),
            Params::Test(p) => json!({
                "selectors": p.selectors,
                "environment": p.environment,
                "base": p.base,
            }),
            Params::Promote(p) => json!({
                "candidate": p.candidate,
                "to": p.to,
                "check": p.check,
                "require_diff": p.require_diff,
                "allow_breaking_schema": p.allow_breaking_schema,
                "cleanup": p.cleanup,
                "actor": p.actor,
            }),
            Params::Reload(_) => json!({}),
        }
    }
}

/// The canonical request fingerprint: `sha256` over the serialized
/// `{kind, params}` — `params.echo()` is built from fixed key order, so the
/// hash is stable across submissions and daemon restarts.
fn request_hash(params: &Params) -> String {
    let canonical = serde_json::to_string(&json!({
        "kind": params.kind(),
        "params": params.echo(),
    }))
    .expect("params serialise");
    phlo_transform_core::version::sha256_hex(&canonical)
}

/// Parse a submission body: `{kind, idempotency_key?, params?}`.
/// `params` may also be spread at the top level for convenience.
pub fn parse_submission(
    body: &Value,
) -> Result<(String, Params, Option<String>), (String, String)> {
    let kind = body
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| ("API012".to_string(), "missing `kind`".to_string()))?;
    let idempotency_key = body
        .get("idempotency_key")
        .and_then(Value::as_str)
        .map(str::to_string);
    let params = match body.get("params") {
        Some(p) => p.clone(),
        None => {
            // Top-level params: everything except the envelope keys.
            let mut p = body.clone();
            if let Some(map) = p.as_object_mut() {
                map.remove("kind");
                map.remove("idempotency_key");
            }
            p
        }
    };
    let parsed = match kind {
        "run" => serde_json::from_value::<RunParams>(params).map(Params::Run),
        "resume" => serde_json::from_value::<ContinueParams>(params).map(Params::Resume),
        "retry_failed" => serde_json::from_value::<ContinueParams>(params).map(Params::RetryFailed),
        "test" => serde_json::from_value::<TestParams>(params).map(Params::Test),
        "promote" => serde_json::from_value::<PromoteParams>(params).map(Params::Promote),
        "reload" => serde_json::from_value::<ReloadParams>(params).map(Params::Reload),
        _ => {
            return Err((
                "API012".to_string(),
                format!("unknown operation kind `{kind}`; expected one of {KINDS:?}"),
            ))
        }
    };
    parsed
        .map(|p| (kind.to_string(), p, idempotency_key))
        .map_err(|error| {
            (
                "API012".to_string(),
                format!("invalid `{kind}` params: {error}"),
            )
        })
}

/// What a submission produced.
#[derive(Debug)]
pub enum SubmitOutcome {
    /// A new operation was queued.
    New(OperationRecord),
    /// The idempotency key matched an existing operation with the same
    /// request fingerprint.
    Replayed(OperationRecord),
    /// The idempotency key matched an existing operation submitted with
    /// *different* params — a `409`, never a replay.
    Conflict(OperationRecord),
    /// Another mutating operation holds the gate.
    Busy,
}

impl OperationStore {
    /// An in-memory store (tests, embedded use without durability).
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the store with a journal file: every transition is appended, and
    /// the file is folded back in on load. Records left non-terminal by a
    /// crash are restored as `failed` (`interrupted`) — they did not finish,
    /// and their cancel handles are gone with the process.
    pub fn open(journal: PathBuf) -> Self {
        let store = Self {
            journal: Some(journal.clone()),
            ..Self::default()
        };
        let Ok(text) = std::fs::read_to_string(&journal) else {
            return store;
        };
        let mut interrupted = Vec::new();
        {
            let mut ops = store.ops.write().expect("ops lock");
            let mut idempotent = store.idempotent.lock().expect("idempotency lock");
            for line in text.lines() {
                let Ok(record) = serde_json::from_str::<OperationRecord>(line) else {
                    continue;
                };
                if let Some(key) = &record.idempotency_key {
                    idempotent.insert(key.clone(), record.id.clone());
                }
                let mut record = record;
                if !record.status.terminal() {
                    record.status = OperationStatus::Failed;
                    record.error = Some(OperationError {
                        code: "interrupted".to_string(),
                        message: "the daemon restarted while this operation was in flight"
                            .to_string(),
                    });
                    record.finished_at = Some(now_rfc3339());
                    interrupted.push(record.clone());
                }
                ops.insert(
                    record.id.clone(),
                    OperationEntry {
                        record,
                        cancel: CancelHandle::default(),
                    },
                );
            }
        }
        for record in interrupted {
            store.journal_note(&record);
        }
        store
    }

    /// Append one record line to the journal — the only channel through
    /// which a transition becomes durable.
    fn journal_append(&self, record: &OperationRecord) -> std::io::Result<()> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        if let Some(parent) = journal.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal)?;
        let line = serde_json::to_string(record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")
    }

    /// Journal a non-reservation transition. Best-effort: the queued record
    /// was persisted durably at submit time, so a lost transition degrades
    /// history (the op is restored as `interrupted` on restart) but can
    /// never cause a duplicate execution.
    fn journal_note(&self, record: &OperationRecord) {
        if let Err(error) = self.journal_append(record) {
            eprintln!(
                "warning: could not journal operation {}: {error}",
                record.id
            );
        }
    }

    /// Register a queued operation. The idempotency lookup and the insert
    /// happen under one lock, so concurrent resubmissions of the same key
    /// cannot execute twice.
    ///
    /// The queued record — the idempotency reservation — is journaled
    /// *before* it becomes visible; when a journal is configured and the
    /// append fails, the submission is rejected rather than acknowledged,
    /// so a restart can never accept the same key twice.
    pub fn submit(
        &self,
        params: &Params,
        key: Option<&str>,
    ) -> Result<SubmitOutcome, OperationError> {
        let hash = request_hash(params);
        if let Some(key) = key {
            let mut idempotent = self.idempotent.lock().expect("idempotency lock");
            // Keys are global to the endpoint — the fingerprint already
            // carries the kind, so a hit with a different hash is a
            // conflict whether the params or the kind itself differs.
            if let Some(existing) = idempotent.get(key).and_then(|id| self.get(id)) {
                // A record without a stored hash predates request binding —
                // its request cannot be re-verified, so it replays by key.
                return Ok(
                    if existing.request_hash.as_deref().is_none()
                        || existing.request_hash.as_deref() == Some(hash.as_str())
                    {
                        SubmitOutcome::Replayed(existing)
                    } else {
                        SubmitOutcome::Conflict(existing)
                    },
                );
            }
            let Some(record) = self.register(params, Some(key), &hash)? else {
                return Ok(SubmitOutcome::Busy);
            };
            idempotent.insert(key.to_string(), record.id.clone());
            return Ok(SubmitOutcome::New(record));
        }
        match self.register(params, None, &hash)? {
            Some(record) => Ok(SubmitOutcome::New(record)),
            None => Ok(SubmitOutcome::Busy),
        }
    }

    /// Try to take the mutating-operation gate. `None` when the kind is not
    /// gated; `Some(true)` acquired; `Some(false)` another op holds it.
    fn acquire(&self, kind: &str) -> Option<bool> {
        if !MUTATING.contains(&kind) {
            return None;
        }
        Some(
            self.busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
        )
    }

    pub fn release(&self, kind: &str) {
        if MUTATING.contains(&kind) {
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    /// Insert the queued record; `Ok(None)` when the gate is held. The
    /// journal append happens before the record is visible: a reservation
    /// that cannot be persisted releases the gate and fails the submission.
    fn register(
        &self,
        params: &Params,
        key: Option<&str>,
        hash: &str,
    ) -> Result<Option<OperationRecord>, OperationError> {
        if let Some(false) = self.acquire(params.kind()) {
            return Ok(None);
        }
        let record = OperationRecord {
            id: uuid::Uuid::new_v4().to_string(),
            kind: params.kind().to_string(),
            status: OperationStatus::Queued,
            submitted_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            params: params.echo(),
            idempotency_key: key.map(str::to_string),
            request_hash: Some(hash.to_string()),
            result: None,
            error: None,
        };
        if let Err(error) = self.journal_append(&record) {
            self.release(params.kind());
            return Err(OperationError {
                code: "API011".to_string(),
                message: format!("could not durably record the operation submission: {error}"),
            });
        }
        self.ops.write().expect("ops lock").insert(
            record.id.clone(),
            OperationEntry {
                record: record.clone(),
                cancel: CancelHandle::default(),
            },
        );
        Ok(Some(record))
    }

    pub fn get(&self, id: &str) -> Option<OperationRecord> {
        self.ops
            .read()
            .expect("ops lock")
            .get(id)
            .map(|entry| entry.record.clone())
    }

    pub fn list(&self) -> Vec<OperationRecord> {
        self.ops
            .read()
            .expect("ops lock")
            .values()
            .map(|entry| entry.record.clone())
            .collect()
    }

    /// Signal cooperative cancellation; `false` when unknown or finished.
    pub fn cancel(&self, id: &str) -> bool {
        let ops = self.ops.read().expect("ops lock");
        match ops.get(id) {
            Some(entry) if !entry.record.status.terminal() => {
                entry.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// The cancel handle for the executor of this op.
    pub fn cancel_handle(&self, id: &str) -> CancelHandle {
        self.ops
            .read()
            .expect("ops lock")
            .get(id)
            .map(|entry| entry.cancel.clone())
            .unwrap_or_default()
    }

    /// Check the cooperative cancel flag (for executors).
    pub fn is_cancelled(&self, id: &str) -> bool {
        self.ops
            .read()
            .expect("ops lock")
            .get(id)
            .map(|entry| entry.cancel.is_cancelled())
            .unwrap_or(false)
    }

    pub fn mark_running(&self, id: &str) {
        let record = {
            let mut ops = self.ops.write().expect("ops lock");
            if let Some(entry) = ops.get_mut(id) {
                if entry.record.status == OperationStatus::Queued {
                    entry.record.status = OperationStatus::Running;
                    entry.record.started_at = Some(now_rfc3339());
                    Some(entry.record.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(record) = record {
            self.journal_note(&record);
        }
    }

    /// Finish an operation: result on success, error on failure, cancelled
    /// when the cancel flag was observed.
    pub fn finish(&self, id: &str, outcome: Result<Value, OperationError>) {
        let record = {
            let mut ops = self.ops.write().expect("ops lock");
            ops.get_mut(id).map(|entry| {
                let cancelled = entry.cancel.is_cancelled();
                match outcome {
                    Ok(result) => {
                        entry.record.status = if cancelled {
                            OperationStatus::Cancelled
                        } else {
                            OperationStatus::Succeeded
                        };
                        entry.record.result = Some(result);
                    }
                    Err(mut error) => {
                        if cancelled {
                            entry.record.status = OperationStatus::Cancelled;
                            error.code = "cancelled".to_string();
                        } else {
                            entry.record.status = OperationStatus::Failed;
                        }
                        entry.record.error = Some(error);
                    }
                }
                entry.record.finished_at = Some(now_rfc3339());
                entry.record.clone()
            })
        };
        if let Some(record) = record {
            self.journal_note(&record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_params() -> Params {
        Params::Run(RunParams {
            selectors: vec![],
            environment: None,
            base: None,
            force: false,
            run_tests: None,
        })
    }

    fn run_params_env(environment: &str) -> Params {
        Params::Run(RunParams {
            selectors: vec![],
            environment: Some(environment.to_string()),
            base: None,
            force: false,
            run_tests: None,
        })
    }

    /// A restart restores finished history and idempotency keys; an op that
    /// was in flight when the process died comes back as `interrupted`,
    /// never silently re-executed.
    #[test]
    fn journal_restores_history_and_idempotency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = dir.path().join("operations.jsonl");
        let id;
        {
            let store = OperationStore::open(journal.clone());
            let SubmitOutcome::New(record) =
                store.submit(&run_params(), Some("k1")).expect("submit")
            else {
                panic!("expected new");
            };
            id = record.id.clone();
            store.mark_running(&id);
            store.finish(&id, Ok(json!({"status": "passed"})));

            let SubmitOutcome::New(stuck) = store
                .submit(&Params::Reload(ReloadParams {}), None)
                .expect("submit")
            else {
                panic!("expected new");
            };
            store.mark_running(&stuck.id);
        }

        let store = OperationStore::open(journal);
        let restored = store.get(&id).expect("finished op restored");
        assert_eq!(restored.status, OperationStatus::Succeeded);
        assert_eq!(restored.result, Some(json!({"status": "passed"})));
        match store.submit(&run_params(), Some("k1")).expect("submit") {
            SubmitOutcome::Replayed(record) => assert_eq!(record.id, id),
            _ => panic!("keyed resubmission after restart must replay, not re-execute"),
        }
        let stuck = store
            .list()
            .into_iter()
            .find(|record| record.kind == "reload")
            .expect("in-flight op restored");
        assert_eq!(stuck.status, OperationStatus::Failed);
        assert_eq!(
            stuck.error.as_ref().map(|error| error.code.as_str()),
            Some("interrupted")
        );
    }

    /// Same key + same request replays; same key + different request is a
    /// conflict — never a replay of an operation that did different work.
    #[test]
    fn idempotency_is_request_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = dir.path().join("operations.jsonl");
        let store = OperationStore::open(journal.clone());

        let SubmitOutcome::New(first) = store
            .submit(&run_params_env("dev"), Some("k1"))
            .expect("submit")
        else {
            panic!("expected new");
        };
        match store
            .submit(&run_params_env("dev"), Some("k1"))
            .expect("submit")
        {
            SubmitOutcome::Replayed(record) => assert_eq!(record.id, first.id),
            _ => panic!("identical request must replay"),
        }
        match store
            .submit(&run_params_env("prod"), Some("k1"))
            .expect("submit")
        {
            SubmitOutcome::Conflict(record) => assert_eq!(record.id, first.id),
            _ => panic!("same key with different params must conflict"),
        }
        // Keys are global to the endpoint: a different kind under the same
        // key is a conflict too — kind is part of the request fingerprint.
        match store
            .submit(&Params::Reload(ReloadParams {}), Some("k1"))
            .expect("submit")
        {
            SubmitOutcome::Conflict(record) => assert_eq!(record.id, first.id),
            _ => panic!("a different kind under the same key must conflict"),
        }

        // The binding survives a restart: replay and conflict still hold.
        let store = OperationStore::open(journal);
        match store
            .submit(&run_params_env("dev"), Some("k1"))
            .expect("submit")
        {
            SubmitOutcome::Replayed(record) => assert_eq!(record.id, first.id),
            _ => panic!("identical request must replay after restart"),
        }
        match store
            .submit(&run_params_env("prod"), Some("k1"))
            .expect("submit")
        {
            SubmitOutcome::Conflict(_) => {}
            _ => panic!("conflict must survive restart"),
        }
    }

    /// The idempotency reservation must be durable before the operation is
    /// acknowledged: a journal that cannot be written rejects the
    /// submission instead of queueing work a restart would repeat.
    #[test]
    fn unwritable_journal_rejects_the_reservation() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory where the journal file should be: appending fails.
        let journal = dir.path().join("operations.jsonl");
        std::fs::create_dir(&journal).expect("journal as directory");
        let store = OperationStore::open(journal);

        let error = store
            .submit(&run_params(), Some("k1"))
            .expect_err("submission must fail");
        assert_eq!(error.code, "API011");
        assert!(
            store.list().is_empty(),
            "the rejected submission must not leave a record"
        );
        // The mutating gate was released: a later submission is not held.
        assert!(store
            .submit(&Params::Reload(ReloadParams {}), None)
            .is_err());
        // (reload is not gated — this also fails on the journal, which is
        // the point: no submission is acknowledged without a durable record)
    }
}
