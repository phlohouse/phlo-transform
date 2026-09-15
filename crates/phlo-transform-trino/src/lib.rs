//! A minimal Trino HTTP adapter.
//!
//! Implements only what Phase 1 needs: execute, create/replace view, create
//! table, existence checks, column metadata and cancellation. The Trino
//! protocol is simple enough to speak directly
//! (<https://trino.io/docs/current/develop/client-protocol.html>).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use phlo_transform_core::Relation;
use phlo_transform_engine::{
    Adapter, AdapterError, CatalogRequest, CatalogStatus, ColumnInfo, QueryResult,
};

/// Configuration for a Trino connection.
#[derive(Clone, Debug)]
pub struct TrinoConfig {
    /// Base endpoint, e.g. `http://localhost:8080`.
    pub endpoint: String,
    pub user: String,
    pub password: Option<String>,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub source: String,
    pub timeout: Duration,
}

impl TrinoConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            user: "phlo".to_string(),
            password: None,
            catalog: None,
            schema: None,
            source: "phlo-transform".to_string(),
            timeout: Duration::from_secs(300),
        }
    }

    pub fn with_basic_auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.user = user.into();
        self.password = Some(password.into());
        self
    }

    pub fn with_catalog(mut self, catalog: impl Into<String>) -> Self {
        self.catalog = Some(catalog.into());
        self
    }

    pub fn with_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }
}

/// A Trino-backed adapter.
pub struct TrinoAdapter {
    client: reqwest::Client,
    config: TrinoConfig,
    /// Query ids executing through this adapter instance. Entries are added
    /// as soon as Trino assigns an id and removed when the statement
    /// completes; if the caller drops the statement future, the id remains
    /// registered so [`TrinoAdapter::in_flight_queries`] can still name it
    /// for cancellation.
    in_flight: Arc<Mutex<BTreeSet<String>>>,
}

impl TrinoAdapter {
    pub fn new(config: TrinoConfig) -> Result<Self, AdapterError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| AdapterError::new("TRINO_CLIENT", error.to_string()))?;
        Ok(Self {
            client,
            config,
            in_flight: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    /// Query ids currently executing through this adapter — the set
    /// [`phlo_transform_engine::Adapter::in_flight_queries`] reports.
    pub fn in_flight_queries(&self) -> Vec<String> {
        self.in_flight
            .lock()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, url)
            .header("X-Trino-User", &self.config.user)
            .header("X-Trino-Source", &self.config.source);
        if let Some(catalog) = &self.config.catalog {
            request = request.header("X-Trino-Catalog", catalog);
        }
        if let Some(schema) = &self.config.schema {
            request = request.header("X-Trino-Schema", schema);
        }
        if let Some(password) = &self.config.password {
            request = request.basic_auth(&self.config.user, Some(password));
        }
        request
    }

    /// Execute a statement, following `nextUri` pages.
    pub async fn run(&self, sql: &str) -> Result<QueryResult, AdapterError> {
        let url = format!("{}/v1/statement", self.config.endpoint);
        let response = self
            .request(reqwest::Method::POST, &url)
            .header("Content-Type", "text/plain")
            .body(sql.to_string())
            .send()
            .await
            .map_err(|error| self.transport_error(error))?;

        let status = response.status();
        let payload: StatementResponse = response
            .json()
            .await
            .map_err(|error| self.transport_error(error))?;

        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(AdapterError::new(
                "TRINO_AUTH",
                "authentication was rejected by the Trino server",
            ));
        }

        // Register the id as soon as Trino assigns it. If this future is
        // dropped mid-flight (timeout, abort) the id stays registered so a
        // caller can still cancel the warehouse query; a completed
        // statement deregisters itself.
        let query_id = payload.id.clone();
        if let Some(id) = &query_id {
            if let Ok(mut set) = self.in_flight.lock() {
                set.insert(id.clone());
            }
        }
        let result = self.follow(payload, query_id.clone()).await;
        if let Some(id) = &query_id {
            if let Ok(mut set) = self.in_flight.lock() {
                set.remove(id);
            }
        }
        result
    }

    /// Follow `nextUri` pages until the statement finishes.
    async fn follow(
        &self,
        mut payload: StatementResponse,
        query_id: Option<String>,
    ) -> Result<QueryResult, AdapterError> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();

        loop {
            if let Some(error) = payload.error.take() {
                return Err(AdapterError::new(error.code(), error.message()));
            }
            if let Some(descriptions) = payload.columns.take() {
                columns = descriptions
                    .into_iter()
                    .map(|description| description.name)
                    .collect();
            }
            if let Some(data) = payload.data.take() {
                for row in data {
                    rows.push(row.into_iter().map(value_to_string).collect());
                }
            }

            let Some(next) = payload.next_uri.take() else {
                break;
            };
            let response = self
                .request(reqwest::Method::GET, &next)
                .send()
                .await
                .map_err(|error| self.transport_error(error))?;
            payload = response
                .json()
                .await
                .map_err(|error| self.transport_error(error))?;
        }

        Ok(QueryResult {
            query_id,
            columns,
            row_count: rows.len() as u64,
            rows,
        })
    }

    fn transport_error(&self, error: reqwest::Error) -> AdapterError {
        AdapterError::new("TRINO_TRANSPORT", error.to_string()).retryable()
    }

    /// The latest Iceberg snapshot id, when the relation is an Iceberg
    /// table — the only strong content identity Trino can prove.
    async fn iceberg_snapshot(&self, relation: &Relation) -> Option<String> {
        let snapshots = Relation {
            catalog: relation.catalog.clone(),
            schema: relation.schema.clone(),
            table: format!("{}$snapshots", relation.table),
        };
        self.run(&format!(
            "SELECT snapshot_id FROM {} ORDER BY committed_at DESC LIMIT 1",
            snapshots.sql()
        ))
        .await
        .ok()
        .and_then(|result| result.rows.first().and_then(|row| row.first()).cloned())
        .map(|value| format!("snapshot:{value}"))
    }
}

