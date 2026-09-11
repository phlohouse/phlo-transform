//! An embedded DuckDB adapter — the zero-infrastructure execution path.
//!
//! The adapter runs an in-process DuckDB database, either in memory or backed
//! by a file (the CLI default is `.phlo/transform/local.duckdb`). It
//! implements the full [`Adapter`] contract: `MERGE` is emulated as
//! delete+insert so behaviour does not depend on the bundled DuckDB version.
//!
//! DuckDB's [`Connection`] is `Send` but not `Sync`, so calls are serialised
//! through a `Mutex` and run on the blocking pool.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use duckdb::types::ValueRef;
use duckdb::Connection;

use phlo_transform_core::Relation;
use phlo_transform_engine::{Adapter, AdapterError, CatalogRequest, ColumnInfo, QueryResult};

/// An `Adapter` backed by an embedded DuckDB database.
pub struct DuckDbAdapter {
    connection: Arc<Mutex<Connection>>,
}

impl DuckDbAdapter {
    /// Open a file-backed database, creating the file and parent directories.
    pub fn open(path: &Path) -> Result<Self, AdapterError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                AdapterError::new(
                    "DUCKDB_OPEN",
                    format!("could not create {parent:?}: {error}"),
                )
            })?;
        }
        let connection = Connection::open(path)
            .map_err(|error| AdapterError::new("DUCKDB_OPEN", error.to_string()))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Open a transient in-memory database.
    pub fn in_memory() -> Result<Self, AdapterError> {
        let connection = Connection::open_in_memory()
            .map_err(|error| AdapterError::new("DUCKDB_OPEN", error.to_string()))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Run a statement and collect all rows as strings.
    pub async fn run_sql(&self, sql: &str) -> Result<QueryResult, AdapterError> {
        let connection = self.connection.clone();
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| AdapterError::new("DUCKDB", "connection lock poisoned"))?;
            run_blocking(&connection, &sql)
        })
        .await
        .map_err(|error| AdapterError::new("DUCKDB", format!("task failed: {error}")))?
    }
}

fn run_blocking(connection: &Connection, sql: &str) -> Result<QueryResult, AdapterError> {
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        // `prepare` fails on multi-statement input; `execute_batch` runs it.
        Err(_) => {
            connection
                .execute_batch(sql)
                .map_err(|error| duck_error(&error))?;
            return Ok(QueryResult {
                query_id: None,
                columns: Vec::new(),
                rows: Vec::new(),
                row_count: 0,
            });
        }
    };
    // In this crate version the result schema only exists once the statement
    // has been executed, so `query` must run before reading column metadata.
    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(_) => {
            // DDL/DML may fail `query`; `execute_batch` runs them.
            drop(statement);
            connection
                .execute_batch(sql)
                .map_err(|error| duck_error(&error))?;
            return Ok(QueryResult {
                query_id: None,
                columns: Vec::new(),
                rows: Vec::new(),
                row_count: 0,
            });
        }
    };
    let columns: Vec<String> = rows
        .as_ref()
        .map(|statement| statement.column_names())
        .unwrap_or_default();
    let mut out_rows = Vec::new();
    while let Some(row) = rows.next().map_err(|error| duck_error(&error))? {
        let mut values = Vec::with_capacity(columns.len().max(1));
        let mut index = 0usize;
        while let Ok(value) = row.get_ref(index) {
            values.push(value_to_string(value));
            index += 1;
        }
        out_rows.push(values);
    }
    Ok(QueryResult {
        query_id: None,
        columns,
        rows: out_rows.clone(),
        row_count: out_rows.len() as u64,
    })
}

