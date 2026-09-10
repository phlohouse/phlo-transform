# Phase 6 — Native data diff

## Objective

Make output changes inspectable at the data level, not merely at the SQL/model level.

At the end of this phase, Phlo Transform should compare candidate and published model outputs, summarize meaningful row/value/schema changes, and optionally use those diffs as WAP promotion gates.

Data diff is a core safety and review feature, especially for versioned Iceberg/Nessie environments where candidate and published states can coexist cleanly.

## Design principles

1. Diffing is model-aware and should reuse keys, schemas, partitions and lineage already known to the compiler.
2. Large tables must not default to full cell-by-cell scans.
3. The plan must explain what comparison strategy will be used and its cost/coverage limitations.
4. Diff results are structured artifacts first; terminal presentation is a rendering.
5. Data-diff policies can block promotion, but the policy language must remain small and declarative.
6. A diff must identify exactly which two environment/model versions it compares.

## CLI

Implement:

```bash
phlo transform diff assay.results
```

Useful options:

```bash
phlo transform diff assay.results --ref feature/new-assay --base main
phlo transform diff assay.results --full
phlo transform diff assay.results --sample 10000
phlo transform diff assay.results --json
```

Default candidate/base references should be inferred only when unambiguous from current environment context.

## Comparison identity

Every diff records:

```text
model_id
candidate_ref
candidate_model_version
candidate_relation/snapshot
base_ref
base_model_version
base_relation/snapshot
strategy
key/partition columns
```

This prevents a diff report from becoming detached from the exact data states reviewed.

## Diff strategies

### Key-based

Preferred where a model has a declared/inferred stable key.

Produce:

- rows added;
- rows removed;
- rows present in both;
- rows modified;
- changed values by column;
- optional example keys/rows.

Example:

```text
assay.results

Rows
  main       128,231
  candidate  128,237
  delta           +6

Records
  added          12
  removed         6
  modified       31

Changed values
  concentration  23
  status          8
  sample_id       0
```

### Partition-based

For large partitioned data, first compare partition-level metadata/statistics and evaluate changed partitions only.

Useful outputs:

```text
partitions added
partitions removed
partitions changed
rows changed within changed partitions
```

### Aggregate

For large models without a key, compute summaries such as:

- row count;
- null counts;
- min/max;
- numeric mean/stddev where appropriate;
- approximate distinct counts where supported;
- deterministic checksums/hashes where feasible.

Aggregate comparison is not equivalent to row-level equality. Mark its coverage explicitly.

### Sampled

Use deterministic sampling for exploratory comparisons when full comparison is too expensive.

Sampling seed/strategy must be recorded so results are reproducible.

### Full

Explicit user request or policy for manageable datasets.

Perform complete row/value comparison using a stable strategy appropriate to the model.

## Strategy selection

Default selection should be compiler-informed:

```text
stable key available       -> key-based
partition metadata useful  -> partition-first, then keyed/aggregate within changes
no key, large table        -> aggregate + optional deterministic sample
small table                -> full where cost threshold permits
```

Cost thresholds should be explicit configuration, not hidden magic.

## Key reuse

Reuse the model's `@key` / incremental key automatically.

Do not require:

```text
incremental_key = experiment_id
diff_key = experiment_id
unique_key = experiment_id
```

as three separate settings.

Composite keys are supported.

## Schema diff integration

Data diff should include the Phase 2 schema comparison:

```text
Schema
  + dilution_factor DOUBLE
  ~ result FLOAT -> DOUBLE
  - legacy_result VARCHAR
```

Where columns are added/removed, value-level diffing must adapt accordingly and not report misleading comparisons.

## Column-aware changes

For key-based diffs, report per-column modification counts.

Potential structured output:

```json
{
  "column_changes": {
    "result": {"changed_rows": 31},
    "status": {"changed_rows": 8}
  }
}
```

Later statistical summaries may show magnitude/distribution changes, but basic correctness comes first.

## Numeric tolerances

Support optional explicit tolerances for floating-point comparisons.

Example:

```toml
[model.assay_results.diff.columns.concentration]
absolute_tolerance = 1e-9
relative_tolerance = 1e-6
```

Default exactness should follow type semantics and avoid arbitrary hidden tolerances.

## Null semantics

Define deterministic null comparison:

```text
NULL vs NULL -> unchanged
NULL vs value -> changed
value vs NULL -> changed
```

## Large object/text columns

Do not dump complete changed cell values into terminal/artifacts by default.

Store counts and bounded examples. Add explicit flags for deeper inspection.

## Sensitive data

Diff artifacts may expose record values. Support a policy to disable example-value emission or redact configured columns.

Example:

```toml
[diff]
include_examples = false
```

or column-level metadata such as `sensitive = true` when the wider Phlo metadata model supports it.

## Diff policies

Keep policy small and measurable.

Example:

```toml
[model.assay_results.diff]
max_removed_rows = 0
max_changed_fraction = 0.05
max_added_rows = 10000
```

Potential gates:

```text
max_added_rows
max_removed_rows
max_modified_rows
max_changed_fraction
require_full_diff
require_keyed_diff
```

Avoid arbitrary policy scripting in the first implementation.

## WAP integration

Candidate flow becomes:

```text
WRITE
  ↓
TEST / CONTRACT
  ↓
SCHEMA DIFF
  ↓
DATA DIFF
  ↓
POLICY
  ↓
PUBLISH
```

Promotion artifacts should reference the exact successful diff artifact used by the gate.

If the candidate changes after diffing, the prior diff is stale and cannot authorize promotion.

## Changed-model selection

`phlo transform diff --changed` should compare only materially changed models from the current plan/run.

Models skipped because the same version is already materialised need no data diff.

## Lineage-assisted review

The diff result may include downstream impact context:

```text
assay.results changed

Downstream:
  analytics.monthly_summary
  qc.release_export
```

This should reuse existing lineage rather than perform new analysis.

## Artifacts

Add:

```text
.phlo/transform/diff.json
```

or a run-scoped set of diff artifacts.

Schema should include:

```text
diff_id
plan_id
run_id
model_id
base/candidate identity
strategy
coverage
schema_changes
row_summary
column_summary
examples (bounded/optional)
policy_results
started_at
finished_at
```

## Performance

Use warehouse execution for comparisons rather than downloading whole datasets to the Rust process.

The Rust engine should compile and orchestrate diff queries, then collect summaries.

For Iceberg, exploit metadata/snapshots/partitions where this can safely avoid unnecessary scans.

## Failure behaviour

Distinguish:

```text
DIFF_EXECUTION_FAILED
DIFF_INCOMPLETE
DIFF_POLICY_FAILED
DIFF_NOT_POSSIBLE
```

A missing usable key does not automatically mean diff failure if aggregate/full comparison is possible.

If configured promotion policy requires a keyed/full diff and only aggregate coverage is possible, promotion must block.

## Testing

### Keyed fixtures

Cover:

- no change;
- additions;
- deletions;
- modifications;
- composite keys;
- duplicate-key violation;
- null changes;
- floating tolerance.

### Partition fixtures

Cover changed and unchanged partitions and verify unchanged partitions are not unnecessarily scanned where the adapter supports metadata pruning.

### Policy tests

Verify each threshold can pass/fail and that stale diff artifacts cannot authorize a later modified candidate.

### Scale test

Include at least one large synthetic model demonstrating that default strategy does not require full data transfer to the client.

## Acceptance criteria

Phase 6 is complete when:

1. `phlo transform diff <model>` compares exact base/candidate model states;
2. keyed models report added, removed and modified records plus per-column change counts;
3. partition/aggregate/sample/full strategies exist with explicit coverage semantics;
4. the engine reuses model keys and partition metadata rather than requiring duplicate diff configuration;
5. schema and data changes appear in one structured review result;
6. bounded/redacted output prevents accidental giant/sensitive artifacts by default;
7. declarative diff policies can block WAP promotion;
8. a changed candidate invalidates an earlier diff gate;
9. warehouse-side execution avoids downloading complete large tables to the Rust process;
10. integration tests validate correctness across representative Trino/Iceberg models.

## Explicitly deferred

- sophisticated distribution-shift statistics;
- anomaly detection/ML;
- visual diff UI;
- arbitrary policy scripting;
- cross-warehouse diffing.

## Implementation notes

Phase 6 is **partially implemented** (audited against code and tests). See
[`docs/diff.md`](../diff.md).

Implemented:

- keyed diff via warehouse-side `FULL OUTER JOIN` + `IS DISTINCT FROM`,
  reporting added/removed/modified/unchanged and per-column change counts;
- aggregate fallback, explicit `full`, real `TABLESAMPLE` `sampled`, and a
  partition strategy reporting added/removed/changed partitions;
- keys reused from `@incremental key=`/`@key`;
- config-driven policies and per-column numeric tolerances;
- populated `schema_changes` (added/removed/changed with safety);
- promotion reads `diff.json`, enforces `--require-diff`, and rejects a stale
  diff whose candidate version no longer matches;
- live Trino coverage for keyed, tolerance, partition-aware and sampled diffs.

Remaining gaps: statistical distribution summaries and an example-value
redaction policy (partition comparison now uses Iceberg metadata where
available).