#[async_trait]
impl Adapter for TrinoAdapter {
    fn name(&self) -> &str {
        "trino"
    }

    async fn relation_exists(&self, relation: &Relation) -> Result<bool, AdapterError> {
        let sql = format!("SELECT 1 FROM {} LIMIT 0", relation.sql());
        match self.run(&sql).await {
            Ok(_) => Ok(true),
            Err(error) if is_missing_relation(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Existence from `information_schema.tables` — one query per
    /// catalog/schema group instead of one analyzed `SELECT` per relation.
    /// Relations without an explicit catalog cannot be placed in a catalog's
    /// information_schema; they keep the per-relation probe. A failed group
    /// query falls back to probing that group serially.
    async fn relations_exist(&self, relations: &[Relation]) -> Result<Vec<bool>, AdapterError> {
        let mut found = vec![false; relations.len()];
        let mut groups: std::collections::BTreeMap<(&str, &str), Vec<usize>> =
            std::collections::BTreeMap::new();
        for (index, relation) in relations.iter().enumerate() {
            if let Some(catalog) = &relation.catalog {
                groups
                    .entry((catalog.as_str(), relation.schema.as_str()))
                    .or_default()
                    .push(index);
            }
        }
        let mut probed = vec![false; relations.len()];
        for ((catalog, schema), indexes) in &groups {
            let names = indexes
                .iter()
                .map(|index| literal(&relations[*index].table))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT table_name FROM {}.information_schema.tables \
                 WHERE table_schema = {} AND table_name IN ({names})",
                quote(catalog),
                literal(schema),
            );
            match self.run(&sql).await {
                Ok(result) => {
                    let present: BTreeSet<&str> = result
                        .rows
                        .iter()
                        .filter_map(|row| row.first().map(String::as_str))
                        .collect();
                    for &index in indexes {
                        probed[index] = true;
                        found[index] = present.contains(relations[index].table.as_str());
                    }
                }
                Err(_) => {
                    for &index in indexes {
                        found[index] = self.relation_exists(&relations[index]).await?;
                        probed[index] = true;
                    }
                }
            }
        }
        for (index, relation) in relations.iter().enumerate() {
            if probed[index] {
                continue;
            }
            found[index] = self.relation_exists(relation).await?;
        }
        Ok(found)
    }

    async fn execute(&self, sql: &str) -> Result<QueryResult, AdapterError> {
        self.run(sql).await
    }

    async fn create_or_replace_view(
        &self,
        relation: &Relation,
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        self.run(&format!(
            "CREATE OR REPLACE VIEW {} AS {}",
            relation.sql(),
            sql
        ))
        .await
    }

    async fn create_or_replace_table(
        &self,
        relation: &Relation,
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        self.run(&format!("DROP TABLE IF EXISTS {}", relation.sql()))
            .await?;
        self.run(&format!("CREATE TABLE {} AS {}", relation.sql(), sql))
            .await
    }

    async fn append(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError> {
        self.run(&format!("INSERT INTO {} {}", relation.sql(), sql))
            .await
    }

    async fn merge(
        &self,
        relation: &Relation,
        key_columns: &[String],
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let columns = self.relation_columns(relation).await?;
        if columns.is_empty() {
            return Err(AdapterError::new(
                "TRINO_MERGE",
                format!(
                    "cannot merge into {}: relation columns are unknown",
                    relation.sql()
                ),
            ));
        }
        let update: Vec<String> = columns
            .iter()
            .filter(|column| {
                !key_columns
                    .iter()
                    .any(|key| key.eq_ignore_ascii_case(&column.name))
            })
            .map(|column| format!("{} = s.{}", quote(&column.name), quote(&column.name)))
            .collect();
        let insert_columns: Vec<String> =
            columns.iter().map(|column| quote(&column.name)).collect();
        let insert_values: Vec<String> = columns
            .iter()
            .map(|column| format!("s.{}", quote(&column.name)))
            .collect();
        let on: Vec<String> = key_columns
            .iter()
            .map(|key| format!("t.{} = s.{}", quote(key), quote(key)))
            .collect();

        let mut merge = format!(
            "MERGE INTO {} t USING ({}) s ON ({})",
            relation.sql(),
            sql,
            on.join(" AND ")
        );
        if !update.is_empty() {
            merge.push_str(&format!(
                " WHEN MATCHED THEN UPDATE SET {}",
                update.join(", ")
            ));
        }
        merge.push_str(&format!(
            " WHEN NOT MATCHED THEN INSERT ({}) VALUES ({})",
            insert_columns.join(", "),
            insert_values.join(", ")
        ));
        self.run(&merge).await
    }

    async fn replace_partitions(
        &self,
        relation: &Relation,
        partition_columns: &[String],
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let columns: Vec<String> = partition_columns
            .iter()
            .map(|column| quote(column))
            .collect();
        let columns = columns.join(", ");
        self.run(&format!(
            "DELETE FROM {} WHERE ({columns}) IN (SELECT {columns} FROM ({sql}) AS __phlo_src)",
            relation.sql()
        ))
        .await?;
        self.run(&format!(
            "INSERT INTO {} SELECT * FROM ({sql}) AS __phlo_src",
            relation.sql()
        ))
        .await
    }

    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError> {
        let url = format!("{}/v1/query/{}", self.config.endpoint, query_id);
        let response = self
            .request(reqwest::Method::DELETE, &url)
            .send()
            .await
            .map_err(|error| self.transport_error(error))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(AdapterError::new(
                "TRINO_CANCEL",
                format!("cancel returned HTTP {}", response.status()),
            ))
        }
    }

    /// A tracked view shares the client and config but gets a fresh
    /// in-flight registry, so it reports only the queries started through
    /// it — exactly the set one model attempt can cancel.
    fn track_attempt(&self) -> Option<Arc<dyn Adapter>> {
        Some(Arc::new(Self {
            client: self.client.clone(),
            config: self.config.clone(),
            in_flight: Arc::new(Mutex::new(BTreeSet::new())),
        }))
    }

    fn in_flight_queries(&self) -> Vec<String> {
        TrinoAdapter::in_flight_queries(self)
    }

    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError> {
        let result = self.run(&format!("DESCRIBE {}", relation.sql())).await?;
        Ok(result
            .rows
            .into_iter()
            .filter_map(|row| {
                let name = row.first()?.clone();
                let data_type = row.get(1).cloned().unwrap_or_default();
                Some(ColumnInfo {
                    name,
                    data_type,
                    nullable: true,
                })
            })
            .collect())
    }

    /// Column metadata from `information_schema.columns` — one query per
    /// catalog/schema group instead of a `DESCRIBE` per relation. A failed
    /// group query falls back to serial `DESCRIBE`; relations without an
    /// explicit catalog always describe serially.
    async fn relation_columns_many(
        &self,
        relations: &[Relation],
    ) -> Vec<Result<Vec<ColumnInfo>, AdapterError>> {
        let mut out: Vec<Result<Vec<ColumnInfo>, AdapterError>> = relations
            .iter()
            .map(|_| Err(AdapterError::new("UNPROBED", "not probed")))
            .collect();
        let mut groups: std::collections::BTreeMap<(&str, &str), Vec<usize>> =
            std::collections::BTreeMap::new();
        for (index, relation) in relations.iter().enumerate() {
            if let Some(catalog) = &relation.catalog {
                groups
                    .entry((catalog.as_str(), relation.schema.as_str()))
                    .or_default()
                    .push(index);
            }
        }
        let mut probed = vec![false; relations.len()];
        for ((catalog, schema), indexes) in &groups {
            let names = indexes
                .iter()
                .map(|index| literal(&relations[*index].table))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT table_name, column_name, data_type \
                 FROM {}.information_schema.columns \
                 WHERE table_schema = {} AND table_name IN ({names}) \
                 ORDER BY table_name, ordinal_position",
                quote(catalog),
                literal(schema),
            );
            if let Ok(result) = self.run(&sql).await {
                let mut by_table: std::collections::BTreeMap<String, Vec<ColumnInfo>> =
                    std::collections::BTreeMap::new();
                for row in &result.rows {
                    let (Some(table), Some(name)) = (row.first(), row.get(1)) else {
                        continue;
                    };
                    by_table.entry(table.clone()).or_default().push(ColumnInfo {
                        name: name.clone(),
                        data_type: row.get(2).cloned().unwrap_or_default(),
                        nullable: true,
                    });
                }
                for &index in indexes {
                    // A table absent from the result does not exist (Trino
                    // tables always have columns) — mirror `DESCRIBE`'s
                    // not-found error rather than reporting empty columns.
                    out[index] = match by_table.remove(&relations[index].table) {
                        Some(columns) => Ok(columns),
                        None => Err(AdapterError::new(
                            "TABLE_NOT_FOUND",
                            format!("{} does not exist", relations[index].display()),
                        )),
                    };
                    probed[index] = true;
                }
            }
        }
        for (index, relation) in relations.iter().enumerate() {
            if probed[index] {
                continue;
            }
            out[index] = self.relation_columns(relation).await;
        }
        out
    }

    fn supports_catalog_provisioning(&self) -> bool {
        // Trino provisions dynamic catalogs — `ensure_catalog` issues
        // `CREATE CATALOG` bound to the candidate's Nessie ref.
        true
    }

    async fn ensure_catalog(
        &self,
        request: &CatalogRequest,
    ) -> Result<CatalogStatus, AdapterError> {
        let (Some(reference), Some(nessie_uri)) = (&request.reference, &request.nessie_uri) else {
            return Ok(CatalogStatus::Unmanaged);
        };
        match self.catalog_connector(&request.catalog).await? {
            // An existing catalog is NOT accepted on name alone: the ref it
            // is bound to cannot be read back over SQL, so the caller must
            // prove the binding from recorded evidence — or refuse it. A
            // non-Iceberg catalog under the name is never acceptable.
            Some(connector) if connector != "iceberg" => Err(AdapterError::new(
                "CATALOG_CONFLICT",
                format!(
                    "catalog `{}` already exists as a `{connector}` catalog, not an \
                     Iceberg/Nessie catalog bound to `{reference}`",
                    request.catalog
                ),
            )),
            Some(_) => Ok(CatalogStatus::Unverified),
            None => {
                let warehouse = request
                    .warehouse
                    .clone()
                    .unwrap_or_else(|| "local:///tmp/phlo-warehouse".to_string());
                let mut properties = vec![
                    "\"iceberg.catalog.type\"='nessie'".to_string(),
                    format!(
                        "\"iceberg.nessie-catalog.uri\"={}",
                        literal(&format!("{}/api/v2", nessie_uri.trim_end_matches('/')))
                    ),
                    format!("\"iceberg.nessie-catalog.ref\"={}", literal(reference)),
                    format!(
                        "\"iceberg.nessie-catalog.default-warehouse-dir\"={}",
                        literal(&warehouse)
                    ),
                ];
                if warehouse.starts_with("local://") || warehouse.starts_with('/') {
                    properties.push("\"fs.local.enabled\"='true'".to_string());
                }
                self.run(&format!(
                    "CREATE CATALOG {} USING iceberg WITH ({})",
                    quote(&request.catalog),
                    properties.join(", ")
                ))
                .await?;
                Ok(CatalogStatus::Created)
            }
        }
    }

    async fn drop_catalog(&self, catalog: &str) -> Result<(), AdapterError> {
        self.run(&format!("DROP CATALOG IF EXISTS {}", quote(catalog)))
            .await?;
        Ok(())
    }

    async fn ensure_schema(&self, relation: &Relation) -> Result<(), AdapterError> {
        let schema = match &relation.catalog {
            Some(catalog) => format!("{}.{}", quote(catalog), quote(&relation.schema)),
            None => quote(&relation.schema),
        };
        self.run(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
            .await?;
        Ok(())
    }

    async fn source_state(&self, relation: &Relation) -> Result<Option<String>, AdapterError> {
        // Iceberg snapshot state, when the relation is an Iceberg table.
        if let Some(snapshot) = self.iceberg_snapshot(relation).await {
            return Ok(Some(snapshot));
        }

        // Fallback: a stable fingerprint of the relation's schema, so schema
        // changes remain observable for non-Iceberg sources.
        if let Ok(columns) = self.relation_columns(relation).await {
            if !columns.is_empty() {
                let parts = columns
                    .iter()
                    .map(|column| format!("{}:{}", column.name, column.data_type));
                return Ok(Some(format!("schema:{}", fingerprint(parts))));
            }
        }
        Ok(None)
    }

    /// For Iceberg the snapshot id is a strong identity: an unchanged value
    /// proves the same physical output. Non-Iceberg relations have no
    /// provable identity — schema fingerprints describe shape, not bytes.
    async fn output_identity(&self, relation: &Relation) -> Result<Option<String>, AdapterError> {
        Ok(self.iceberg_snapshot(relation).await)
    }

    async fn partition_counts(
        &self,
        relation: &Relation,
        _partition_columns: &[String],
    ) -> Result<Option<Vec<(String, i64)>>, AdapterError> {
        let partitions = Relation {
            catalog: relation.catalog.clone(),
            schema: relation.schema.clone(),
            table: format!("{}$partitions", relation.table),
        };
        // Trino exposes the partition tuple as a `partition` column plus
        // `record_count` from Iceberg metadata.
        match self
            .run(&format!(
                "SELECT partition, record_count FROM {}",
                partitions.sql()
            ))
            .await
        {
            Ok(result) => Ok(Some(
                result
                    .rows
                    .iter()
                    .filter_map(|row| {
                        let key = row.first()?.clone();
                        let count: i64 = row.get(1)?.parse().ok()?;
                        Some((key, count))
                    })
                    .collect(),
            )),
            Err(_) => Ok(None),
        }
    }
}

impl TrinoAdapter {
    /// The connector behind a catalog name (`iceberg`, `memory`, ...) —
    /// `None` when no catalog of that name exists. Trino's
    /// `system.metadata.catalogs` does not expose the catalog's configured
    /// Nessie ref, so existence plus connector is the strongest claim SQL
    /// can make; the engine treats that as [`CatalogStatus::Unverified`]
    /// rather than proof of binding.
    async fn catalog_connector(&self, catalog: &str) -> Result<Option<String>, AdapterError> {
        let query = format!(
            "SELECT connector_name FROM system.metadata.catalogs WHERE catalog_name = '{}'",
            catalog.replace('\'', "''")
        );
        let result = self.run(&query).await?;
        Ok(result.rows.first().and_then(|row| row.first()).cloned())
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// A SQL string literal — single quotes doubled per the SQL standard.
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Deterministic FNV-1a fingerprint of a sequence of strings.
fn fingerprint<I, S>(parts: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut hash: u64 = 0xcbf29ce484222325;
    for part in parts {
        for byte in part.as_ref().bytes().chain(std::iter::once(b'\n')) {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn is_missing_relation(error: &AdapterError) -> bool {
    matches!(
        error.code.as_str(),
        "TABLE_NOT_FOUND" | "SCHEMA_NOT_FOUND" | "CATALOG_NOT_FOUND" | "NOT_FOUND"
    ) || error
        .message
        .to_ascii_lowercase()
        .contains("does not exist")
}

fn value_to_string(value: Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::String(value) => value,
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        other => other.to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct StatementResponse {
    id: Option<String>,
    #[serde(rename = "nextUri")]
    next_uri: Option<String>,
    columns: Option<Vec<ColumnDescription>>,
    data: Option<Vec<Vec<Value>>>,
    error: Option<TrinoErrorPayload>,
}

#[derive(Debug, Deserialize)]
struct ColumnDescription {
    name: String,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    data_type: String,
}

#[derive(Debug, Deserialize)]
struct TrinoErrorPayload {
    message: Option<String>,
    #[serde(rename = "errorName")]
    error_name: Option<String>,
    #[serde(rename = "errorCode")]
    error_code: Option<i64>,
}

impl TrinoErrorPayload {
    fn code(&self) -> String {
        self.error_name
            .clone()
            .unwrap_or_else(|| format!("TRINO_{}", self.error_code.unwrap_or(0)))
    }

    fn message(&self) -> String {
        self.message
            .clone()
            .unwrap_or_else(|| "Trino query failed".into())
    }
}
