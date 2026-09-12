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

    // Ephemeral models translate to `-- @ephemeral` and stay clean.
    assert_eq!(
        class_of("helpers", ResourceKind::Model),
        Classification::Clean
    );
    assert!(report
        .resources
        .iter()
        .find(|r| r.name.ends_with("helpers") && r.kind == ResourceKind::Model)
        .map(|r| r.emitted_path.as_deref().map(|_| true).unwrap_or(false))
        .unwrap_or(false));

    // Review: else-branch incremental models and packages.
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
    // `generate_schema_name`: every exercised case — model+staging, model
    // with no schema, seed+raw — evaluates statically to the schema the
    // translation already emits (target `dev`, so the prod branch is dead).
    assert_eq!(
        class_of("generate_schema_name", ResourceKind::Macro),
        Classification::Clean
    );
    // `dbt_utils.star(from=ref('stg_customers'), except=['region'])`
    // lowers statically to `* exclude ("region")`.
    assert_eq!(
        class_of("package_users", ResourceKind::Model),
        Classification::Clean
    );
    // `star` with `relation_alias` is outside the provable subset — REVIEW.
    assert_eq!(
        class_of("package_users_aliased", ResourceKind::Model),
        Classification::Review
    );
    // `star` with `rename` is outside the provable subset — REVIEW.
    assert_eq!(
        class_of("package_users_renamed", ResourceKind::Model),
        Classification::Review
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

    // Ephemeral maps to the native `-- @ephemeral` directive; the model is
    // inlined into dependents at compile time rather than materialised.
    let helpers = file("transforms/helpers.sql");
    assert!(!helpers.contains("{{ config"), "{helpers}");
    assert!(helpers.contains("-- @ephemeral"), "{helpers}");

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

    // dbt_utils.generate_surrogate_key lowers to md5(concat_ws(...)) using
    // the upstream null sentinel; the dispatching project macro inlines its
    // default variant.
    let enriched = file("transforms/marts/orders_enriched.sql");
    assert!(
        enriched.contains("md5(concat_ws('-', coalesce(cast(\"order_id\" as varchar), '_dbt_utils_surrogate_key_null_'), coalesce(cast(\"status\" as varchar), '_dbt_utils_surrogate_key_null_')))"),
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

    // dbt_utils.not_constant fails when the column is constant.
    let constant = file("tests/generated/marts__customers__customer_id__not_constant.sql");
    assert!(
        constant.contains("having count(distinct \"customer_id\") = 1"),
        "{constant}"
    );
    // `group_by_columns` scopes the constant check per group.
    let grouped = file("tests/generated/marts__labelled__status_label__not_constant.sql");
    assert!(
        grouped.contains("group by \"order_id\" having count(distinct \"status_label\") = 1"),
        "{grouped}"
    );

    // dbt_utils.accepted_range with only a lower bound and inclusive=false.
    let range = file("tests/generated/marts__customers__customer_id__accepted_range.sql");
    assert!(range.contains("\"customer_id\" <= 0"), "{range}");

    // dbt_utils.not_empty_string honours trim_whitespace.
    let empty = file("tests/generated/marts__customers__customer_id__not_empty_string.sql");
    assert!(empty.contains("\"customer_id\" = ''"), "{empty}");
    let trimmed = file("tests/generated/marts__customers__email__not_empty_string.sql");
    assert!(trimmed.contains("trim(\"email\") = ''"), "{trimmed}");
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

    // `get_base_dates` lowers only for a known DuckDB profile; this project
    // has no profiles.yml, so the model stays REVIEW.
    let spine = report
        .resources
        .iter()
        .find(|r| r.kind == ResourceKind::Model && r.name.ends_with("time_spine"))
        .expect("time_spine outcome");
    assert_eq!(spine.classification, Classification::Review);
}

/// Regression: `generate_schema_name` must not be marked CLEAN for only
/// the selected target. With `target: dev` + a `prod` output, the `prod`
/// arm places a custom-schematised model in `prod_custom` while the
/// emitted layout uses `custom` — divergent across targets → REVIEW.
#[test]
fn multi_target_schema_macro_stays_review() {
    let translation = translate_project(&fixture("dbt-schema-targets")).expect("dbt project loads");
    let macro_outcome = translation
        .report
        .resources
        .iter()
        .find(|r| r.kind == ResourceKind::Macro && r.name == "generate_schema_name")
        .expect("generate_schema_name outcome");
    assert_eq!(macro_outcome.classification, Classification::Review);
    assert!(
        macro_outcome
            .issues
            .iter()
            .any(|issue| issue.message.contains("`prod`")),
        "the issue names the diverging target: {:?}",
        macro_outcome.issues
    );

    // The models themselves are unaffected — the override only determines
    // where dbt would have placed them.
    assert!(
        translation
            .report
            .resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Model)
            .all(|m| m.classification == Classification::Clean),
        "models stay clean"
    );
}

