//! The adapter boundary.
//!
//! Deliberately compact: only what Phase 1 needs. Dialect-specific DDL lives
//! in the adapter, not the engine.

use std::path::Path;
use std::sync::Arc;

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

/// What `ensure_catalog` established about the requested catalog.
///
/// The distinction matters for branch isolation: a candidate catalog is
/// expected to be bound to the candidate's Nessie reference, but no adapter
/// can introspect an existing catalog's configured ref over SQL — so
/// "the catalog exists" must never silently mean "the catalog is correct".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    /// The catalog was created by this call, bound to `request.reference`.
    Created,
    /// The catalog already existed; the adapter could not prove which
    /// Nessie reference it serves. The caller must establish the binding
    /// from recorded evidence — or refuse to use it.
    #[default]
    Unverified,
    /// The adapter does not provision catalogs (or no reference/URI was
    /// supplied) — there is nothing to verify.
    Unmanaged,
}

/// A SQL execution target (Trino, or a fake adapter in tests).
#[async_trait]
pub trait Adapter: Send + Sync {
    /// A short adapter name, for reporting.
    fn name(&self) -> &str;

    async fn relation_exists(&self, relation: &Relation) -> Result<bool, AdapterError>;

    /// Existence probes for a batch of relations — positions in the result
    /// match `relations`. Adapters that can answer from catalog metadata
    /// (Trino's `information_schema`) override this to collapse N probes
    /// into a handful of queries; the default is the honest serial loop.
    async fn relations_exist(&self, relations: &[Relation]) -> Result<Vec<bool>, AdapterError> {
        let mut out = Vec::with_capacity(relations.len());
        for relation in relations {
            out.push(self.relation_exists(relation).await?);
        }
        Ok(out)
    }

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

    /// Replace the partitions (identified by `partition_columns`) that appear
    /// in `sql`, leaving other partitions untouched.
    async fn replace_partitions(
        &self,
        relation: &Relation,
        partition_columns: &[String],
        sql: &str,
    ) -> Result<QueryResult, AdapterError>;

    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError>;

    /// A view of this adapter that tracks the queries started through it,
    /// so the runner can cancel this attempt's in-flight warehouse work on
    /// timeout or shutdown. `None` (the default) means the adapter cannot
    /// report in-flight query ids — cancellation then degrades to dropping
    /// the attempt's future.
    fn track_attempt(&self) -> Option<Arc<dyn Adapter>> {
        None
    }

    /// Query ids currently executing through a tracked attempt view —
    /// only meaningful on adapters returned by [`Adapter::track_attempt`].
    /// The default is empty (untracked).
    fn in_flight_queries(&self) -> Vec<String> {
        Vec::new()
    }

    /// Read column metadata for an existing relation.
    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError>;

    /// Column metadata for a batch of relations — per-relation results, so
    /// one unreadable relation does not sink the batch (the planner treats
    /// unknown columns as absent evidence, not a failure). Adapters with a
    /// queryable catalog (Trino's `information_schema.columns`) override to
    /// collapse the probes; the default probes serially.
    async fn relation_columns_many(
        &self,
        relations: &[Relation],
    ) -> Vec<Result<Vec<ColumnInfo>, AdapterError>> {
        let mut out = Vec::with_capacity(relations.len());
        for relation in relations {
            out.push(self.relation_columns(relation).await);
        }
        out
    }

    /// Whether `ensure_catalog` can provision a catalog bound to a Nessie
    /// reference (dynamic catalog management). `false` — the default — is
    /// the honest answer for adapters without it. Environment resolution
    /// reads this to fail a read-only preview whose generated catalog the
    /// adapter could never create, instead of previewing a target a run
    /// would refuse.
    fn supports_catalog_provisioning(&self) -> bool {
        false
    }

    /// Ensure a catalog exists for a Nessie reference, reporting what was
    /// established — see [`CatalogStatus`]. Adapters that do not support
    /// catalog provisioning return [`CatalogStatus::Unmanaged`]. An adapter
    /// must never report an existing catalog as correct when it cannot
    /// prove the binding; it reports `Unverified` instead and the caller
    /// decides from recorded evidence.
    async fn ensure_catalog(&self, request: &CatalogRequest)
        -> Result<CatalogStatus, AdapterError>;

    /// Drop a catalog the caller has proven phlo owns. Identifier quoting
    /// is the adapter's business — the engine never builds catalog names
    /// into raw SQL. Adapters that cannot drop catalogs report
    /// `UNSUPPORTED`; cleanup records that as a failure rather than
    /// staying silent about a leftover catalog.
    async fn drop_catalog(&self, catalog: &str) -> Result<(), AdapterError> {
        let _ = catalog;
        Err(AdapterError::new(
            "UNSUPPORTED",
            format!("{} does not drop catalogs", self.name()),
        ))
    }

    /// Ensure the schema/namespace containing a relation exists.
    async fn ensure_schema(&self, relation: &Relation) -> Result<(), AdapterError>;

    /// Observe a source state used for model versioning. For Iceberg this is
    /// the latest snapshot id; for other relations it may be `None`.
    async fn source_state(&self, relation: &Relation) -> Result<Option<String>, AdapterError>;

    /// A strong physical identity of the relation's contents: if this value
    /// is unchanged, the physical materialisation is provably the same
    /// output (for example an Iceberg snapshot id). This is what
    /// cross-environment cache reuse keys on — a weaker fingerprint such as
    /// a schema hash does not prove the bytes are still there. `None` means
    /// the adapter cannot prove physical identity; the default fails closed.
    async fn output_identity(&self, relation: &Relation) -> Result<Option<String>, AdapterError> {
        let _ = relation;
        Ok(None)
    }

    /// Per-partition record counts from metadata, when the adapter supports it
    /// (for example Iceberg `$partitions`). `None` means unsupported.
    async fn partition_counts(
        &self,
        relation: &Relation,
        partition_columns: &[String],
    ) -> Result<Option<Vec<(String, i64)>>, AdapterError>;

    /// Load a CSV seed file into a relation, replacing it. Adapters that
    /// cannot ingest CSVs report `UNSUPPORTED` (the default).
    async fn load_csv(
        &self,
        relation: &Relation,
        path: &Path,
    ) -> Result<QueryResult, AdapterError> {
        let _ = (relation, path);
        Err(AdapterError::new(
            "UNSUPPORTED",
            format!("{} does not support CSV seeds", self.name()),
        ))
    }
}
