# dbt migration

> For a guided end-to-end walkthrough (analysis → translation → running on
> DuckDB), see [`dbt-migration-guide.md`](dbt-migration-guide.md).

`phlo-transform translate --from dbt` analyses a dbt project and emits the
smallest equivalent native Phlo workspace. It is a one-way translator, not a
dbt compatibility runtime: all dbt semantics live in the
`phlo-transform-dbt` crate and never reach the compiler.

Measured coverage against ~30 public dbt projects lives in
[`dbt-compatibility-corpus.md`](dbt-compatibility-corpus.md) (reproducible via
`scripts/dbt_compat_corpus.py`).

## Usage

```bash
# Analysis only — writes nothing.
phlo-transform -r path/to/dbt-project translate --from dbt --check

# Full report as JSON.
phlo-transform -r path/to/dbt-project translate --from dbt --check --json

# Emit the translated workspace into a directory.
phlo-transform -r path/to/dbt-project translate --from dbt --out generated/

# Overwrite existing files and compile-check the result.
phlo-transform -r path/to/dbt-project translate --from dbt --out generated/ --overwrite --verify
```

`--check` never writes. Writing requires `--out` and refuses to overwrite
existing files unless `--overwrite` is passed. The manifest lands at
`.phlo/migration/dbt-translation.json` inside the output directory.

## What is read

- `dbt_project.yml`: `name`, `model-paths`, `test-paths`, `seed-paths`,
  `macro-paths`, `snapshot-paths`, `analysis-paths`, `vars`, and the `models:`
  configuration hierarchy.
- `models/**/*.sql` and `models/**/*.yml` property files (`models:`,
  `sources:`, `exposures:`, `metrics:`, `semantic_models:`, `seeds:`,
  `snapshots:`).
- `tests/**/*.sql` singular tests, `macros/`, `data/`, `snapshots/`,
  `analyses/`, `packages.yml`/`dependencies.yml`, and a project-local
  `profiles.yml` (non-secret target fields only: `type`, `database`,
  `catalog`, `schema`).

## What is translated