/// Regression: every case must be checked against every target, not just
/// the first. Both outputs share `schema: main`, so the no-schema case —
/// which sorts first — is equivalent under `dev` and `prod`; only the
/// later custom-schema case diverges (`main_custom` under `prod`).
#[test]
fn later_diverging_case_is_still_caught_across_targets() {
    let translation =
        translate_project(&fixture("dbt-schema-targets-late")).expect("dbt project loads");
    let macro_outcome = translation
        .report
        .resources
        .iter()
        .find(|r| r.kind == ResourceKind::Macro && r.name == "generate_schema_name")
        .expect("generate_schema_name outcome");
    assert_eq!(macro_outcome.classification, Classification::Review);
    assert!(
        macro_outcome
            .issues
            .iter()
            .any(|issue| issue.message.contains("`prod`") && issue.message.contains("main_custom")),
        "the issue names the diverging target and schema: {:?}",
        macro_outcome.issues
    );
}

/// Regression: static evaluation of config values and macro bodies.
/// `enabled = target.type == 'duckdb'` resolves against the profile;
/// `boolean_var(...)` (a project macro wrapping `var` with `{% do return %}`)
/// proves `enabled = false`; own-package-qualified and adapter-dispatched
/// project macros inline; `{% set %}` capture, `{% raw %}`, `{% do log %}`,
/// `dbt_utils.group_by`, and literal per-model `schema` all lower.
#[test]
fn static_eval_lowering() {
    let translation = translate_project(&fixture("dbt-static-eval")).expect("dbt project loads");
    let report = &translation.report;

    let outcome = |name: &str| -> &phlo_transform_dbt::ResourceOutcome {
        report
            .resources
            .iter()
            .find(|r| r.kind == ResourceKind::Model && r.name.ends_with(name))
            .unwrap_or_else(|| panic!("no model named {name}"))
    };

    // `target.type == 'duckdb'` is true under the dev profile.
    assert_eq!(
        outcome("enabled_by_target").classification,
        Classification::Clean
    );

    // `boolean_var('missing_flag', false)` → `enabled = false` → the model
    // is disabled; Phlo has no disabled state.
    let disabled = outcome("disabled_model");
    assert_eq!(disabled.classification, Classification::Unsupported);
    assert!(
        disabled
            .issues
            .iter()
            .any(|issue| issue.message.contains("enabled = false")),
        "{:?}",
        disabled.issues
    );

    // Capture blocks, `{% raw %}`, `{% do %}`, own-package-qualified and
    // adapter-dispatched macros, and `group_by` all lower statically.
    let constructs = outcome("static_constructs");
    assert_eq!(constructs.classification, Classification::Clean);

    let file = |path: &str| {
        translation
            .files
            .iter()
            .find(|file| file.rel_path == path)
            .unwrap_or_else(|| panic!("no emitted file {path}"))
            .contents
            .clone()
    };

    let sql = file("transforms/static_constructs.sql");
    assert!(
        sql.contains("convert_timezone('UTC', 'UTC', created_at)"),
        "{sql}"
    );
    assert!(
        sql.contains("(amount_cents / 100)::numeric(16, 2)"),
        "{sql}"
    );
    assert!(sql.contains("concat('region', '_', 'eu')"), "{sql}");
    assert!(sql.contains("group by 1, 2"), "{sql}");
    assert!(sql.contains("this_looks_like_jinja"), "{sql}");
    assert!(!sql.contains("{%"), "{sql}");

    // A literal `schema = 'custom'` relocates the model into a
    // schema-named folder preserving its logical name via `-- @id`.
    let relocated = file("transforms/custom/custom_schema.sql");
    assert!(relocated.contains("-- @id"), "{relocated}");

    // Upstream-equivalence: `dbt.hash` casts to the string type before
    // hashing, and `dbt.split_part` keeps dbt's argument order
    // (string, delimiter, part_number).
    let equiv = file("transforms/upstream_equivalence.sql");
    assert!(equiv.contains("md5(cast(id as varchar))"), "{equiv}");
    assert!(equiv.contains("split_part(code, '-', 2)"), "{equiv}");
    // `{% do return(...) %}` exits the macro: nothing after it (in the
    // branch or the tail) is emitted. The returned string value is
    // emitted bare, matching upstream `{{ return(...) }}` rendering.
    assert!(equiv.contains("flag_on"), "{equiv}");
    assert!(!equiv.contains("unreached"), "{equiv}");

    // `dbt_utils.surrogate_key` is deprecated upstream (raises a compiler
    // error; historical null semantics differ) and bare `type_numeric()`
    // is not the `dbt.` builtin — both stay REVIEW. `default`, `escape`
    // and `list` filters are excluded from the static subset because
    // faithful Jinja semantics (undefined-only defaulting, HTML escaping,
    // string→char-list) differ from naive approximations.
    let not_provable = outcome("not_provable");
    assert_eq!(not_provable.classification, Classification::Review);
    let np = file("transforms/not_provable.sql");
    assert!(np.contains("surrogate_key"), "{np}");
    assert!(np.contains("type_numeric"), "{np}");
    assert!(np.contains("default"), "{np}");
    assert!(np.contains("escape"), "{np}");
    assert!(np.contains("list"), "{np}");

    // The generated workspace compiles except for the REVIEW model, whose
    // residual Jinja must fail loudly — not silently change meaning.
    let out = tempfile::tempdir().expect("tempdir");
    phlo_transform_dbt::write_translation(out.path(), &translation).expect("write");
    let project = phlo_transform_core::load_project(out.path()).expect("generated project loads");
    let report = phlo_transform_core::compile(&project).check_report();
    assert!(!report.ok, "expected not_provable.sql to fail check");
    for diagnostic in &report.diagnostics {
        assert_eq!(
            diagnostic.path.as_deref(),
            Some("transforms/not_provable.sql"),
            "unexpected diagnostic outside not_provable.sql: {diagnostic:?}"
        );
    }
}

