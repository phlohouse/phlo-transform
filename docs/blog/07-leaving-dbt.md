# 7. Leaving dbt without losing your work

A new engine is worthless if adopting it means rewriting a thousand models
by hand. Phlo ships a one-way translator, `phlo-transform translate
--from dbt`, that reads a dbt project and emits the smallest equivalent
native workspace. It is a migration tool, not a compatibility runtime: the
output is plain Phlo SQL, and no dbt semantics survive into the engine.

The full walkthrough lives in the
[migration guide](../dbt-migration-guide.md). This post covers what the
translator is doing and why.

## Translate intent, not syntax

The naive approach to migration is a Jinja-to-Jinja mapping: keep the
templates, swap the functions. That preserves every accidental behaviour,
including the ones you were leaving dbt to escape. Phlo translates the
*meaning* instead.

```sql
-- dbt
{{ config(materialized = 'incremental', unique_key = 'order_id', incremental_strategy = 'merge') }}

select order_id, customer_id, amount, status, ordered_at
from {{ ref('stg_orders') }}
{% if is_incremental() %}
where ordered_at > (select max(ordered_at) from {{ this }})
{% endif %}
```

becomes:

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

`ref()` becomes the logical name. `source()` becomes the physical relation.
`var()` becomes the literal. `unique_key` becomes `key=`, which already
carries identity, so the separate unique and not-null tests on that column
fold away. The hand-written watermark filter disappears because the
strategy knows how to find new rows. The output is shorter than the input
on purpose: semantic compression, not syntax mapping.

## Honest classification beats fake coverage

The translator never pretends. Every resource gets one of three labels:

- **CLEAN**: translated to native semantics, nothing left to fix.
- **REVIEW**: emitted, but needs a human. Macro calls are preserved
  verbatim so they fail loudly at `check` instead of silently changing
  meaning. `ephemeral` models come out as views. `is_incremental()`
  else-branches degrade to a correct full refresh.
- **UNSUPPORTED**: not emitted at all. Snapshots, analyses, exposures,
  metrics, and disabled models land here. The report and the manifest list
  them with reasons.

`--check` writes nothing and prints the classification summary. `--json`
returns the same report for automation, with a stable `DBT0xx` code per
issue. `--out <dir> --verify` writes the workspace and compile-checks it,
exiting non-zero when residual Jinja fails to parse, because REVIEW output
should not be mistaken for a working project.

A manifest lands at `.phlo/migration/dbt-translation.json`: every resource,
its classification, the transformations applied, and a hash of the source
file. The migration is auditable and rerunnable.

## Does the result actually run?

Yes, and that claim is tested, not aspirational. The repo carries a
fixture, `fixtures/dbt-shop`, that translates 100% CLEAN and runs on the
bundled DuckDB adapter with zero edits:

```bash
phlo-transform -r fixtures/dbt-shop translate --from dbt --out generated --verify
cd generated
duckdb shop.duckdb < seed.sql
phlo-transform run --adapter duckdb --duckdb-path shop.duckdb
```

First run builds everything and runs 11 tests. Insert a row into the source
and run again: the keyed incremental merges the update, the window
incremental appends past its watermark, and a no-change third run reports
all `SKIP`. The end-to-end test
[`translated_dbt_project_runs_on_duckdb`](../../crates/phlo-transform-cli/tests/cli.rs)
asserts all of that in CI.

For the parts that cannot be CLEAN, such as a `dbt_utils.star` call or a
project macro, the translator keeps the call verbatim and flags the model
REVIEW. You get a parse error pointing at the generated file, you fix the
call site by hand, and you move on. Honest breakage beats plausible
wrongness.

## Where this leaves you

The series started from a question: why does SQL need a build system? Phlo's
answer in one line per post:

1. Transformations are builds, so treat them like builds.
2. SQL stays SQL: dependencies come from the `from` clause.
3. The whole repo is one typed graph.
4. Every mutation follows an explainable plan.
5. Incremental is a declaration, not a macro.
6. Production changes are written, audited, and promoted on evidence.
7. Getting here from dbt is a command, not a rewrite.

Sources and further reading: [`SPEC.md`](../../SPEC.md) for the full design
and [`docs/roadmap/`](../roadmap/README.md) for honest implementation status.
