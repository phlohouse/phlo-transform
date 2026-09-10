# 4. Plan before you apply

The most expensive habit in data tooling is "run first, ask questions
later". `dbt build` against production tells you what it is doing only by
doing it. Phlo splits every mutation into a plan that explains itself and
an apply that executes what the plan said.

## What a plan looks like

```text
$ phlo-transform plan --adapter duckdb --duckdb-path shop.duckdb

Models (5)
  BUILD  staging.stg_customers    [view] staging.stg_customers
           reason: target relation does not exist
  BUILD  staging.stg_orders       [view] staging.stg_orders
           reason: target relation does not exist
  BUILD  marts.customer_orders    [table] marts.customer_orders
           reason: target relation does not exist
  BUILD  marts.daily_revenue      [incremental] marts.daily_revenue
           reason: target relation does not exist
           strategy: time-window
           full rebuild required
  BUILD  marts.orders_incremental [incremental] marts.orders_incremental
           reason: target relation does not exist
           strategy: key
           full rebuild required
```

Every model carries an action, `build`, `skip`, or `cached`, and the
reason for the action. The plan is written to `.phlo/` as a versioned
artifact, so the decision is an auditable record and not console output.
`plan --json` gives the same record to machines.

A workspace that does not compile blocks the plan and exits non-zero.
Nothing executes against a broken graph:

```text
plan blocked by compilation errors
  UNKNOWN marts.labelled    [table] marts.labelled
  ...
error[PARSE001]: could not parse model SQL ... --> transforms/marts/labelled.sql
```

## How "what changed" gets computed

Each compiled model gets a content-addressed version: a hash over the
canonical SQL semantics, the config, the contract, the dependency versions,
the source states, and the compiler semantics version. After a run, the
materialised version lands in local state at `.phlo/transform/state.db`.

The next `plan` is a comparison, not a guess:

```text
$ phlo-transform run    # second time, nothing changed

  SKIP   staging.stg_customers   [view] staging.stg_customers
  SKIP   staging.stg_orders      [view] staging.stg_orders
  SKIP   marts.customer_orders   [table] marts.customer_orders
  ...
```

Each change kind has its own reason. Edit the SQL and the reason is `SQL
semantics changed`. Edit config and the reason is `config changed`. A
source moves and the reason is `a source state changed`: on Iceberg the
state is a snapshot id, on DuckDB a schema and row-count fingerprint. A
dependency's version moves and the reason is `an upstream model version
changed`, propagated transitively.

Two properties matter here:

- **The hash is semantic, not textual.** Reformatting a file does not
  rebuild data, because the version covers canonicalised SQL rather than
  bytes.
- **Stale plans are rejected.** A plan binds to the state it was computed
  against. If the world moved on, you plan again. Apply never executes a
  decision made against a different reality.

## `inspect` shows the same accounting per model

```text
State:
  desired:  8974fcb
  current:  8974fcb
  status:   unchanged
```

Desired is what the compiler wants now. Current is what is materialised in
this environment. That one comparison, computed instead of assumed, is the
core of state-aware execution.

## Why the split pays for itself

`run` is a convenience wrapper for plan, apply, and tests. The split is
what the rest of this series builds on: promotion gates that require a
green run on a candidate branch, data diffs computed before merge, and CI
that fails on a plan which would rebuild production. All before a single
byte is written.

*Next: [Incremental models without macros](05-incremental-without-macros.md).*
