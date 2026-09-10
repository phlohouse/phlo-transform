# dbt migration

> For a guided end-to-end walkthrough (analysis → translation → running on
> DuckDB), see [`dbt-migration-guide.md`](dbt-migration-guide.md).

`phlo-transform translate --from dbt` analyses a dbt project and emits the
smallest equivalent native Phlo workspace. It is a one-way translator, not a
dbt compatibility runtime: all dbt semantics live in the
`phlo-transform-dbt` crate and never reach the compiler.

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
| `{{ ref('seed_name') }}` | the relation the CSV lands as (`<schema>.<name>`, or bare `<name>` when no target schema is known); the seed resource is flagged REVIEW, the model itself is CLEAN |
| `{{ dbt.date_trunc('p', 'col') }}` | `date_trunc('p', col)` |
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
| `meta.owner` | `-- @owner` |
| `tags` | `-- @tags` |
| `description` | a leading `-- ` comment |
| `contract: enforced: true` | `[model.*.contract]` + column types in `phlo.toml` |
| `models: <proj>: <dir>: +materialized/+schema/+tags` | `transform.toml` folder config |
| `models: <proj>: +materialized` | `default_materialization` in `phlo.toml` |
| singular tests | `tests/**/*.sql` (same layout) |
| sources | resolved to physical relations; metadata in the manifest |

## Classification

Every resource is classified — nothing is silently treated as equivalent.

- `CLEAN` — translated to native semantics with no residual Jinja.
- `REVIEW` — emitted but requires human attention: residual Jinja
  (macros, `{% set %}`, `{% for %}`, non-watermark `{% if %}`), `ephemeral`
  materialisation (emitted as a view), environment-dependent expressions
  (`env_var`, `run_started_at`, `target.*`, `adapter.*`), unresolved
  `ref`/`source`/`var`, unrecognised incremental patterns (degraded to
  full-refresh), unconvertible config (`grants`, hooks, `database`), seeds,
  and package dependencies.
- `UNSUPPORTED` — not emitted: `snapshot`/`custom` materialisations, snapshot
  and analysis files, disabled models, exposures/metrics/semantic models.

The report (`--check`) shows per-kind counts by classification, model
conversion coverage, and deduplicated review reasons. `--json` returns the
full `MigrationReport` (per-resource issues with stable `DBT0xx` codes,
transformations, notes, and source hashes); the manifest mirrors it for
idempotence auditing.

## Deliberate limits

- Jinja is scanned and classified, never executed. Unknown `{{ }}`/`{% %}`
  constructs are preserved verbatim so the generated file fails loudly at
  `check` time rather than silently changing meaning.
- A non-watermark `is_incremental()` body is dropped (or its `else` branch
  kept) and the model degrades to a correct full-refresh, flagged `REVIEW`.
- `profiles.yml` credentials are never read; only non-secret target fields are
  used as workspace defaults.
- Seeds, macros, and packages are reported but not translated. `ref()` calls
  *to* a seed resolve — to the relation the CSV would materialise as — so
  dependent models emit valid SQL and classify CLEAN; only the seed resource
  itself is REVIEW, since nothing loads the CSV for you.
