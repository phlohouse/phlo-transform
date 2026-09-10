//! Trino SQL syntax coverage.
//!
//! The compiler parses with `sqlparser-rs`'s permissive generic dialect,
//! because that crate has no dedicated Trino dialect. Before Phase 1 depends
//! on Trino, these tests prove the constructs we intend to support parse and
//! that relation extraction still behaves correctly.
//!
//! Each snippet is a complete statement. Add new constructs here rather than
//! scattering ad-hoc parser checks through the codebase.

use phlo_transform_sql::{extract_relations, parse_statements, Dialect};

/// Trino statements that must parse.
const PARSEABLE: &[&str] = &[
    "SELECT * FROM tpch.tiny.orders",
    "WITH ranked AS (SELECT orderkey FROM tpch.tiny.orders) SELECT * FROM ranked",
    "SELECT * FROM UNNEST(ARRAY[1, 2, 3]) AS t(x)",
    "SELECT * FROM UNNEST(ARRAY[1, 2, 3]) WITH ORDINALITY AS t(x, i)",
    "SELECT * FROM tpch.tiny.orders o CROSS JOIN UNNEST(ARRAY[1]) AS t(x)",
    "SELECT * FROM tpch.tiny.orders o, LATERAL (SELECT 1 AS x) t",
    "SELECT CAST(ROW(1, 'a') AS ROW(id BIGINT, name VARCHAR)) AS r",
    "SELECT MAP(ARRAY['a'], ARRAY[1]) AS m",
    "SELECT TRY_CAST('1' AS BIGINT) AS v",
    "SELECT approx_percentile(x, 0.5) FROM (SELECT 1 AS x) t",
    "SELECT date_trunc('day', CAST('2020-01-01' AS timestamp)) AS d",
    "SELECT INTERVAL '1' DAY",
    "SELECT TIMESTAMP '2020-01-01 00:00:00'",
    "SELECT count(*) FILTER (WHERE x > 0) FROM (SELECT 1 AS x) t",
    "SELECT a, b, count(*) FROM t GROUP BY GROUPING SETS ((a), (b))",
    "SELECT a, b, count(*) FROM t GROUP BY CUBE (a, b)",
    "SELECT a, b, count(*) FROM t GROUP BY ROLLUP (a, b)",
    "SELECT * FROM t OFFSET 5 ROWS",
    "SELECT * FROM t FETCH FIRST 5 ROWS ONLY",
    "SELECT * FROM t QUALIFY row_number() OVER (PARTITION BY a ORDER BY b) = 1",
    "SELECT json_extract_scalar(payload, '$.a') FROM t",
    "SELECT x[1] FROM (SELECT ARRAY[1, 2] AS x) t",
    "SELECT r.field FROM (SELECT CAST(ROW(1) AS ROW(field BIGINT)) AS r) t",
    "SELECT CASE WHEN a > 0 THEN 'p' ELSE 'n' END FROM t",
    "SELECT * FROM t TABLESAMPLE BERNOULLI (10)",
    "SELECT count(DISTINCT a) FROM t",
    "SELECT * FROM t WHERE a IN (SELECT a FROM u)",
    "SELECT * FROM t1 JOIN t2 USING (id)",
    "SELECT * FROM t1 LEFT JOIN t2 ON t1.id = t2.id WHERE t2.id IS NULL",
    "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, name)",
    "SELECT * FROM t WHERE a = DATE '2020-01-01'",
    "SELECT sum(price) OVER (PARTITION BY custkey ORDER BY orderdate ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM t",
];

#[test]
fn trino_constructs_parse() {
    let mut failures = Vec::new();
    for sql in PARSEABLE {
        if let Err(error) = parse_statements(sql, Dialect::Trino) {
            failures.push(format!("{sql}\n    {error}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} Trino snippet(s) failed to parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn trino_relation_extraction_is_unchanged() {
    // Table-valued functions are not relations; lateral subqueries are.
    assert_eq!(
        relations("SELECT * FROM tpch.tiny.orders o, LATERAL (SELECT 1) t"),
        vec!["tpch.tiny.orders"]
    );
    assert_eq!(
        relations("SELECT * FROM UNNEST(ARRAY[1, 2, 3]) WITH ORDINALITY AS t(x, i)"),
        Vec::<String>::new()
    );
    assert_eq!(
        relations(
            "SELECT * FROM tpch.tiny.orders o \
             CROSS JOIN UNNEST(o.items) AS i(item)"
        ),
        vec!["tpch.tiny.orders"]
    );
    assert_eq!(
        relations("SELECT * FROM t QUALIFY row_number() OVER (PARTITION BY a ORDER BY b) = 1"),
        vec!["t"]
    );
}

#[test]
fn trino_ctes_are_still_excluded() {
    assert_eq!(
        relations(
            "WITH recent AS (SELECT * FROM tpch.tiny.orders) \
             SELECT * FROM recent"
        ),
        vec!["tpch.tiny.orders"]
    );
}

fn relations(sql: &str) -> Vec<String> {
    let statements = parse_statements(sql, Dialect::Trino)
        .unwrap_or_else(|error| panic!("failed to parse: {sql}\n{error}"));
    extract_relations(&statements)
        .into_iter()
        .map(|relation| relation.name.as_dotted())
        .collect()
}