| dbt | Phlo |
|---|---|
| `{{ ref('m') }}` | the model's logical name (`staging.stg_orders`) |
| `{{ ref('pkg', 'm') }}` (same project) | the model's logical name |
| `{{ source('s', 't') }}` | the physical `db.schema.identifier` relation |
| `{{ ref('seed_name') }}` | the relation the CSV lands as (`<schema>.<name>`, or bare `<name>` when no target schema is known) — the CSV itself is copied to `seeds/` and loads automatically at `run` time |
| `seeds/**/*.csv` | copied verbatim to `seeds/`; `[seeds]`/`[seed."name"]` schema config in `phlo.toml`; content-hash state (`csv:` prefix) so CSV edits re-trigger downstream builds |
| `{{ dbt.date_trunc('p', 'col') }}` | `date_trunc('p', col)` |
| `{{ dbt.current_timestamp() }}` | `current_timestamp` |
| `{{ dbt.type_string/timestamp/datetime/int/bigint/numeric/boolean() }}` | `varchar` / `timestamp` / `integer` / `bigint` / `numeric` / `boolean` |
| `{{ dbt.cast('e','t') }}` / `{{ dbt.string_literal('s') }}` / `{{ dbt.escape_single_quotes('s') }}` | `cast(e as t)` / `'s'` / `s` with `'` doubled |
| `{{ dbt.dateadd('p', n, from) }}` | `(from + interval 'n' p)` (portable across all targets) |
| `{{ dbt.datediff / last_day / split_part / hash / concat / type_float }}` | lowered only when `profiles.yml` declares `type: duckdb` and only with the `dbt.` qualifier (bare `hash()` is not the builtin); unknown profiles stay REVIEW |
| `{{ dbt_utils.group_by(n) }}` | `1, 2, …, n` |
| `{{ dbt_utils.equality }}` / `equal_rowcount` / `unique_combination_of_columns` / `not_empty_string` tests | generated `tests/**/*.sql` (`summarize`, `precision`, `exclude_columns` variants stay REVIEW) |
| `{{ dbt_utils.generate_surrogate_key(['a','b']) }}` | `md5(concat_ws('-', coalesce(cast("a" as varchar), '_dbt_utils_surrogate_key_null_'), …))` — `''` instead when the `surrogate_key_treat_nulls_as_empty_strings` var is set. The deprecated `dbt_utils.surrogate_key(...)` varargs form is intentionally REVIEW (upstream raises a compiler error; its null handling differed) |
| `{{ dbt_utils.star(from=…, except=[…]) }}` | `* exclude (…)` (DuckDB-first; only `from`+`except`/`exclude` are provably equivalent — `relation_alias`, `prefix`, `suffix`, `quote_identifiers`, `unquote_aliases`, `rename`, or any other argument stays REVIEW) |
| `{{ dbt_utils.safe_cast('c','t') }}` | `try_cast("c" as t)` |
| `{{ dbt_date.get_base_dates(n_dateparts=N\|start_date,end_date, datepart='p') }}` | a `generate_series` spine select (`day`/`week`/`month`/`quarter`/`year`) — lowered only when `profiles.yml` declares `type: duckdb`; unknown/non-DuckDB profiles stay REVIEW |
| simple project macros (`{{ my_macro('x') }}` whose body is literal SQL + `{{ param }}` substitutions, `{{ return(…) }}`, static `{% set %}`/`{% if %}`/`{% for %}`/`{% do return(…) %}`, `adapter.dispatch` → `default__`/`adapter__` variants) | inlined into the model's SQL; dynamic bodies (runtime lookups, unprovable statements) stay REVIEW |
| `{% set name = <literal, var(), or static call> %}` | bound at translation time; `{{ name }}` substitutes statically |
| `{% set name %}…{% endset %}` | captured statically; `{{ name }}` substitutes the rendered body |
| `{% raw %}…{% endraw %}` | contents emitted verbatim (Jinja-looking text inside is data) |
| `{% do log(…) %}` / `print(…)` / `exceptions.warn(…)` / `return(…)` | dropped / statically returned |
| `{{ target.type }}`, `{{ target.schema }}` and other scalar profile fields | the literal value from `profiles.yml`; unknown profiles stay REVIEW |
| `{{ this.name }}` | the current model's name |
| `{{ config(...) }}` with non-literal values (`target.type == 'x'`, `var()`, boolean project-macro wrappers) | evaluated statically; `enabled = false` marks the model UNSUPPORTED (Phlo has no disabled state) |
| `config(schema = 'literal')` differing from the derived namespace | model relocated under a `custom/` folder with `-- @id` preserving its logical name; dynamic schemas stay REVIEW |
| `{{ pkg.macro(...) }}` where `pkg` is the project's own name | inlined like an unqualified project macro; `adapter.dispatch('m', '<project>')` resolves to `default__m`/adapter variants |
| Jinja ternaries `x if cond else y`, `~` concatenation, `not/and/or`, `in`, `is [not] sameas/none`, `> >= < <=` comparisons | evaluated statically |
| `{% for x in <literal list> %}…{% endfor %}` (incl. `loop.index/first/last`) | unrolled statically |
| `{% if <statically-known condition> %}` / `{% if execute %}` | resolved at translation time / gate removed |
| `test: {arguments: {...}}` (modern) and `test: {...}` (legacy) | both argument spellings are read |
| `{{ var('x') }}` / `var('x', default)` | the literal value from `vars:` |
| `{{ config(...) }}` | removed; its values become directives/config |
| `materialized: table/view` | `-- @table` / `-- @view` (or inherited) |
| `materialized: incremental` + `unique_key` | `-- @incremental key=…` |
| `incremental` + `insert_overwrite` + `partition_by` | `-- @incremental partition=…` |
| `incremental` + `append` | `-- @incremental append` |
| `{% if is_incremental() %} where c > (select max(c) from {{ this }}) {% endif %}` | `@incremental window=c` |
| `unique`/`not_null` on a key column | folded into `@key`/`@incremental key=` |
| `not_null` on other columns | `-- @not-null col` |
| other column tests (`unique`, `accepted_values`, `relationships`, `unique_combination_of_columns`) | generated `tests/**/*.sql` |
| `dbt_utils.expression_is_true` / `accepted_range` (either bound optional, `inclusive: false` supported) / `not_constant` (static `group_by_columns` supported) / `not_empty_string` (honours `trim_whitespace`) (package-qualified names resolve when the package is declared) | generated `tests/**/*.sql` |
| `meta.owner` | `-- @owner` |
| `tags` | `-- @tags` |
| `description` | a leading `-- ` comment |
| `contract: enforced: true` | `[model.*.contract]` + column types in `phlo.toml` |
| `models: <proj>: <dir>: +materialized/+schema/+tags` | `transform.toml` folder config |
| `models: <proj>: +materialized` | `default_materialization` in `phlo.toml` |
| `{% macro generate_schema_name(custom_schema_name, node) %}` overrides | evaluated statically: its `{% set %}`/`{% if/elif/else %}` chain is run for every `(resource_type, schema)` case the project exercises (`node.resource_type`, `custom_schema_name [is] none`, `target.name`, `target.schema`, `| trim/lower/upper`); when the macro references `target.name` and `profiles.yml` declares multiple outputs, every output is evaluated and all must agree — a Phlo workspace is not dbt-target-specific. CLEAN only when every case resolves to the schema Phlo already emits; a diverging or unprovable case is REVIEW and names the target |
| singular tests | `tests/**/*.sql` (same layout) |
| sources | resolved to physical relations; metadata in the manifest |

