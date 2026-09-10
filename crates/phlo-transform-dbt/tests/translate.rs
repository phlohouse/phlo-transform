//! End-to-end translation tests against the `fixtures/dbt-*` projects.

use std::path::PathBuf;

use phlo_transform_dbt::{translate_project, Classification, ResourceKind};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures")
        .join(name)
}

#[test]
fn jaffle_classification() {
    let translation = translate_project(&fixture("dbt-jaffle")).expect("dbt project loads");
    let report = &translation.report;

    let class_of = |name: &str, kind: ResourceKind| -> Classification {
        report
            .resources
            .iter()
            .find(|r| r.kind == kind && (r.name == name || r.name.ends_with(&format!(".{name}"))))
            .unwrap_or_else(|| panic!("no {kind:?} resource named {name}"))
            .classification
    };

    // Clean conversions.
    assert_eq!(
        class_of("stg_customers", ResourceKind::Model),
        Classification::Clean
    );
    assert_eq!(
        class_of("stg_orders", ResourceKind::Model),
        Classification::Clean
    );
    assert_eq!(
        class_of("customers", ResourceKind::Model),
        Classification::Clean
    );
    assert_eq!(
        class_of("orders_incremental", ResourceKind::Model),
        Classification::Clean
    );
    assert_eq!(
        class_of("assert_positive_amounts", ResourceKind::SingularTest),
        Classification::Clean
    );
    assert_eq!(
        class_of("raw.customers", ResourceKind::Source),
        Classification::Clean
    );
    assert_eq!(
        class_of("raw.orders", ResourceKind::Source),
        Classification::Clean
    );

    // Review: macros, ephemeral, else-branch incremental, packages, seeds.
    assert_eq!(
        class_of("helpers", ResourceKind::Model),
        Classification::Review
    );
    assert_eq!(
        class_of("labelled", ResourceKind::Model),
        Classification::Review
    );
    assert_eq!(
        class_of("package_users", ResourceKind::Model),
        Classification::Review
    );
    assert_eq!(
        class_of("daily_rollup", ResourceKind::Model),
        Classification::Review
    );
    assert_eq!(
        class_of("label_status", ResourceKind::Macro),
        Classification::Review
    );
    assert_eq!(
        class_of("dbt-labs/dbt_utils", ResourceKind::Package),
        Classification::Review
    );
    assert_eq!(
        class_of("countries", ResourceKind::Seed),
        Classification::Review
    );

    // Unsupported: snapshots.
    assert_eq!(
        class_of("orders_snapshot", ResourceKind::Snapshot),
        Classification::Unsupported
    );
}

#[test]
fn jaffle_emitted_sql() {
    let translation = translate_project(&fixture("dbt-jaffle")).expect("dbt project loads");
    let file = |path: &str| {
        translation
            .files
            .iter()
            .find(|file| file.rel_path == path)
            .unwrap_or_else(|| panic!("no emitted file {path}"))
            .contents
            .clone()
    };

    // ref() lowered to the logical name; var() to a literal.
    let stg = file("transforms/staging/stg_orders.sql");
    assert!(stg.contains("from warehouse.raw_lims.orders"), "{stg}");
    assert!(stg.contains("where region = 'eu'"), "{stg}");
    assert!(!stg.contains("{{"), "{stg}");

    // Incremental + unique_key + merge becomes a keyed incremental; the
    // watermark filter is preserved as the window column.
    let incremental = file("transforms/marts/orders_incremental.sql");
    assert!(
        incremental.contains("-- @incremental key=order_id"),
        "{incremental}"
    );
    assert!(!incremental.contains("{%"), "{incremental}");
    assert!(!incremental.contains("{{ this }}"), "{incremental}");

    // customers: materialized=table equals the folder default (marts → table),
    // so no @table directive; tags come through.
    let customers = file("transforms/marts/customers.sql");
    assert!(
        customers.contains("from staging.stg_customers c"),
        "{customers}"
    );
    assert!(customers.contains("-- @tags gold"), "{customers}");
    assert!(customers.contains("-- @owner analytics"), "{customers}");
    assert!(
        customers.contains("-- Customer mart with order counts"),
        "{customers}"
    );

    // Ephemeral degrades to a view (the workspace default — no directive
    // needed; the REVIEW note records the change).
    let helpers = file("transforms/helpers.sql");
    assert!(!helpers.contains("{{ config"), "{helpers}");

    // Folder-level config lands in transform.toml.
    let staging_toml = file("transforms/staging/transform.toml");
    assert!(
        staging_toml.contains("[folder.\"staging\"]"),
        "{staging_toml}"
    );
    assert!(
        staging_toml.contains("schema = \"staging\""),
        "{staging_toml}"
    );
    let marts_toml = file("transforms/marts/transform.toml");
    assert!(
        marts_toml.contains("materialized = \"table\""),
        "{marts_toml}"
    );
    assert!(marts_toml.contains("tags = [\"mart\"]"), "{marts_toml}");

    // Generated tests from schema.yml column tests.
    let unique = file("tests/generated/marts__customers__customer_id__unique.sql");
    assert!(unique.contains("marts.customers"), "{unique}");
    file("tests/generated/raw__orders__id__unique.sql");
    file("tests/generated/raw__orders__status__accepted_values.sql");
    file("tests/assert_positive_amounts.sql");
}

