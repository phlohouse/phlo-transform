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

    // Static Jinja: `{% set %}` + `{% for %}` over a literal list expand
    // at translation time; `{% if not loop.last %}` resolves statically.
    assert_eq!(
        class_of("payments_pivot", ResourceKind::Model),
        Classification::Clean
    );
    // Recognised package helpers and an adapter-dispatching project macro.
    assert_eq!(
        class_of("orders_enriched", ResourceKind::Model),
        Classification::Clean
    );
    assert_eq!(
        class_of("time_spine", ResourceKind::Model),
        Classification::Clean
    );

    // Review: ephemeral and else-branch incremental models, packages.
    assert_eq!(
        class_of("helpers", ResourceKind::Model),
        Classification::Review
    );
    assert_eq!(
        class_of("daily_rollup", ResourceKind::Model),
        Classification::Review
    );
    assert_eq!(
        class_of("dbt-labs/dbt_utils", ResourceKind::Package),
        Classification::Review
    );

    // Project-macro inlining: `label_status('status')` renders its body.
    assert_eq!(
        class_of("labelled", ResourceKind::Model),
        Classification::Clean
    );
    assert_eq!(
        class_of("label_status", ResourceKind::Macro),
        Classification::Clean
    );
    // `dbt_utils.star(from=ref('stg_customers'), except=['region'])`
    // lowers statically to `* exclude ("region")`.
    assert_eq!(
        class_of("package_users", ResourceKind::Model),
        Classification::Clean
    );
    // Seeds copy as runnable CSV inputs.
    assert_eq!(
        class_of("countries", ResourceKind::Seed),
        Classification::Clean
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

    // Static Jinja expansion: `{% set %}` + `{% for %}` + `{% if loop.last %}`
    // fully unroll into plain SQL.
    let pivot = file("transforms/marts/payments_pivot.sql");
    assert!(
        pivot.contains("'credit_card' then amount else 0 end) as credit_card_amount"),
        "{pivot}"
    );
    assert!(
        pivot.contains("'bank_transfer' then amount else 0 end) as bank_transfer_amount"),
        "{pivot}"
    );
    assert!(!pivot.contains("{%"), "{pivot}");
    assert!(!pivot.contains("{{"), "{pivot}");
    assert_eq!(pivot.matches("_amount").count(), 3, "{pivot}");

    // dbt_utils.generate_surrogate_key lowers to md5(concat_ws(...));
    // the dispatching project macro inlines its default variant.
    let enriched = file("transforms/marts/orders_enriched.sql");
    assert!(
        enriched.contains("md5(concat_ws('-', coalesce(cast(\"order_id\" as varchar), ''), coalesce(cast(\"status\" as varchar), '')))"),
        "{enriched}"
    );
    assert!(
        enriched.contains("(amount / 100)::numeric(16, 2)"),
        "{enriched}"
    );
    assert!(!enriched.contains("{{"), "{enriched}");

    // dbt_date.get_base_dates(n_dateparts=365*2, datepart="day") lowers to a
    // generate_series spine; the arithmetic arg evaluates at compile time.
    let spine = file("transforms/marts/time_spine.sql");
    assert!(spine.contains("generate_series("), "{spine}");
    assert!(spine.contains("interval '730' day"), "{spine}");
    assert!(spine.contains("as date_day"), "{spine}");
    assert!(!spine.contains("{{"), "{spine}");

    // The CSV seed is copied verbatim as a native seed input.
    let seed = file("seeds/countries.csv");
    assert!(seed.contains("code,"), "{seed}");

    // Generated tests from schema.yml column tests.
    let unique = file("tests/generated/marts__customers__customer_id__unique.sql");
    assert!(unique.contains("marts.customers"), "{unique}");
    file("tests/generated/raw__orders__id__unique.sql");
    file("tests/generated/raw__orders__status__accepted_values.sql");
    file("tests/assert_positive_amounts.sql");

    // dbt_utils.expression_is_true lowers to a failing-rows select.
    let expression = file("tests/generated/marts__customers__model__expression_is_true.sql");
    assert!(
        expression.contains("where not (customer_id >= 0)"),
        "{expression}"
    );
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

/// Regression: `ref()` to a seed resolves to the relation the CSV loads
/// into, the CSV is copied as a native seed, modern `arguments:` test
/// syntax converts, and `dbt.date_trunc` lowers to the native function.
#[test]
fn seed_refs_arguments_syntax_and_dbt_builtins() {
    let translation = translate_project(&fixture("dbt-seeds")).expect("dbt project loads");
    let report = &translation.report;

    let model = report
        .resources
        .iter()
        .find(|r| r.kind == ResourceKind::Model && r.name.ends_with("stg_events"))
        .expect("stg_events outcome");
    assert_eq!(model.classification, Classification::Clean);
    assert!(
        model.issues.is_empty(),
        "seed ref resolves and lowers clean: {:?}",
        model.issues
    );

    let seed = report
        .resources
        .iter()
        .find(|r| r.kind == ResourceKind::Seed && r.name == "raw_events")
        .expect("raw_events seed outcome");
    assert_eq!(seed.classification, Classification::Clean);

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
