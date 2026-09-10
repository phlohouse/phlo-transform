//! The adapter boundary.
//!
//! Deliberately compact: only what Phase 1 needs. Dialect-specific DDL lives
//! in the adapter, not the engine.

use async_trait::async_trait;

use phlo_transform_core::Relation;

use crate::error::AdapterError;

/// The result of executing SQL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryResult {
    /// Warehouse query identifier, when available.
    pub query_id: Option<String>,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Number of rows returned, when known.
    pub row_count: u64,
}

/// Basic column metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

/// A SQL execution target (Trino, or a fake adapter in tests).
#[async_trait]
pub trait Adapter: Send + Sync {
    /// A short adapter name, for reporting.
    fn name(&self) -> &str;

    async fn relation_exists(&self, relation: &Relation) -> Result<bool, AdapterError>;

    async fn execute(&self, sql: &str) -> Result<QueryResult, AdapterError>;

    async fn create_or_replace_view(
        &self,
        relation: &Relation,
        sql: &str,
    ) -> Result<QueryResult, AdapterError>;

    async fn create_or_replace_table(
        &self,
        relation: &Relation,
        sql: &str,
    ) -> Result<QueryResult, AdapterError>;

    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError>;

    /// Read column metadata for an existing relation.
    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError>;
}
