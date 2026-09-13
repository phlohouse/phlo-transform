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

use std::collections::BTreeMap;
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

#[derive(Clone, Debug, Serialize)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
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
    /// `(kind, idempotency_key)` -> operation id.
    idempotent: Mutex<BTreeMap<(String, String), String>>,
    /// At most one warehouse-mutating operation at a time.
    busy: AtomicBool,
}

/// Kinds that write to the warehouse or mutate branch state — serialized
/// through the busy gate.
const MUTATING: &[&str] = &["run", "promote"];
/// Every kind the API accepts.
pub const KINDS: &[&str] = &["run", "test", "promote", "reload"];

/// Per-kind parameter schemas. `deny_unknown_fields` keeps the contract
/// strict: a misspelled key is a 400, never silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunParams {
    #[serde(default)]
    pub selectors: Vec<String>,
    /// Environment label for state records and env-scoped cache lookups
    /// (the CLI's `--environment`; Nessie provisioning stays a CLI step).
    pub environment: Option<String>,
    #[serde(default)]
    pub force: bool,
    pub run_tests: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestParams {
    #[serde(default)]
    pub selectors: Vec<String>,
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
    Test(TestParams),
    Promote(PromoteParams),
    Reload(ReloadParams),
}

impl Params {
    pub fn kind(&self) -> &'static str {
        match self {
            Params::Run(_) => "run",
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
                "force": p.force,
                "run_tests": p.run_tests,
            }),
            Params::Test(p) => json!({ "selectors": p.selectors }),
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
pub enum SubmitOutcome {
    /// A new operation was queued.
    New(OperationRecord),
    /// The idempotency key matched an existing operation.
    Replayed(OperationRecord),
    /// Another mutating operation holds the gate.
    Busy,
}

impl OperationStore {
    /// Register a queued operation. The idempotency lookup and the insert
    /// happen under one lock, so concurrent resubmissions of the same key
    /// cannot execute twice.
    pub fn submit(&self, params: &Params, key: Option<&str>) -> SubmitOutcome {
        let kind = params.kind().to_string();
        if let Some(key) = key {
            let mut idempotent = self.idempotent.lock().expect("idempotency lock");
            if let Some(existing) = idempotent
                .get(&(kind.clone(), key.to_string()))
                .and_then(|id| self.get(id))
            {
                return SubmitOutcome::Replayed(existing);
            }
            let Some(record) = self.register(params) else {
                return SubmitOutcome::Busy;
            };
            idempotent.insert((kind, key.to_string()), record.id.clone());
            return SubmitOutcome::New(record);
        }
        match self.register(params) {
            Some(record) => SubmitOutcome::New(record),
            None => SubmitOutcome::Busy,
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

    /// Insert the queued record; `None` when the gate is held.
    fn register(&self, params: &Params) -> Option<OperationRecord> {
        if let Some(false) = self.acquire(params.kind()) {
            return None;
        }
        let record = OperationRecord {
            id: uuid::Uuid::new_v4().to_string(),
            kind: params.kind().to_string(),
            status: OperationStatus::Queued,
            submitted_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            params: params.echo(),
            result: None,
            error: None,
        };
        self.ops.write().expect("ops lock").insert(
            record.id.clone(),
            OperationEntry {
                record: record.clone(),
                cancel: CancelHandle::default(),
            },
        );
        Some(record)
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
        if let Some(entry) = self.ops.write().expect("ops lock").get_mut(id) {
            if entry.record.status == OperationStatus::Queued {
                entry.record.status = OperationStatus::Running;
                entry.record.started_at = Some(now_rfc3339());
            }
        }
    }

    /// Finish an operation: result on success, error on failure, cancelled
    /// when the cancel flag was observed.
    pub fn finish(&self, id: &str, outcome: Result<Value, OperationError>) {
        let mut ops = self.ops.write().expect("ops lock");
        if let Some(entry) = ops.get_mut(id) {
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
        }
    }
}