#[test]
fn translation_is_deterministic() {
    let first = translate_project(&fixture("dbt-jaffle")).expect("load");
    let second = translate_project(&fixture("dbt-jaffle")).expect("load");
    let files = |t: &phlo_transform_dbt::Translation| {
        t.files
            .iter()
            .map(|f| (f.rel_path.clone(), f.contents.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(files(&first), files(&second));
    assert_eq!(
        serde_json::to_string(&first.report).unwrap(),
        serde_json::to_string(&second.report).unwrap()
    );
}

/// Regression: `ref()` to a seed must resolve to the relation dbt would
/// materialise it as (not "no known target"), modern `arguments:` test
/// syntax must convert, and `dbt.date_trunc` lowers to the native function.
#[test]
fn seed_refs_arguments_syntax_and_dbt_builtins() {
    let translation = translate_project(&fixture("dbt-seeds")).expect("dbt project loads");
    let report = &translation.report;

    let model = report
        .resources
        .iter()
        .find(|r| r.kind == ResourceKind::Model && r.name.ends_with("stg_events"))
        .expect("stg_events outcome");
    assert_eq!(model.classification, Classification::Review);
    assert!(
        model.issues.iter().all(|issue| issue.code != "DBT001"),
        "seed ref must resolve: {:?}",
        model.issues
    );
    assert!(
        model
            .issues
            .iter()
            .any(|issue| issue.code == "DBT015" && issue.message.contains("raw_events")),
        "expected a seed-hosting note: {:?}",
        model.issues
    );

    let file = |path: &str| {
        translation
            .files
            .iter()
            .find(|file| file.rel_path == path)
            .unwrap_or_else(|| panic!("no emitted file {path}"))
            .contents
            .clone()
    };

    let stg = file("transforms/staging/stg_events.sql");
    assert!(stg.contains("from raw_events"), "{stg}");
    assert!(stg.contains("date_trunc('day', ts)"), "{stg}");
    assert!(!stg.contains("{{"), "{stg}");

    // `arguments:`-style tests must produce generated test files.
    let accepted = file("tests/generated/staging__stg_events__status__accepted_values.sql");
    assert!(accepted.contains("'placed'"), "{accepted}");
    let relationships = file("tests/generated/staging__stg_events__event_id__relationships.sql");
    assert!(relationships.contains("raw_events"), "{relationships}");
}

#[test]
fn clean_fixture_verifies_with_the_native_compiler() {
    let translation = translate_project(&fixture("dbt-clean")).expect("load");
    let out = tempfile::tempdir().expect("tempdir");
    phlo_transform_dbt::write_translation(out.path(), &translation).expect("write");

    let project = phlo_transform_core::load_project(out.path()).expect("generated project loads");
    let compilation = phlo_transform_core::compile(&project);
    let report = compilation.check_report();
    assert!(
        report.ok,
        "generated workspace failed check: {:?}",
        report.diagnostics
    );
    assert_eq!(report.model_count, 2);

    // The generated test was discovered too.
    let list = compilation.list_report();
    assert!(list.tests.iter().any(|t| t.name.contains("not_null")));
}
