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

/// A request to provision a warehouse catalog for a Nessie reference.
#[derive(Clone, Debug, Default)]
pub struct CatalogRequest {
    pub catalog: String,
    /// Nessie branch the catalog should read and write.
    pub reference: Option<String>,
    /// Nessie API base URI, e.g. `http://nessie:19120`.
    pub nessie_uri: Option<String>,
    /// Warehouse location, e.g. `local:///tmp/warehouse` or `s3://bucket/wh`.
    pub warehouse: Option<String>,
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

    /// Append rows from `sql` into an existing relation.
    async fn append(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError>;

    /// Merge rows from `sql` into an existing relation on the key columns.
    async fn merge(
        &self,
        relation: &Relation,
        key_columns: &[String],
        sql: &str,
    ) -> Result<QueryResult, AdapterError>;

    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError>;

    /// Read column metadata for an existing relation.
    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError>;

    /// Ensure a catalog exists for a Nessie reference. Adapters that do not
    /// support catalog provisioning treat this as a no-op.
    async fn ensure_catalog(&self, request: &CatalogRequest) -> Result<(), AdapterError>;

    /// Ensure the schema/namespace containing a relation exists.
    async fn ensure_schema(&self, relation: &Relation) -> Result<(), AdapterError>;
}
