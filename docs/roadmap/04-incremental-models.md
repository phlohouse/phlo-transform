# Phase 4 — Incremental models

## Objective

Add incremental materialisation without importing dbt's macro-driven complexity.

Users should describe **incremental intent**, while the adapter chooses safe physical SQL for the target.

Supported initial strategies:

```text
append
key
partition
time-window
```

A full rebuild remains an explicit fallback.

## Design rules

1. Incremental behaviour is declarative, not arbitrary code.
2. Incremental configuration participates in model-version hashing.
3. Unsafe configuration changes must force or recommend full rebuilds.
4. The plan must explain exactly what incremental work is proposed.
5. A model should not need to declare the same key separately for merge, uniqueness, diffing and identity.

## Syntax

### Append

```sql
-- @incremental append
```

Use when incoming rows are immutable and only new records should be added.

### Key-based

```sql
-- @incremental key=experiment_id
```

or equivalently:

```sql
-- @incremental key=experiment_id
-- @key experiment_id
```

Prefer one declaration where possible; if `@incremental key=x` exists it should normally imply `@key x` semantics unless explicitly weakened for a justified use case.

### Partition replacement

```sql
-- @incremental partition=run_date
```

Rebuild only affected partitions where the underlying engine/table format supports this safely.

### Time window

```sql
-- @incremental window=updated_at
```

The engine determines the last successfully processed boundary and applies an overlap policy if configured.

## Internal model

Avoid materialisation-specific SQL in core compiler structures.

Conceptually:

```rust
enum IncrementalStrategy {
    Append,
    Key { columns: Vec<ColumnId> },
    Partition { columns: Vec<ColumnId> },
    TimeWindow { column: ColumnId, overlap: Option<Duration> },
}
```

The adapter converts this intent into target-specific execution operations.

## Planning

Example:

```text
BUILD assay.results
  materialization: incremental
  strategy: key
  key: experiment_id
  reason: upstream source snapshot changed
  estimated action: merge candidate rows
```

Partition example:

```text
BUILD manufacturing.batch_summary
  strategy: partition
  partition: batch_date
  affected partitions:
    2026-09-09
    2026-09-10
```

If affected partitions cannot be determined safely, plan conservatively or require full rebuild rather than pretending precision.

## Full-rebuild detection

Configuration/state changes that should trigger a full rebuild include, where relevant:

- incremental key changed;
- incremental strategy changed incompatibly;
- partition column changed;
- target schema changed incompatibly;
- materialisation changed from non-incremental to incremental with no safe bootstrap;
- model identity/physical relation mapping changed in a way that invalidates state;
- state required for a window strategy is unavailable or inconsistent.

Example:

```text
FULL REBUILD REQUIRED: assay.results

Reason:
  incremental key changed

Previous:
  sample_id

Desired:
  experiment_id
```

## Bootstrap behaviour

If an incremental target does not exist, the first execution should create a full initial materialisation using the same logical model definition.

Do not require separate "initial" SQL.

## Key semantics

A key declaration should be reused by:

- incremental merge;
- uniqueness assertion;
- non-null assertion;
- keyed data diff;
- record-level diagnostics.

Composite keys must be supported:

```sql
-- @incremental key=experiment_id,sample_id
```

## Append safety

Append models should optionally support a deduplication guard when a key is also declared.

The default must not silently duplicate data because of a retried run if the execution strategy can avoid it.

Where true idempotence cannot be guaranteed, surface that limitation in the plan.

## Time-window state

Persist successful watermarks separately from generic model versions.

Suggested state:

```text
last_successful_value
overlap
observed_max_value
run_id
```

A failed run must not advance the committed watermark.

## Late-arriving data

Time-window strategy should support an overlap such as:

```toml
[model.events.incremental]
strategy = "time-window"
column = "updated_at"
overlap = "2h"
```

The compiler/executor should not invent a universal overlap default beyond a conservative documented value, if any.

## Schema evolution

Before incremental writes, compare desired output schema with target schema.

Classify changes as:

```text
SAFE
REVIEW
FULL_REBUILD_REQUIRED
ERROR
```

Examples:

- add nullable column: potentially safe;
- type widening: review/adapter-specific;
- remove key column: error/full rebuild;
- incompatible narrowing: block.

## Trino/Iceberg implementation

Initial implementation should optimise for Trino + Iceberg rather than lowest-common-denominator SQL.

Evaluate target-native approaches for:

- `MERGE`;
- append inserts;
- partition overwrite/replacement;
- CTAS/bootstrap;
- atomicity guarantees provided by Iceberg commits.

Keep the core strategy independent of the generated SQL.

## Testing

### Unit

- directive/config parsing;
- strategy validation;
- composite keys;
- rebuild-requirement classification;
- time-window state transitions;
- schema-change safety classification.

### Integration

For each strategy:

1. initial bootstrap;
2. second run with no changes;
3. new/changed source rows;
4. expected incremental output;
5. failed execution does not corrupt committed state;
6. configuration mutation produces correct rebuild decision.

### Idempotence

Reapply the same planned input where safe and confirm no duplicate or unintended output.

## Acceptance criteria

Phase 4 is complete when:

1. append, key, partition and time-window strategies are represented declaratively;
2. no user-written materialisation macros are required;
3. initial incremental runs bootstrap correctly;
4. key-based incremental execution is idempotent for supported Trino/Iceberg semantics;
5. planner explains incremental strategy and reason for work;
6. unsafe strategy/key/schema changes force or clearly require full rebuild;
7. failed runs do not advance successful incremental state;
8. keys are reused for validation and later diff semantics;
9. all strategies have end-to-end integration tests.

## Explicitly deferred

- arbitrary user-defined incremental algorithms;
- warehouse-specific strategy configuration beyond proven needs;
- Nessie/WAP promotion;
- data diffs as policy gates;
- streaming execution.
