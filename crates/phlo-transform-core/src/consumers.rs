//! Registered non-transform consumers.
//!
//! The wider Phlo host can register published datasets, API outputs and other
//! consumers so impact analysis can include them without the transform engine
//! hard-coding host features.

use crate::semantic::ColumnRef;

/// Provides downstream consumers for a column.
pub trait ConsumerRegistry: Send + Sync {
    /// Human-readable consumer descriptions (for example `api: assay-results`).
    fn consumers(&self, column: &ColumnRef) -> Vec<String>;
}

/// A registry that reports no external consumers.
#[derive(Clone, Debug, Default)]
pub struct EmptyConsumerRegistry;

impl ConsumerRegistry for EmptyConsumerRegistry {
    fn consumers(&self, _column: &ColumnRef) -> Vec<String> {
        Vec::new()
    }
}

/// A fixed registry for tests and host integration.
#[derive(Clone, Debug, Default)]
pub struct StaticConsumerRegistry {
    entries: std::collections::BTreeMap<String, Vec<String>>,
}

impl StaticConsumerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, column: &ColumnRef, consumers: Vec<String>) -> &mut Self {
        self.entries.insert(column.display(), consumers);
        self
    }
}

impl ConsumerRegistry for StaticConsumerRegistry {
    fn consumers(&self, column: &ColumnRef) -> Vec<String> {
        self.entries
            .get(&column.display())
            .cloned()
            .unwrap_or_default()
    }
}
