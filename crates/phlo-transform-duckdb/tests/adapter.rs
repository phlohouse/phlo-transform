//! Adapter contract tests against in-memory DuckDB.

use phlo_transform_core::Relation;
use phlo_transform_duckdb::DuckDbAdapter;
use phlo_transform_engine::Adapter;

fn rel(schema: &str, table: &str) -> Relation {
    Relation {
        catalog: None,
        schema: schema.to_string(),
        table: table.to_string(),
    }
}

#[tokio::test]
async fn create_table_and_view() {
    let adapter = DuckDbAdapter::in_memory().expect("open");
    let table = rel("main", "items");
    adapter.ensure_schema(&table).await.expect("schema");
    adapter
        .create_or_replace_table(
            &table,
            "select 1 as id, 'a' as kind union all select 2, 'b'",
        )
        .await
        .expect("create table");
    assert!(adapter.relation_exists(&table).await.expect("exists"));

    let view = rel("main", "item_view");
    adapter
        .create_or_replace_view(&view, "select * from main.items where id = 1")
        .await
        .expect("create view");
    let result = adapter
        .execute("select count(*) as n from main.item_view")
        .await
        .expect("select");
    assert_eq!(result.rows[0][0], "1");
}

#[tokio::test]
async fn merge_upserts_by_key() {
    let adapter = DuckDbAdapter::in_memory().expect("open");
    let table = rel("main", "t");
    adapter
        .create_or_replace_table(
            &table,
            "select 1 as id, 'old' as v union all select 2, 'keep'",
        )
        .await
        .expect("create");
    adapter
        .merge(
            &table,
            &["id".to_string()],
            "select 1 as id, 'new' as v union all select 3, 'ins'",
        )
        .await
        .expect("merge");
    let result = adapter
        .execute("select v from main.t order by id")
        .await
        .expect("select");
    let values: Vec<&str> = result.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(values, ["new", "keep", "ins"]);
}

#[tokio::test]
async fn append_and_partitions() {
    let adapter = DuckDbAdapter::in_memory().expect("open");
    let table = rel("main", "events");
    adapter
        .create_or_replace_table(
            &table,
            "select 1 as id, date '2024-01-01' as d union all select 2, date '2024-01-02'",
        )
        .await
        .expect("create");
    adapter
        .replace_partitions(
            &table,
            &["d".to_string()],
            "select 3 as id, date '2024-01-02' as d union all select 4, date '2024-01-03'",
        )
        .await
        .expect("replace partitions");
    let result = adapter
        .execute("select id from main.events order by id")
        .await
        .expect("select");
    let ids: Vec<&str> = result.rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(ids, ["1", "3", "4"]);
}

#[tokio::test]
async fn relation_columns_and_state() {
    let adapter = DuckDbAdapter::in_memory().expect("open");
    let table = rel("main", "src");
    adapter
        .create_or_replace_table(&table, "select 1 as id, 'x'::varchar as name")
        .await
        .expect("create");
    let columns = adapter.relation_columns(&table).await.expect("columns");
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].name, "id");
    assert!(adapter
        .source_state(&table)
        .await
        .expect("state")
        .is_some_and(|s| s.starts_with("schema:")));
    assert!(adapter
        .partition_counts(&table, &[])
        .await
        .expect("pc")
        .is_none());
}

#[tokio::test]
async fn file_backed_database_persists() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("local.duckdb");
    let table = rel("main", "persisted");
    {
        let adapter = DuckDbAdapter::open(&path).expect("open");
        adapter
            .create_or_replace_table(&table, "select 42 as answer")
            .await
            .expect("create");
    }
    let adapter = DuckDbAdapter::open(&path).expect("reopen");
    let result = adapter
        .execute("select answer from main.persisted")
        .await
        .expect("select");
    assert_eq!(result.rows[0][0], "42");
}
