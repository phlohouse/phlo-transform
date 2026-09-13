//! Failure classification and retry policy.
//!
//! Every execution failure carries a stable machine-readable
//! [`FailureCategory`] so callers never parse message strings. Retry
//! decisions are centralised in [`RetryPolicy`]: SQL-semantic failures are
//! never retried, transient adapter failures declared `retryable` by the
//! adapter may be, and test/cancellation/dependency/timeout failures are
//! not.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::AdapterError;
use crate::util::now_rfc3339;

/// A stable, machine-readable failure category.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// The adapter or warehouse failed (transport, internal adapter error).
    Adapter,
    /// The model's SQL was rejected (syntax, type, missing column/relation).
    Sql,
    /// A test assertion failed or the test query errored.
    Test,
    /// Execution exceeded its configured timeout.
    Timeout,
    /// The run or node was cancelled before completing.
    Cancelled,
    /// A required upstream (model or seed) did not produce its dataset, so
    /// this node never executed.
    Dependency,
    /// The state store failed.
    State,
    /// An internal engine invariant broke.
    #[default]
    Internal,
}

impl FailureCategory {
    /// Stable machine-readable code.
    pub fn code(self) -> &'static str {
        match self {
            FailureCategory::Adapter => "adapter",
            FailureCategory::Sql => "sql",
            FailureCategory::Test => "test",
            FailureCategory::Timeout => "timeout",
            FailureCategory::Cancelled => "cancelled",
            FailureCategory::Dependency => "dependency",
            FailureCategory::State => "state",
            FailureCategory::Internal => "internal",
        }
    }
}

/// A structured, inspectable failure attached to a result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub category: FailureCategory,
    /// Human-readable explanation.
    pub message: String,
    /// The adapter/database error code, when the failure came from the
    /// adapter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_code: Option<String>,
    /// The adapter/database error message, when it adds detail beyond
    /// `message`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_message: Option<String>,
    /// The attempt that produced this failure (1-based). `0` for failures
    /// that never reached an attempt (blocked, cancelled while queued).
    pub attempt: u32,
    /// RFC3339 timestamp.
    pub at: String,
    /// Whether the policy judged another attempt could succeed.
    pub retryable: bool,
}

impl Failure {
    /// A failure that never reached execution (blocked, cancelled while
    /// queued).
    pub fn without_attempt(category: FailureCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            adapter_code: None,
            adapter_message: None,
            attempt: 0,
            at: now_rfc3339(),
            retryable: false,
        }
    }
}

/// One execution attempt — recorded even when it fails, so run history shows
/// every try.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    /// 1-based attempt number.
    pub attempt: u32,
    pub started_at: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

/// Centralised retry policy: bounded exponential backoff, classified
/// per-failure.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Extra attempts after the first (`0` = try once).
    pub retries: u32,
    /// First retry delay; each retry doubles it.
    pub base_delay: Duration,
    /// Backoff ceiling.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            retries: 0,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// Total attempts a node may get (`1 + retries`).
    pub fn max_attempts(&self) -> u32 {
        self.retries.saturating_add(1)
    }

    /// Delay before the attempt following failed attempt `attempt`.
    pub fn delay(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        let millis = self
            .base_delay
            .as_millis()
            .saturating_mul(multiplier as u128)
            .min(self.max_delay.as_millis());
        Duration::from_millis(millis as u64)
    }

    /// Whether `failure` on attempt `attempt` may be retried. Only transient
    /// adapter failures are retried — SQL errors fail deterministically,
    /// tests are assertions, timeouts would likely recur, and
    /// dependency/cancellation states are scheduler decisions.
    pub fn should_retry(&self, failure: &Failure, attempt: u32) -> bool {
        failure.retryable
            && attempt < self.max_attempts()
            && matches!(failure.category, FailureCategory::Adapter)
    }
}

/// Adapter codes that mean "the SQL itself is wrong" — retrying can never
/// help. Anything else is treated as an adapter failure, retried only when
/// the adapter declared it retryable.
const SQL_CODES: &[&str] = &[
    "SYNTAX_ERROR",
    "PARSE_ERROR",
    "COLUMN_NOT_FOUND",
    "TABLE_NOT_FOUND",
    "SCHEMA_NOT_FOUND",
    "CATALOG_NOT_FOUND",
    "FUNCTION_NOT_FOUND",
    "TYPE_MISMATCH",
    "TYPE_NOT_FOUND",
    "AMBIGUOUS_NAME",
    "DUPLICATE_COLUMN_NAME",
    "DIVISION_BY_ZERO",
    "INVALID_FUNCTION_ARGUMENT",
];

/// Classify an adapter error into a category and retryability. The only
/// retryable class is transient adapter/transport failure — SQL-semantic
/// errors are never retried.
pub fn classify_adapter_error(error: &AdapterError, attempt: u32) -> Failure {
    let sql = SQL_CODES
        .iter()
        .any(|code| error.code.eq_ignore_ascii_case(code));
    let (category, retryable) = if sql {
        (FailureCategory::Sql, false)
    } else {
        (FailureCategory::Adapter, error.retryable)
    };
    Failure {
        category,
        message: error.to_string(),
        adapter_code: Some(error.code.clone()),
        adapter_message: Some(error.message.clone()),
        attempt,
        at: now_rfc3339(),
        retryable,
    }
}

/// A timeout failure for a bounded attempt.
pub fn timeout_failure(timeout: Duration, attempt: u32) -> Failure {
    Failure {
        category: FailureCategory::Timeout,
        message: format!("exceeded the {}s execution timeout", timeout.as_secs_f64()),
        adapter_code: None,
        adapter_message: None,
        attempt,
        at: now_rfc3339(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_errors_are_never_retried() {
        let policy = RetryPolicy {
            retries: 3,
            ..Default::default()
        };
        let failure =
            classify_adapter_error(&AdapterError::new("COLUMN_NOT_FOUND", "no column"), 1);
        assert_eq!(failure.category, FailureCategory::Sql);
        assert!(!failure.retryable);
        assert!(!policy.should_retry(&failure, 1));
    }

    #[test]
    fn retryable_adapter_errors_back_off_exponentially() {
        let policy = RetryPolicy {
            retries: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
        };
        assert_eq!(policy.max_attempts(), 4);
        assert_eq!(policy.delay(1), Duration::from_millis(100));
        assert_eq!(policy.delay(2), Duration::from_millis(200));
        assert_eq!(policy.delay(3), Duration::from_millis(400));
        assert_eq!(policy.delay(4), Duration::from_millis(800));
        assert_eq!(policy.delay(5), Duration::from_secs(1));

        let failure =
            classify_adapter_error(&AdapterError::new("TRINO_TRANSPORT", "boom").retryable(), 1);
        assert_eq!(failure.category, FailureCategory::Adapter);
        assert!(policy.should_retry(&failure, 1));
        assert!(policy.should_retry(&failure, 3));
        assert!(!policy.should_retry(&failure, 4));
    }

    #[test]
    fn non_retryable_adapter_errors_do_not_retry() {
        let policy = RetryPolicy {
            retries: 2,
            ..Default::default()
        };
        let failure = classify_adapter_error(&AdapterError::new("TRINO_AUTH", "denied"), 1);
        assert_eq!(failure.category, FailureCategory::Adapter);
        assert!(!policy.should_retry(&failure, 1));
    }
}
