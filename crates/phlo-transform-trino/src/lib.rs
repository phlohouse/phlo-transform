//! A minimal Trino HTTP adapter.
//!
//! Implements only what Phase 1 needs: execute, create/replace view, create
//! table, existence checks, column metadata and cancellation. The Trino
//! protocol is simple enough to speak directly
//! (<https://trino.io/docs/current/develop/client-protocol.html>).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use phlo_transform_core::Relation;
use phlo_transform_engine::{Adapter, AdapterError, CatalogRequest, ColumnInfo, QueryResult};

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
}

impl TrinoAdapter {
    pub fn new(config: TrinoConfig) -> Result<Self, AdapterError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| AdapterError::new("TRINO_CLIENT", error.to_string()))?;
        Ok(Self { client, config })
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
        let mut payload: StatementResponse = response
            .json()
            .await
            .map_err(|error| self.transport_error(error))?;

        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(AdapterError::new(
                "TRINO_AUTH",
                "authentication was rejected by the Trino server",
            ));
        }

        let query_id = payload.id.clone();
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

    async fn ensure_catalog(&self, request: &CatalogRequest) -> Result<(), AdapterError> {
        let (Some(reference), Some(nessie_uri)) = (&request.reference, &request.nessie_uri) else {
            return Ok(());
        };
        if self.catalog_exists(&request.catalog).await? {
            return Ok(());
        }
        let warehouse = request
            .warehouse
            .clone()
            .unwrap_or_else(|| "local:///tmp/phlo-warehouse".to_string());
        let mut properties = vec![
            "\"iceberg.catalog.type\"='nessie'".to_string(),
            format!(
                "\"iceberg.nessie-catalog.uri\"='{}/api/v2'",
                nessie_uri.trim_end_matches('/')
            ),
            format!("\"iceberg.nessie-catalog.ref\"='{reference}'"),
            format!("\"iceberg.nessie-catalog.default-warehouse-dir\"='{warehouse}'"),
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
        let snapshots = Relation {
            catalog: relation.catalog.clone(),
            schema: relation.schema.clone(),
            table: format!("{}$snapshots", relation.table),
        };
        match self
            .run(&format!(
                "SELECT snapshot_id FROM {} ORDER BY committed_at DESC LIMIT 1",
                snapshots.sql()
            ))
            .await
        {
            Ok(result) => Ok(result.rows.first().and_then(|row| row.first()).cloned()),
            Err(_) => Ok(None),
        }
    }
}

impl TrinoAdapter {
    async fn catalog_exists(&self, catalog: &str) -> Result<bool, AdapterError> {
        let result = self.run("SHOW CATALOGS").await?;
        Ok(result
            .rows
            .iter()
            .any(|row| row.first().map(String::as_str) == Some(catalog)))
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
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
