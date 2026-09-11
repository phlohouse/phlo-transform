# Guide: migrating a dbt project to Phlo Transform

This guide walks through converting an existing dbt project into a native
Phlo Transform workspace and running it — from the first analysis through to
incremental rebuilds. It uses `fixtures/dbt-shop`, a small but realistic dbt
project: two sources, staging views, a table mart, a keyed (`merge`)
incremental, a watermark incremental, folder-level config and schema tests.

The translator is a **one-way migration tool, not a dbt runtime**. Jinja is
scanned and classified, never executed, and the output is plain Phlo SQL —
simpler than the dbt input. See [`dbt-migration.md`](dbt-migration.md) for the
full translation table.

## 0. Prerequisites

```bash
# build the CLI (installs `phlo-transform`)
cargo install --path crates/phlo-transform-cli

# for local execution you also want the DuckDB shell
brew install duckdb    # macOS
```

Everything in this guide runs locally — no Trino, Nessie or credentials
required. To target Trino instead, see "Running against Trino" below.

## 1. Analyse the dbt project

Point `--root` (or `-r`) at the dbt project and run `translate --check`. This
writes nothing — it is a dry run that classifies every resource it finds.

```bash
cd my-dbt-project
phlo-transform translate --from dbt --check
```

![translate --check and --verify](assets/translate.gif)

```text
dbt migration analysis

Project: shop (.)

Resources
  models                5
  sources               2
  singular tests        1

CLEAN
  models                5
  sources               2
  singular tests        1

Model conversion coverage: 100.0%
```

Every resource gets a classification — nothing is silently treated as
equivalent:

- **CLEAN** — translated to native semantics with no residual Jinja. Safe to
  run as-is.
- **REVIEW** — emitted, but needs a human: residual Jinja (macros,
  `{% set %}`, `{% for %}`, non-watermark `{% if %}`), `ephemeral` models
  (emitted as views), environment-dependent expressions (`env_var`,
  `run_started_at`, `target.*`), unresolved `ref`/`source`/`var`, and
  unconvertible config (`grants`, hooks).
- **UNSUPPORTED** — not emitted at all: snapshots, analyses, disabled models,
  exposures, metrics, semantic models.

Use `--json` for the machine-readable report (per-resource issues with stable
`DBT0xx` codes, per-file transformations and source hashes):

```bash
phlo-transform translate --from dbt --check --json
```

## 2. Emit the translated workspace

```bash
phlo-transform translate --from dbt --out ../generated --verify
```

- `--out <dir>` is required to write; `--check` never writes.
- Existing files are never overwritten unless `--overwrite` is passed.
- `--verify` compiles the generated workspace and exits non-zero if it does
  not pass `check` — which is how REVIEW models with residual Jinja surface.
  Files are still written so you can inspect and fix them.
- A manifest lands at `.phlo/migration/dbt-translation.json` recording what
  was translated, so the migration is auditable and idempotent.

```text
Wrote 13 file(s) to ../generated
Manifest: .phlo/migration/dbt-translation.json

Workspace: ../generated
Roots:     1
Models:    5
Sources:   2
Tests:     11

check passed
```

## 3. What the output looks like

The generated project is deliberately smaller than the dbt input:

```text
generated/
├── transforms/
│   ├── staging/
│   │   ├── stg_customers.sql
│   │   ├── stg_orders.sql
│   │   └── transform.toml          # from models: shop: staging: +materialized
│   └── marts/
│       ├── customer_orders.sql
│       ├── daily_revenue.sql
│       ├── orders_incremental.sql
│       └── transform.toml          # from models: shop: marts: +materialized/+tags
└── tests/
    ├── assert_positive_amounts.sql  # singular test, refs resolved
    └── generated/                   # schema.yml tests → SQL
        ├── raw__orders__id__unique.sql
        ├── staging__stg_orders__status__accepted_values.sql
        └── ...
```

`phlo.toml` is only emitted when the project actually needs defaults (for
example a `default_namespace` for models that lived at `models/` root). Less
config, not a copy of dbt's.

### Before / after

**dbt input** (`models/marts/orders_incremental.sql`):

```sql
{{ config(materialized = 'incremental', unique_key = 'order_id', incremental_strategy = 'merge') }}

select order_id, customer_id, amount, status, ordered_at
from {{ ref('stg_orders') }}
{% if is_incremental() %}
where ordered_at > (select max(ordered_at) from {{ this }})
{% endif %}
```

**Phlo output** (`transforms/marts/orders_incremental.sql`):

```sql
-- translated from dbt model `orders_incremental` (models/marts/orders_incremental.sql)
-- @incremental key=order_id
-- @tags mart

select
    order_id,
    customer_id,
    amount,
    status,
    ordered_at
from staging.stg_orders
```

The key carries the merge semantics; the redundant filter is dropped (a keyed
merge over a full scan is still correct).

**Watermark incremental** — `is_incremental()` containing
`col > (select max(col) from {{ this }})` becomes a native window:

```sql
-- @incremental window=ordered_at
```

**Sources and vars** — `{{ source('raw', 'orders') }}` becomes the physical
relation `raw.orders`; `{{ var('default_region') }}` becomes its literal value
`'eu'`.

**Schema tests** — `unique`/`not_null` on a key column fold into `@key` /
`@incremental key=`; other `not_null`s become `-- @not-null col`; the rest
(`unique`, `accepted_values`, `relationships`) become ordinary test files
under `tests/generated/`.

## 4. Handling REVIEW and UNSUPPORTED output

