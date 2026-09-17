# 5. Incremental models without macros

A table model is easy to reason about if you rebuild the whole table every time:

```text
old table
   │
   └── replace with result of current query
```

That is simple and usually correct. It can also be wasteful.

If a ten-billion-row event table receives one million new rows, rebuilding all ten billion rows to incorporate that million may be unnecessary.

An **incremental model** updates only the part that needs changing.

The hard part is not performance. The hard part is preserving correctness while doing less work.

## Incremental is not one algorithm

“Incremental” describes a goal, not an implementation.

Different data has different update semantics.

### Append

New rows only arrive at the end.

```text
existing rows + new rows
```

### Keyed update

A row has an identity such as `sample_id`, and newer input may replace the previous value for that identity.

```text
sample_id=42, result=1.2
            ↓
new input:  sample_id=42, result=1.4
            ↓
replace/update row 42
```

### Partition replacement

A bounded partition such as `run_date=2026-09-17` can be recomputed as a unit without touching other partitions.

### Time window

Rows can be selected after a known processing frontier, usually with some overlap to catch late-arriving data.

These are different operations. Treating all of them as one “incremental mode” pushes the complexity somewhere else.

## Why Phlo declares the strategy

In a templated transformation system, incremental behaviour often lives inside the model itself:

```text
if incremental:
    add this filter
else:
    run something else
```

Now the SQL has two possible meanings depending on runtime state.

Phlo instead makes the incremental **strategy** explicit configuration.

Examples:

```sql
-- @incremental append
select ...
```

```sql
-- @incremental key=experiment_id
select ...
```

```sql
-- @incremental partition=run_date
select ...
```

```sql
-- @incremental window=updated_at
select ...
```

A time-window model can also be configured with an overlap:

```toml
[model.events.incremental]
strategy = "time-window"
column = "updated_at"
overlap = "2h"
```

The SQL still defines the desired rows. The strategy tells the engine how an existing materialisation may be updated safely.

## Strategy is part of model identity

Changing:

```text
append
```

to:

```text
keyed merge on sample_id
```

is not a cosmetic setting change. It changes how state is maintained.

So incremental configuration contributes to the model version.

A plan can therefore say:

```text
BUILD assay.results
      incremental strategy changed
      full rebuild required
```

rather than quietly applying a new maintenance algorithm to a table created under old assumptions.

## Bootstrap comes first

An incremental strategy only makes sense once a target exists.

On the first run, Phlo creates the table from the full model query.

```text
no target
   │
   ▼
full table build
   │
   ▼
future runs may be incremental
```

The same is true whenever a change invalidates the incremental assumptions strongly enough that a full rebuild is safer.

Correctness always has a fallback: rebuild the desired table from scratch.

## Append

For append-only data, the physical operation is conceptually:

```sql
insert into target
select ...
```

The responsibility for deciding *which rows the query represents* still belongs to the strategy/state model, not a hidden macro branch in the model.

Append is appropriate only when duplicate/replayed input semantics are understood. If the model actually needs row replacement, keyed merge is the right declaration.

## Keyed merge

Suppose:

```sql
-- @incremental key=sample_id
select
    sample_id,
    result,
    measured_at
from staging.results
```

The key says `sample_id` identifies a row.

On an engine with native `MERGE`, the adapter can update/insert by key.

On another engine, the same semantics can be implemented by a safe delete-and-insert sequence.

The important point is architectural:

> The model declares row identity; the adapter owns physical SQL mechanics.

That same key can also drive uniqueness tests and keyed data diffs. The user does not have to restate row identity for three different subsystems.

Composite keys work the same way:

```sql
-- @key experiment_id,sample_id
```

means the **pair** is unique, not that either column is individually unique.

## Partition replacement

Partitioned data often has a natural unit of recomputation.

If an upstream change affects only:

```text
run_date = 2026-09-17
```

we can replace that partition instead of the entire table.

Conceptually:

```text
partition A  unchanged
partition B  replace
partition C  unchanged
```

On Iceberg, partition metadata is also useful later for branch diffs because the engine can identify which partitions changed without scanning the whole table first.

## Time-window incrementals

Time-window processing needs durable state.

Suppose the model declares:

```sql
-- @incremental window=updated_at
select ...
```

After a successful run, Phlo stores a watermark such as:

```text
2026-09-17 10:42:00
```

The next execution can derive a predicate from that state:

```sql
updated_at > <previous watermark>
```

With overlap, the lower bound is moved backwards intentionally:

```text
stored watermark = 10:42
2h overlap       = 08:42 lower bound
```

Overlap is useful when sources can deliver late updates.

## Watermarks advance only on success

This is one of the most important invariants in an incremental engine.

Imagine a run reads through 12:00, starts writing, and fails halfway through.

If the watermark were advanced to 12:00 anyway, the next run might begin after data that was never successfully materialised.

So the sequence is:

```text
read old watermark
      │
      ▼
execute incremental write
      │
      ├── failure ──► keep old watermark
      │
      └── success ──► record new watermark
```

State advances with successful materialisation, not with attempted work.

## Cache adoption and watermarks

Executable cache reuse introduces an interesting case.

Suppose a Nessie candidate inherits exactly the same Iceberg output as `main`, and Phlo verifies the physical snapshot identity matches.

The candidate can adopt the existing materialisation without running model SQL.

For a time-window model, it also adopts the source environment's watermark.

That is safe because the watermark is a statement about the content that was materialised. If the content is proven identical, the processing frontier is identical too.

Again, Phlo does not fabricate a new “latest” timestamp. It preserves provenance from the physical output being reused.

## Failures, retries and incrementals

Retries need to distinguish transient infrastructure failures from semantic SQL failures.

A temporary Nessie or warehouse resource failure may be retryable.

A syntax error is not.

Phlo records structured attempts and only retries adapter failures classified as retryable. A successful retry records one successful materialisation transition; failed attempts do not advance watermarks or claim new output identity.

This matters because incrementals are stateful. Retrying blindly is much more dangerous when each attempt can mutate a target.

## Incremental models still participate in normal planning

Incremental does not bypass content-addressed state.

The planner still compares:

- desired model version;
- current environment record;
- source states;
- dependency versions;
- target existence;
- strong physical identity where available.

Only after the model is known to require execution does the runner choose the appropriate physical operation.

That keeps two questions separate:

```text
Should this model do work?
        ↓ planner

What physical update should it perform?
        ↓ incremental strategy + adapter
```

## Why this is simpler for users

The user writes the data transformation once:

```sql
select ...
```

and declares the state-maintenance intent:

```text
append
merge by key
replace partitions
advance by time window
```

The adapter layer owns warehouse-specific DDL and DML.

That is the same general Phlo design principle we have seen already:

> Put semantic intent in the model; put physical execution mechanics in the engine.

The next problem is broader than one model. How do you make changes to an entire lakehouse without writing directly into production and hoping the tests catch problems afterwards?

Phlo's answer is Write-Audit-Publish on Nessie branches.

*Next: [Safe by default: branches, audits and diffs](06-safe-by-default.md).*