## Classification

Every resource is classified — nothing is silently treated as equivalent.

- `CLEAN` — translated to native semantics with no residual Jinja.
- `REVIEW` — emitted but requires human attention: residual Jinja
  (`{% set %}`/`{% for %}`/`{% if %}` whose values are not statically
  known, dynamic macros, `run_query`), `ephemeral` materialisation
  (emitted as a view), environment-dependent expressions (`env_var`,
  `run_started_at`, `target.*`, `adapter.*` calls beyond dispatch),
  unresolved `ref`/`source`/`var`, unrecognised incremental patterns
  (degraded to full-refresh), unconvertible config (`grants`, hooks,
  `database`), and package dependencies.
- `UNSUPPORTED` — not emitted: `snapshot`/`custom` materialisations, snapshot
  and analysis files, disabled models, exposures/metrics/semantic models,
  dbt `unit_tests`/`groups`, and macros that perform runtime operations.

Package dependencies are CLEAN when every observed call site — model
expressions and package-qualified generic tests — lowered to a native
equivalent, and REVIEW when any call site could not be lowered (a package
resource is only dependency accounting: unexercised package contents are
never vendored). Packages that are declared but unused, or whose helpers we
do not cover, stay REVIEW.

The report (`--check`) shows per-kind counts by classification, model
conversion coverage, and deduplicated review reasons. `--json` returns the
full `MigrationReport` (per-resource issues with stable `DBT0xx` codes,
transformations, notes, and source hashes); the manifest mirrors it for
idempotence auditing.

## Deliberate limits

- Jinja is scanned and classified, never executed. Only statically
  provable constructs are expanded (`{% set %}`/`{% for %}` over literals
  and `var()`s, literal-condition `{% if %}`); anything dynamic —
  `run_query`, adapter calls, environment-driven loops — is preserved
  verbatim so the generated file fails loudly at `check` time rather than
  silently changing meaning.
- Project macros inline only when the whole body statically renders
  (text + parameter substitution + `return(...)` + `adapter.dispatch` to
  `default__`/adapter variants). Anything else stays REVIEW.
- A non-watermark `is_incremental()` body is dropped (or its `else` branch
  kept) and the model degrades to a correct full-refresh, flagged `REVIEW`.
- `profiles.yml` credentials are never read; only non-secret target fields are
  used as workspace defaults.
- Seeds load as `CREATE OR REPLACE TABLE … SELECT * FROM read_csv_auto(…)`
  (DuckDB); adapters without `load_csv` report seeds UNSUPPORTED at plan
  time. Package dependencies are reported, not vendored.