A translated project does not have to be 100% CLEAN to be useful — but REVIEW
models are emitted **deliberately broken** so nothing silently changes
meaning. Running `check` on `fixtures/dbt-jaffle` output:

```text
error[PARSE001]: could not parse model SQL: sql parser error: Expected: identifier, found: {
  --> transforms/marts/labelled.sql
```

`labelled.sql` still contains `{{ label_status('status') }}` — the macro call
is preserved verbatim. Rewrite it as plain SQL (or inline the expression) and
the model compiles. Common fixes:

| REVIEW reason | What to do |
|---|---|
| project/package macro call with a dynamic body (`{% %}` statements, `run_query`) | inline the equivalent SQL by hand — simple expression-body macros and helpers like `dbt_utils.star`/`generate_surrogate_key`/`safe_cast`, `dbt_date.get_base_dates`, `dbt.date_trunc`/`current_timestamp` already lower statically |
| `ephemeral` model | emitted as a view — usually fine; inline it into consumers if you want it gone |
| `is_incremental()` else-branch kept | model is a correct full-refresh; add `@incremental` if you want incremental behaviour |
| `env_var`/`target.*`/`run_started_at` | replace with a literal or a Phlo-native mechanism |
| unresolved `ref`/`source`/`var` | fix the name or supply the var |

Seed CSVs are copied to `seeds/` and load automatically at `run` time —
no manual `duckdb` step is needed for them. UNSUPPORTED resources
(snapshots, exposures, metrics, semantic models, dbt `unit_tests`/`groups`)
are listed in the report and the manifest with reasons — they need a manual
decision, not a translation.

## 5. Run the translated project on DuckDB

Sources in the generated SQL are physical relations (`raw.orders`), so create
them as ordinary tables in a DuckDB file — this mirrors what a warehouse would
already contain:

```bash
cd generated
duckdb shop.duckdb < seed.sql   # create schema raw; create table raw.orders as ...
```

Then the standard lifecycle applies:

```bash
phlo-transform doctor --adapter duckdb --duckdb-path shop.duckdb
phlo-transform check
phlo-transform plan   --adapter duckdb --duckdb-path shop.duckdb
phlo-transform run    --adapter duckdb --duckdb-path shop.duckdb
```

![doctor and run](assets/run.gif)

`run` is plan + apply + tests in one step. First run builds everything; tests
include both the translated singular tests and the generated schema tests.

### The second run is where incrementals pay off

Insert a row and update one in the source, then `run` again:

```bash
duckdb shop.duckdb < mutate.sql   # insert new order; update order 10
phlo-transform run --adapter duckdb --duckdb-path shop.duckdb
```

![incremental second run](assets/incremental.gif)

- `marts.orders_incremental` (`@incremental key=order_id`) merges: the updated
  row is corrected in place, the new row is inserted — no duplicates.
- `marts.daily_revenue` (`@incremental window=ordered_at`) appends only rows
  past the committed watermark.
- A third run with no upstream change is a full `SKIP` — state-aware planning
  sees nothing changed.

## 6. Day-2 commands on the migrated workspace

```bash
phlo-transform explain marts.orders_incremental   # deps, columns, planned action + why
phlo-transform inspect marts.orders_incremental   # incl. desired vs materialised state
phlo-transform lineage marts.customer_orders      # upstream/downstream models
phlo-transform impact staging.stg_orders          # what breaks if this changes
phlo-transform list                               # models, sources, tests
phlo-transform manifest                           # last generated graph artifact
```

All commands accept `--json` for agents and CI, and `--select`,
`--upstream`/`--downstream`, `--tag` for selection.

## 7. Running against Trino

Same workspace, different adapter — no project changes:

```bash
export PHLO_TRINO_ENDPOINT=http://localhost:8080
export PHLO_TRINO_USER=phlo
export PHLO_TRINO_CATALOG=iceberg
export PHLO_TRINO_SCHEMA=analytics

phlo-transform run
```

The DuckDB adapter is the local/dev path; Trino + Iceberg + Nessie is the
production path (branches, Write-Audit-Publish, promotion, data diffs — see
[`docs/wap.md`](wap.md) and [`docs/diff.md`](diff.md)).

## 8. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `translate --verify` exits 1 after writing | REVIEW models with residual Jinja — expected; `check` tells you which files |
| `error: refusing to overwrite N existing file(s)` | pass `--overwrite`, or use a fresh `--out` |
| `error: '.' does not look like a dbt project` | run with `-r` pointing at the directory containing `dbt_project.yml` |
| `plan` shows `UNKNOWN` / "blocked by compilation errors" | fix the files `check` reports; nothing executes while blocked |
| `plan` keeps showing `SKIP` after source data changed | on DuckDB, source state is a schema + row-count fingerprint; in-place updates that keep the row count are invisible — `apply`/`run` the model directly or touch the schema |
| `inspect` shows `status: changed` right after a run | offline compilation can infer different types than the catalogue-enriched run; pass `--adapter`/`--duckdb-path` for accurate state |
| incremental model rebuilt fully | strategy/key changed or first bootstrap — `plan` prints `full rebuild required` with the reason |

## Regenerating the terminal GIFs

The GIFs in `docs/assets/` are recorded with [VHS](https://github.com/charmbracelet/vhs)
from the tapes in `docs/vhs/`:

```bash
vhs docs/vhs/translate.tape
vhs docs/vhs/run.tape
vhs docs/vhs/incremental.tape
```

The tapes operate on `/tmp/phlo-demo` (a copy of `fixtures/dbt-shop`); rebuild
the CLI first so the GIF reflects current output.
