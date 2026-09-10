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
  `INSERT` actions derived from the target's columns. Verified end-to-end
  against Trino/Iceberg, including idempotent re-application.
- **partition**: `DELETE FROM <target> WHERE (partition columns) IN (SELECT ...
  FROM source)` followed by `INSERT`, so only partitions present in the source
  are replaced and other partitions are untouched.
- **time-window**: appends rows where the window column is greater than the
  last committed watermark, optionally backed off by the configured `overlap`
  (`CAST('<watermark>' AS <type>) - INTERVAL '<overlap>' SECOND` for timestamp
  columns), then advances the watermark to `max(column)`. Watermarks are
  committed only on success.

The chosen operation is computed by the engine; adapters implement
`append`, `merge`, `replace_partitions` and `create_or_replace_table`.

## Schema evolution in planning

For an existing target, the planner compares the desired output schema with the
target's actual schema using `classify_schema_change`. Added nullable columns
are safe, numeric widening is review, removed columns force a full rebuild, and
incompatible changes are errors; `schema_change` and `full_rebuild` appear in
the plan.

## State

After a successful incremental build the engine records the strategy and key
alongside the materialised version. Failed builds do not record a
materialisation, so subsequent planning still sees the previous state
(`failed runs do not advance successful incremental state`). Time-window
models additionally persist a committed watermark (`incremental_state` table)
and only advance it after a successful build.

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
- planner full-rebuild on key change and on schema removal;
- fake-adapter bootstrap-then-merge, partition replacement and time-window
  watermark flows;
- live Trino/Iceberg `MERGE` bootstrap/merge/idempotence.

## Deferred

Partition-metadata pruning for the *incremental* partition strategy (the
diff partition strategy does use Iceberg `$partitions`); arbitrary
user-defined incremental algorithms; streaming.