/// Package source resolution: vendored (`dbt_packages/`) and `local:`
/// packages contribute macro source for static inlining — qualified calls,
/// unique bare calls, and `{% for %}` bodies all lower from the real
/// upstream source. Runtime-dependent calls, cross-package `ref()`s, and
/// uninstalled packages stay REVIEW.
#[test]
fn package_source_resolution() {
    let translation = translate_project(&fixture("dbt-packages")).expect("dbt project loads");
    let report = &translation.report;

    let outcome = |kind: ResourceKind, name: &str| -> &phlo_transform_dbt::ResourceOutcome {
        report
            .resources
            .iter()
            .find(|r| r.kind == kind && (r.name == name || r.name.ends_with(&format!(".{name}"))))
            .unwrap_or_else(|| panic!("no {kind:?} resource named {name}"))
    };
    let file = |path: &str| {
        translation
            .files
            .iter()
            .find(|file| file.rel_path == path)
            .unwrap_or_else(|| panic!("no emitted file {path}"))
            .contents
            .clone()
    };

    // Qualified calls (`kit.squared`, `localpkg.prefixed`, `kit.unroll`)
    // and the unique bare `signature()` all inline from package source.
    let uses = outcome(ResourceKind::Model, "uses_package");
    assert_eq!(uses.classification, Classification::Clean);
    let sql = file("transforms/uses_package.sql");
    assert!(sql.contains("(price * price)"), "{sql}");
    assert!(sql.contains("stg_orders"), "{sql}");
    assert!(sql.contains("a, b"), "{sql}");
    assert!(sql.contains("'kit-v1'"), "{sql}");
    assert!(!sql.contains("{{"), "{sql}");

    // Package models are not vendored — the cross-package ref stays REVIEW
    // with a precise reason.
    let pkg_ref = outcome(ResourceKind::Model, "uses_package_ref");
    assert_eq!(pkg_ref.classification, Classification::Review);
    assert!(
        pkg_ref
            .issues
            .iter()
            .any(|issue| issue.message.contains("model in package")),
        "{:?}",
        pkg_ref.issues
    );

    // `kit.dynamic` calls `run_query` — runtime-dependent → REVIEW, and the
    // failed call site marks the package itself REVIEW.
    let dynamic = outcome(ResourceKind::Model, "uses_dynamic");
    assert_eq!(dynamic.classification, Classification::Review);

    let kit = outcome(ResourceKind::Package, "acme/kit");
    assert_eq!(kit.classification, Classification::Review);
    assert_eq!(kit.source_path.as_deref(), Some("dbt_packages/kit"));
    assert!(
        kit.notes
            .iter()
            .any(|note| note.contains("locked to `1.0.0`")),
        "{:?}",
        kit.notes
    );

    let localpkg = outcome(ResourceKind::Package, "localpkg");
    assert_eq!(localpkg.classification, Classification::Clean);
    assert_eq!(localpkg.source_path.as_deref(), Some("localpkg"));

    let missing = outcome(ResourceKind::Package, "acme/missing_pkg");
    assert_eq!(missing.classification, Classification::Review);
    assert!(
        missing
            .issues
            .iter()
            .any(|issue| issue.message.contains("not installed")),
        "{:?}",
        missing.issues
    );

    // `fivetran_utils` is vendored verbatim from upstream; its adapter-
    // dispatching helpers fail inline evaluation but lower through the
    // recognised-helper rewrites of the `default__` implementations. The
    // unprovable call sites in `fivetran_review` mark the package REVIEW.
    let fivetran = outcome(ResourceKind::Package, "fivetran/fivetran_utils");
    assert_eq!(fivetran.classification, Classification::Review);

    // `demo_union_schemas` has two entries → the connector unions, so the
    // helper appends `source_relation` to the partition list.
    let partitioned = outcome(ResourceKind::Model, "fivetran_partitioned");
    assert_eq!(partitioned.classification, Classification::Clean);
    let sql = file("transforms/fivetran_partitioned.sql");
    assert!(sql.contains("partition by s.id\n"), "{sql}");
    assert!(sql.contains(", s.source_relation"), "{sql}");
    assert!(!sql.contains("{{"), "{sql}");

    // `has_other_partitions='no'` starts a fresh `partition by` clause.
    let only = outcome(ResourceKind::Model, "fivetran_partition_only");
    assert_eq!(only.classification, Classification::Clean);
    let sql = file("transforms/fivetran_partition_only.sql");
    assert!(sql.contains("partition by s.source_relation"), "{sql}");

    // Bare call with `package_prefix_union_variable=false`: the unprefixed
    // `union_schemas`/`union_databases` vars are unset and `demo_sources`
    // has one entry → not unioning → the helper emits nothing.
    let not_unioning = outcome(ResourceKind::Model, "fivetran_not_unioning");
    assert_eq!(not_unioning.classification, Classification::Clean);
    let sql = file("transforms/fivetran_not_unioning.sql");
    assert!(!sql.contains("source_relation"), "{sql}");
    assert!(sql.contains("over (\n"), "{sql}");

    // String entries emit bare; mappings honour `alias`/`transform_sql`.
    let pass = outcome(ResourceKind::Model, "fivetran_pass_through");
    assert_eq!(pass.classification, Classification::Clean);
    let sql = file("transforms/fivetran_pass_through.sql");
    assert!(sql.contains(", raw_a"), "{sql}");
    assert!(sql.contains(", renamed_b"), "{sql}");
    assert!(sql.contains(", upper(raw_c) as raw_c"), "{sql}");

    // An unset var raises upstream and a non-list var is not provable:
    // both stay REVIEW rather than guessing.
    let review = outcome(ResourceKind::Model, "fivetran_review");
    assert_eq!(review.classification, Classification::Review);

    // `dispatch:` config is honoured: `adapter.dispatch('greet', 'kit')`
    // searches `pkg_consumer` first, so the project's `default__greet`
    // wins over the package's own variant.
    let over = outcome(ResourceKind::Model, "dispatch_override");
    assert_eq!(over.classification, Classification::Clean);
    let sql = file("transforms/dispatch_override.sql");
    assert!(sql.contains("'project-wins'"), "{sql}");
    assert!(!sql.contains("kit-default"), "{sql}");

    // Without a `dispatch:` entry the namespace's own `default__` variant
    // is used — the local package's implementation.
    let default = outcome(ResourceKind::Model, "dispatch_default");
    assert_eq!(default.classification, Classification::Clean);
    let sql = file("transforms/dispatch_default.sql");
    assert!(sql.contains("'localpkg-default'"), "{sql}");
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

/// A modified `fivetran_utils` implementation fails the fingerprint guard:
/// the recognised-helper lowerings were proven against one exact upstream
/// body, so an unknown implementation stays REVIEW rather than being
/// rewritten by a lowering that no longer applies.
#[test]
fn tampered_fivetran_source_is_not_lowered() {
    fn copy_dir(src: &std::path::Path, dest: &std::path::Path) {
        std::fs::create_dir_all(dest).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let target = dest.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture("dbt-packages"), dir.path());

    // A semantic no-op upstream (dead comment inside the body) still defeats
    // the fingerprint — the guard matches the exact verified source.
    let macro_path = dir
        .path()
        .join("dbt_packages/fivetran_utils/macros/fill_pass_through_columns.sql");
    let body = std::fs::read_to_string(&macro_path).unwrap();
    std::fs::write(
        &macro_path,
        body.replace("{% endmacro %}", "-- tampered\n{% endmacro %}"),
    )
    .unwrap();

    let translation = translate_project(dir.path()).expect("dbt project loads");
    let report = &translation.report;
    let outcome = |name: &str| {
        report
            .resources
            .iter()
            .find(|r| r.kind == ResourceKind::Model && r.name.ends_with(&format!(".{name}")))
            .unwrap_or_else(|| panic!("no model named {name}"))
            .classification
    };
    assert_eq!(outcome("fivetran_pass_through"), Classification::Review);
    // `partition_by_source_relation`'s own verified body is untouched, so
    // its lowering still applies.
    assert_eq!(outcome("fivetran_partitioned"), Classification::Clean);
    // Non-fivetran package inlining is unaffected by the guard.
    assert_eq!(outcome("uses_package"), Classification::Clean);
}