fn duck_error(error: &duckdb::Error) -> AdapterError {
    let text = error.to_string();
    let code = if text.contains("does not exist")
        || text.contains("Catalog Error")
        || text.contains("not found")
    {
        "TABLE_NOT_FOUND"
    } else {
        "DUCKDB"
    };
    AdapterError::new(code, text)
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn value_to_string(value: ValueRef) -> String {
    match value {
        ValueRef::Null => "NULL".to_string(),
        ValueRef::Boolean(v) => v.to_string(),
        ValueRef::TinyInt(v) => v.to_string(),
        ValueRef::SmallInt(v) => v.to_string(),
        ValueRef::Int(v) => v.to_string(),
        ValueRef::BigInt(v) => v.to_string(),
        ValueRef::HugeInt(v) => v.to_string(),
        ValueRef::UTinyInt(v) => v.to_string(),
        ValueRef::USmallInt(v) => v.to_string(),
        ValueRef::UInt(v) => v.to_string(),
        ValueRef::UBigInt(v) => v.to_string(),
        ValueRef::Float(v) => v.to_string(),
        ValueRef::Double(v) => v.to_string(),
        ValueRef::Decimal(v) => v.to_string(),
        ValueRef::Text(v) => String::from_utf8_lossy(v).to_string(),
        ValueRef::Blob(v) => format!("{v:?}"),
        ValueRef::Date32(days) => duckdb_date(days),
        ValueRef::Time64(unit, v) => duckdb_time(unit, v),
        ValueRef::Timestamp(unit, v) => duckdb_timestamp(unit, v),
        other => format!("{other:?}"),
    }
}

/// Days since 1970-01-01 → `YYYY-MM-DD`.
fn duckdb_date(days: i32) -> String {
    let epoch = time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(days as i64);
    epoch
        .format(&time::format_description::well_known::Iso8601::DATE)
        .unwrap_or_else(|_| days.to_string())
}

fn duckdb_duration(unit: duckdb::types::TimeUnit, value: i64) -> time::Duration {
    match unit {
        duckdb::types::TimeUnit::Second => time::Duration::seconds(value),
        duckdb::types::TimeUnit::Millisecond => time::Duration::milliseconds(value),
        duckdb::types::TimeUnit::Microsecond => time::Duration::microseconds(value),
        duckdb::types::TimeUnit::Nanosecond => time::Duration::nanoseconds(value),
    }
}

/// Count of `unit` since midnight → `HH:MM:SS[.ffffff]`, a valid SQL literal.
fn duckdb_time(unit: duckdb::types::TimeUnit, value: i64) -> String {
    let time = (time::OffsetDateTime::UNIX_EPOCH + duckdb_duration(unit, value)).time();
    time.format(&time::macros::format_description!(
        "[hour]:[minute]:[second].[subsecond digits:6]"
    ))
    .unwrap_or_else(|_| value.to_string())
}

/// Count of `unit` since 1970-01-01 → `YYYY-MM-DD HH:MM:SS[.ffffff]`, a valid
/// SQL timestamp literal.
fn duckdb_timestamp(unit: duckdb::types::TimeUnit, value: i64) -> String {
    let datetime = time::OffsetDateTime::UNIX_EPOCH + duckdb_duration(unit, value);
    datetime
        .format(&time::macros::format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:6]"
        ))
        .unwrap_or_else(|_| value.to_string())
}

#[async_trait]
impl Adapter for DuckDbAdapter {
    fn name(&self) -> &str {
        "duckdb"
    }

