# Phase 4 incremental models

Incremental behaviour is declared as intent; the adapter chooses the physical
SQL. No user-written materialisation macros are required.

## Declaring incremental intent

```sql
-- @incremental append
-- @incremental key=experiment_id
-- @incremental key=experiment_id,sample_id
-- @incremental partition=run_date
-- @incremental window=updated_at
```

Or in `phlo.toml`:

```toml
[model.events.incremental]
strategy = "time-window"
column = "updated_at"
overlap = "2h"
```

`@incremental key=...` also implies identity: the columns become `@key`
columns, so unique/not-null assertions and generated tests follow.

## Internal representation

`IncrementalStrategy` (`append`, `key`, `partition`, `time-window`) is part of
the effective `ModelConfig` and therefore of the model-version `config_hash`.
No materialisation SQL lives in compiler structures.

## Planning

Plans expose the strategy, desired/current versions and reasons, for example:

```text
BUILD assay.events  [incremental] assay.events
  reason: SQL semantics changed
  strategy: key
```

When the target does not exist, or an incremental key/strategy changed, the
plan marks `full_rebuild`. A missing recorded strategy is treated as needing a
bootstrap.

## Execution

- **Bootstrap / full rebuild**: `CREATE TABLE ... AS` (or replace).
- **append**: `INSERT INTO <target> <select>`.
- **key**: `MERGE INTO <target> USING (<select>) ON key...` with `UPDATE` and
  `INSERT` actions derived from the target's columns.
- **partition** and **time-window**: the intent is represented and planned, but
  execution currently uses a safe full rebuild rather than claiming precision
  the engine does not yet have.

The chosen operation is computed by the engine; adapters implement `append`
and `merge` (plus `create_or_replace_table` for bootstrap).

## State

After a successful incremental build the engine records the strategy and key
alongside the materialised version. Failed builds do not record a
materialisation, so subsequent planning still sees the previous state
(`failed runs do not advance successful incremental state`). A dedicated
time-window watermark table is not yet implemented.

## Schema evolution

`classify_schema_change(desired, current)` returns `safe`, `review`,
`full_rebuild_required` or `error`:

- added nullable column → safe;
- numeric widening → review;
- removed column → full rebuild required;
- incompatible narrowing/type change → error.

## Tests

- directive and config parsing (append/key/partition/window, composite keys);
- strategy inference and key identity;
- planner full-rebuild on key change;
- fake-adapter bootstrap-then-merge flow;
- schema-change classification unit tests.

## Deferred

Partition/time-window precise execution and watermarks; arbitrary
user-defined incremental algorithms; Nessie/WAP; data diff; streaming.
