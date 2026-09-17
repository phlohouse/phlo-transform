//! A minimal Trino HTTP adapter.
//!
//! Implements only what Phase 1 needs: execute, create/replace view, create
//! table, existence checks, column metadata and cancellation. The Trino
//! protocol is simple enough to speak directly
//! (<https://trino.io/docs/current/develop/client-protocol.html>).

use std::collections::BTreeSet;
use std::path::Path;
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
                return Err(error.into_adapter_error());
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
        let sql = format!(
            "SELECT snapshot_id FROM {} ORDER BY committed_at DESC LIMIT 1",
            snapshots.sql()
        );
        // A missing relation is a real answer (`None`); anything else is a
        // transient — queueing, a worker hiccup, a Nessie blip — that must
        // not masquerade as "no verifiable identity" and demote an
        // unchanged model to a rebuild.
        let mut result = self.run(&sql).await;
        for delay in [150, 400] {
            match &result {
                Err(error) if !is_missing_relation(error) => {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    result = self.run(&sql).await;
                }
                _ => break,
            }
        }
        result
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

    /// CSV seeds are local files Trino cannot read, so the adapter loads
    /// them itself: infer a column type per column (all values empty, an
    /// integer, or a number → `bigint`/`double`; `true`/`false` →
    /// `boolean`; anything else → `varchar`), create the table, then insert
    /// rows in batches. Seeds are small reference inputs by contract — this
    /// is not a bulk-load path.
    async fn load_csv(
        &self,
        relation: &Relation,
        path: &Path,
    ) -> Result<QueryResult, AdapterError> {
        let mut reader = csv::Reader::from_path(path)
            .map_err(|error| AdapterError::new("SEED_IO", error.to_string()))?;
        let headers: Vec<String> = reader
            .headers()
            .map_err(|error| AdapterError::new("SEED_IO", error.to_string()))?
            .iter()
            .map(str::to_string)
            .collect();
        if headers.is_empty() {
            return Err(AdapterError::new(
                "SEED_IO",
                format!("{} has no header row", path.display()),
            ));
        }
        let mut rows: Vec<Vec<String>> = Vec::new();
        for record in reader.records() {
            let record = record.map_err(|error| AdapterError::new("SEED_IO", error.to_string()))?;
            rows.push(record.iter().map(str::to_string).collect());
        }

        let columns: Vec<SeedColumn> = headers
            .iter()
            .enumerate()
            .map(|(index, name)| SeedColumn {
                name: name.clone(),
                data_type: infer_csv_type(rows.iter().filter_map(|row| row.get(index))),
            })
            .collect();
        let ddl = format!(
            "CREATE OR REPLACE TABLE {} ({})",
            relation.sql(),
            columns
                .iter()
                .map(|column| format!("{} {}", quote(&column.name), column.data_type))
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.run(&ddl).await?;

        // One INSERT per batch keeps each statement comfortably under any
        // server-side query length limit.
        let mut inserted = 0u64;
        for batch in rows.chunks(200) {
            let values = batch
                .iter()
                .map(|row| {
                    let fields = columns
                        .iter()
                        .enumerate()
                        .map(|(index, column)| csv_literal(row.get(index), column.data_type))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("({fields})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            self.run(&format!("INSERT INTO {} VALUES {values}", relation.sql()))
                .await?;
            inserted += batch.len() as u64;
        }
        Ok(QueryResult {
            row_count: inserted,
            ..Default::default()
        })
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

/// A seed column and the Trino type inferred from its CSV values.
struct SeedColumn {
    name: String,
    data_type: &'static str,
}

/// The narrowest Trino type every non-empty value in `values` fits:
/// `boolean` for true/false, `bigint` for integers, `double` for numbers,
/// `varchar` otherwise (and for an all-empty column).
fn infer_csv_type<'a>(values: impl Iterator<Item = &'a String>) -> &'static str {
    let mut boolean = true;
    let mut integer = true;
    let mut number = true;
    let mut saw_value = false;
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        saw_value = true;
        boolean &= matches!(value.to_ascii_lowercase().as_str(), "true" | "false");
        integer &= value.parse::<i64>().is_ok();
        number &= value.parse::<f64>().is_ok();
    }
    if !saw_value {
        "varchar"
    } else if boolean {
        "boolean"
    } else if integer {
        "bigint"
    } else if number {
        "double"
    } else {
        "varchar"
    }
}

/// A VALUES literal for `value` in a column of `data_type`. Empty cells are
/// NULL (matching `read_csv_auto`); numerics and booleans go in bare;
/// varchar values are emitted verbatim — whitespace is data, not noise.
fn csv_literal(value: Option<&String>, data_type: &str) -> String {
    let Some(value) = value else {
        return "NULL".to_string();
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return if value.is_empty() || data_type != "varchar" {
            "NULL".to_string()
        } else {
            literal(value)
        };
    }
    match data_type {
        "bigint" | "double" => trimmed.to_string(),
        "boolean" => trimmed.to_ascii_lowercase(),
        _ => literal(value),
    }
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
    /// Trino's error class: `USER_ERROR`, `INTERNAL_ERROR`,
    /// `INSUFFICIENT_RESOURCES` or `EXTERNAL`.
    #[serde(rename = "errorType")]
    error_type: Option<String>,
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

    /// Statement errors that are plausibly transient: `EXTERNAL` means a
    /// connector could not reach its backing system (Nessie, object
    /// storage), `INSUFFICIENT_RESOURCES` means the coordinator refused
    /// work it may accept later. `GENERIC_INTERNAL_ERROR` is nominally
    /// internal, but the Iceberg connector wraps Nessie REST client
    /// failures ("Failed to execute … request against …") in it, so that
    /// signature is retried too.
    fn into_adapter_error(self) -> AdapterError {
        let code = self.code();
        let message = self.message();
        let transient = matches!(
            self.error_type.as_deref(),
            Some("EXTERNAL") | Some("INSUFFICIENT_RESOURCES")
        ) || (code == "GENERIC_INTERNAL_ERROR"
            && message.contains("Failed to execute"));
        let error = AdapterError::new(code, message);
        if transient {
            error.retryable()
        } else {
            error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{csv_literal, infer_csv_type, TrinoErrorPayload};

    fn infer(values: &[&str]) -> &'static str {
        let owned: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        infer_csv_type(owned.iter())
    }

    #[test]
    fn csv_types_infer_narrowly() {
        assert_eq!(infer(&["1", "42", "-7"]), "bigint");
        assert_eq!(infer(&["1.5", "2"]), "double");
        assert_eq!(infer(&["true", "FALSE"]), "boolean");
        assert_eq!(infer(&["a", "1"]), "varchar");
        assert_eq!(infer(&["", ""]), "varchar");
        // Mixed empty + numeric still infers the numeric type; the empty
        // cell lands as NULL.
        assert_eq!(infer(&["", "5"]), "bigint");
    }

    #[test]
    fn csv_literals_escape_and_null() {
        let v = |s: &str| Some(s.to_string());
        assert_eq!(csv_literal(None, "varchar"), "NULL");
        assert_eq!(csv_literal(v("").as_ref(), "bigint"), "NULL");
        assert_eq!(csv_literal(v("").as_ref(), "varchar"), "NULL");
        assert_eq!(csv_literal(v("42").as_ref(), "bigint"), "42");
        assert_eq!(csv_literal(v("TRUE").as_ref(), "boolean"), "true");
        assert_eq!(csv_literal(v("o'clock").as_ref(), "varchar"), "'o''clock'");
        // Whitespace is preserved for text columns, not trimmed away.
        assert_eq!(csv_literal(v(" pad ").as_ref(), "varchar"), "' pad '");
    }

    fn payload(error_type: &str, name: &str, message: &str) -> TrinoErrorPayload {
        serde_json::from_value(serde_json::json!({
            "message": message,
            "errorName": name,
            "errorCode": 1,
            "errorType": error_type,
        }))
        .expect("payload parses")
    }

    #[test]
    fn external_and_resource_errors_are_retryable() {
        // A connector that cannot reach Nessie/object storage, or a
        // coordinator that refused work under pressure, may succeed on a
        // later attempt.
        for error_type in ["EXTERNAL", "INSUFFICIENT_RESOURCES"] {
            let error =
                payload(error_type, "ICEBERG_COMMIT_ERROR", "commit failed").into_adapter_error();
            assert!(error.retryable, "{error_type}");
        }
    }

    #[test]
    fn nessie_client_failures_wrapped_as_internal_are_retryable() {
        let error = payload(
            "INTERNAL_ERROR",
            "GENERIC_INTERNAL_ERROR",
            "Failed to execute POST request against 'http://nessie:19120/api/v2/trees/main/contents'.",
        )
        .into_adapter_error();
        assert!(error.retryable);
    }

    #[test]
    fn user_and_internal_errors_are_not_retried() {
        let error = payload("USER_ERROR", "SYNTAX_ERROR", "bad sql").into_adapter_error();
        assert!(!error.retryable);
        let error = payload("INTERNAL_ERROR", "GENERIC_INTERNAL_ERROR", "npe").into_adapter_error();
        assert!(!error.retryable);
    }
}
