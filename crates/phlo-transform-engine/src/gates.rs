//! Promotion gates.
//!
//! A promotion is authorised by a list of named gates, each producing a
//! `PASS`/`FAIL` result with a human detail. Human output and JSON are
//! derived from the same `GateReport`.
//!
//! ```text
//! PASS run       — run run-3f1 passed on ref ci/pr-1
//! PASS tests     — 2 tests passed
//! PASS blocked   — no blocked or cancelled work
//! PASS schema    — no breaking schema changes
//! FAIL data_diff — diff artifact is stale: candidate data changed
//! FAIL base      — target main advanced (expected aaa, found bbb)
//! PASS conflicts — candidate merges cleanly into main
//! ```

use serde::{Deserialize, Serialize};

use phlo_transform_nessie::MergeOutcome;

use crate::events::ExecutionStatus;
use crate::state::{ModelRunRecord, RunSummary, TestRunRecord};

/// A single gate result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// The evaluated gate set.
#[derive(Clone, Debug, Serialize)]
pub struct GateReport {
    pub results: Vec<GateResult>,
    pub passed: bool,
}

impl GateReport {
    /// The failed gates, for error messages.
    pub fn failures(&self) -> impl Iterator<Item = &GateResult> {
        self.results.iter().filter(|result| !result.passed)
    }
}

/// Everything gate evaluation needs, gathered by the caller.
#[derive(Clone, Debug, Default)]
pub struct GateInput {
    /// The latest run recorded for the candidate reference.
    pub run: Option<RunSummary>,
    /// Model records of that run.
    pub model_runs: Vec<ModelRunRecord>,
    /// Test records of that run.
    pub test_runs: Vec<TestRunRecord>,
    /// Whether a passing data diff is required.
    pub require_diff: bool,
    /// The audited diff's verdict, when one was recorded.
    pub diff_passed: Option<bool>,
    /// Why the audited diff cannot be used (stale artifact), when applicable.
    pub diff_rejected: Option<String>,
    /// Breaking schema changes found by the audit.
    pub breaking_schema_changes: Vec<String>,
    /// Explicit waiver for breaking schema changes.
    pub allow_breaking_schema: bool,
    /// The target hash observed when the candidate was provisioned.
    pub expected_target_hash: Option<String>,
    /// The target hash right now.
    pub actual_target_hash: Option<String>,
    /// A non-destructive merge check, when one was performed.
    pub merge_check: Option<MergeOutcome>,
}

fn gate(name: &str, passed: bool, detail: impl Into<String>) -> GateResult {
    GateResult {
        name: name.to_string(),
        passed,
        detail: detail.into(),
    }
}

/// Evaluate every promotion gate against the gathered evidence.
///
/// The evaluation is pure and order-stable: gate order is fixed so human and
/// JSON output agree.
pub fn evaluate_gates(input: &GateInput) -> GateReport {
    let mut results = Vec::new();

    // run: the candidate must have a finished, fully successful run.
    match &input.run {
        Some(run) if run.status == ExecutionStatus::Passed && run.failed_count == 0 => {
            results.push(gate("run", true, format!("run {} passed", run.run_id)));
        }
        Some(run) => results.push(gate(
            "run",
            false,
            format!(
                "latest run {} {} ({} failed)",
                run.run_id,
                run.status.label(),
                run.failed_count
            ),
        )),
        None => results.push(gate(
            "run",
            false,
            "no recorded run for the candidate reference".to_string(),
        )),
    }

    // tests: no test may have failed in the candidate run. Vacuously true
    // when the run had no tests.
    let failed_tests: Vec<&TestRunRecord> = input
        .test_runs
        .iter()
        .filter(|record| record.status == ExecutionStatus::Failed)
        .collect();
    if input.run.is_none() {
        results.push(gate("tests", false, "no run to audit".to_string()));
    } else if failed_tests.is_empty() {
        results.push(gate(
            "tests",
            true,
            format!("{} tests passed", input.test_runs.len()),
        ));
    } else {
        results.push(gate(
            "tests",
            false,
            format!(
                "{} tests failed: {}",
                failed_tests.len(),
                failed_tests
                    .iter()
                    .map(|record| record.test_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    // blocked: no blocked or cancelled model work may remain.
    let blocked: Vec<&ModelRunRecord> = input
        .model_runs
        .iter()
        .filter(|record| {
            matches!(
                record.status,
                ExecutionStatus::Blocked | ExecutionStatus::Cancelled
            )
        })
        .collect();
    if input.run.is_none() {
        results.push(gate("blocked", false, "no run to audit".to_string()));
    } else if blocked.is_empty() {
        results.push(gate("blocked", true, "no blocked or cancelled work"));
    } else {
        results.push(gate(
            "blocked",
            false,
            format!(
                "{} models blocked or cancelled: {}",
                blocked.len(),
                blocked
                    .iter()
                    .map(|record| record.model_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    // schema: breaking schema changes block unless explicitly waived.
    if input.breaking_schema_changes.is_empty() {
        results.push(gate("schema", true, "no breaking schema changes"));
    } else if input.allow_breaking_schema {
        results.push(gate(
            "schema",
            true,
            format!(
                "{} breaking changes waived",
                input.breaking_schema_changes.len()
            ),
        ));
    } else {
        results.push(gate(
            "schema",
            false,
            format!(
                "breaking schema changes: {}",
                input.breaking_schema_changes.join(", ")
            ),
        ));
    }

    // data_diff: only evaluated when a diff is required.
    if input.require_diff {
        if let Some(rejected) = &input.diff_rejected {
            results.push(gate("data_diff", false, rejected.clone()));
        } else {
            match input.diff_passed {
                Some(true) => results.push(gate("data_diff", true, "data diff passed")),
                Some(false) => results.push(gate("data_diff", false, "data diff failed policy")),
                None => results.push(gate(
                    "data_diff",
                    false,
                    "a passing data diff is required before promotion".to_string(),
                )),
            }
        }
    }

    // base: the target must not have moved since the candidate branched.
    match (&input.expected_target_hash, &input.actual_target_hash) {
        (Some(expected), Some(actual)) if expected != actual => results.push(gate(
            "base",
            false,
            format!("target advanced since planning (expected {expected}, found {actual})"),
        )),
        (Some(_), Some(_)) => results.push(gate("base", true, "target unchanged since planning")),
        _ => results.push(gate(
            "base",
            true,
            "no recorded base hash — target freshness not verified".to_string(),
        )),
    }

    // conflicts: the non-destructive merge check must be clean.
    match &input.merge_check {
        Some(check) if check.is_clean() => {
            results.push(gate("conflicts", true, "candidate merges cleanly"))
        }
        Some(check) => results.push(gate(
            "conflicts",
            false,
            check
                .conflicts
                .iter()
                .map(|conflict| format!("{}: {}", conflict.path, conflict.message))
                .collect::<Vec<_>>()
                .join("; "),
        )),
        None => results.push(gate(
            "conflicts",
            false,
            "merge check was not performed".to_string(),
        )),
    }

    let passed = results.iter().all(|result| result.passed);
    GateReport { results, passed }
}