    async fn relation_exists(&self, relation: &Relation) -> Result<bool, AdapterError> {
        match self
            .run_sql(&format!("SELECT 1 FROM {} LIMIT 0", relation.sql()))
            .await
        {
            Ok(_) => Ok(true),
            Err(error) if error.code == "TABLE_NOT_FOUND" => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn execute(&self, sql: &str) -> Result<QueryResult, AdapterError> {
        self.run_sql(sql).await
    }

    async fn create_or_replace_view(
        &self,
        relation: &Relation,
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        self.run_sql(&format!(
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
        self.run_sql(&format!(
            "CREATE OR REPLACE TABLE {} AS {}",
            relation.sql(),
            sql
        ))
        .await
    }

    async fn append(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError> {
        self.run_sql(&format!("INSERT INTO {} {}", relation.sql(), sql))
            .await
    }

    /// Merge is emulated as delete-then-insert on the key columns, which works
    /// on every DuckDB version without relying on `MERGE INTO` support.
    async fn merge(
        &self,
        relation: &Relation,
        key_columns: &[String],
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        if key_columns.is_empty() {
            return self.append(relation, sql).await;
        }
        let keys = key_columns
            .iter()
            .map(|key| quote(key))
            .collect::<Vec<_>>()
            .join(", ");
        self.run_sql(&format!(
            "DELETE FROM {} WHERE ({keys}) IN (SELECT {keys} FROM ({sql}) AS __phlo_src)",
            relation.sql()
        ))
        .await?;
        self.run_sql(&format!(
            "INSERT INTO {} SELECT * FROM ({sql}) AS __phlo_src",
            relation.sql()
        ))
        .await
    }

    async fn replace_partitions(
        &self,
        relation: &Relation,
        partition_columns: &[String],
        sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let columns = partition_columns
            .iter()
            .map(|column| quote(column))
            .collect::<Vec<_>>()
            .join(", ");
        self.run_sql(&format!(
            "DELETE FROM {} WHERE ({columns}) IN (SELECT {columns} FROM ({sql}) AS __phlo_src)",
            relation.sql()
        ))
        .await?;
        self.run_sql(&format!(
            "INSERT INTO {} SELECT * FROM ({sql}) AS __phlo_src",
            relation.sql()
        ))
        .await
    }

    async fn cancel(&self, _query_id: &str) -> Result<(), AdapterError> {
        // In-process execution has no remote query to cancel; the engine's
        // cooperative cancellation handle covers DuckDB runs.
        Ok(())
    }

    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError> {
        let result = match self.run_sql(&format!("DESCRIBE {}", relation.sql())).await {
            Ok(result) => result,
            // A missing relation is "no columns", not an adapter failure —
            // callers use emptiness to mean "unresolvable".
            Err(error) if error.code == "TABLE_NOT_FOUND" => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(result
            .rows
            .into_iter()
            .filter_map(|row| {
                let name = row.first()?.clone();
                let data_type = row.get(1).cloned().unwrap_or_default();
                let nullable = row
                    .get(2)
                    .map(|null| null.eq_ignore_ascii_case("YES"))
                    .unwrap_or(true);
                Some(ColumnInfo {
                    name,
                    data_type,
                    nullable,
                })
            })
            .collect())
    }

    async fn ensure_catalog(&self, _request: &CatalogRequest) -> Result<(), AdapterError> {
        // DuckDB has no Nessie catalog provisioning.
        Ok(())
    }

    async fn ensure_schema(&self, relation: &Relation) -> Result<(), AdapterError> {
        let schema = match &relation.catalog {
            Some(catalog) => format!("{}.{}", quote(catalog), quote(&relation.schema)),
            None => quote(&relation.schema),
        };
        self.run_sql(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
            .await?;
        Ok(())
    }

    async fn source_state(&self, relation: &Relation) -> Result<Option<String>, AdapterError> {
        // No snapshot metadata; fall back to a schema fingerprint plus row
        // count so appended data — the common incremental case — stays
        // observable. In-place updates that preserve the row count are not
        // detected.
        let columns = self.relation_columns(relation).await?;
        if columns.is_empty() {
            return Ok(None);
        }
        let mut parts: Vec<String> = columns
            .iter()
            .map(|column| format!("{}:{}", column.name, column.data_type))
            .collect();
        if let Ok(result) = self
            .execute(&format!("select count(*) as n from {}", relation.display()))
            .await
        {
            if let Some(count) = result.rows.first().and_then(|row| row.first()) {
                parts.push(format!("count:{count}"));
            }
        }
        Ok(Some(format!("schema:{}", fingerprint(parts))))
    }

    async fn partition_counts(
        &self,
        _relation: &Relation,
        _partition_columns: &[String],
    ) -> Result<Option<Vec<(String, i64)>>, AdapterError> {
        Ok(None)
    }

    async fn load_csv(
        &self,
        relation: &Relation,
        path: &std::path::Path,
    ) -> Result<QueryResult, AdapterError> {
        let escaped = path.display().to_string().replace('\'', "''");
        self.run_sql(&format!(
            "CREATE OR REPLACE TABLE {} AS SELECT * FROM read_csv_auto('{escaped}', header = true)",
            relation.sql()
        ))
        .await
    }
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
