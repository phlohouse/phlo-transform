# 10. Putting Phlo Transform together

The earlier posts each isolated one idea: SQL compilation, the workspace graph, state, incrementals, WAP, migration, execution and the daemon.

This final post connects them into one end-to-end model.

The simplest way to understand Phlo Transform is to follow one transformation from a blank directory to an audited production promotion.

## The system in one diagram

```text
                    workspace files
                          │
                          ▼
                       compiler
                          │
                semantic workspace/DAG
                          │
             ┌────────────┼────────────┐
             │            │            │
             ▼            ▼            ▼
           plan        lineage      contracts/tests
             │
             ▼
      current state + live adapter evidence
             │
             ▼
      BUILD / SKIP / CACHED
             │
             ▼
           runner
             │
             ▼
           adapter
             │
       ┌─────┴─────┐
       ▼           ▼
    DuckDB       Trino/Iceberg/Nessie
                     │
                     ▼
             candidate environment
                     │
              audit evidence
                     │
                     ▼
                  promote
```

Each box has a deliberately narrow responsibility.

## Step 1: initialise a workspace

```bash
phlo-transform init
```

A tiny local model might be:

```sql
-- transforms/assay/results.sql
select
    sample_id,
    result
from raw.assay_results
```

At this point there is no warehouse state to compare with.

The source code describes desired computation only.

## Step 2: compile before running anything

```bash
phlo-transform check
```

The compiler:

1. discovers transform roots and SQL files;
2. parses SQL into syntax trees;
3. extracts relation references with real scope rules;
4. resolves workspace models versus external sources;
5. builds one dependency graph;
6. resolves schemas/types where possible;
7. derives lineage;
8. compiles tests and contracts;
9. resolves physical targets;
10. produces diagnostics for contradictions.

No model SQL has executed yet.

A compile failure is a workspace problem, not a half-finished warehouse run.

## Step 3: use DuckDB for the shortest local path

For local development:

```bash
phlo-transform plan --adapter duckdb
```

DuckDB runs embedded in the process. No Trino, object store or Nessie service is required.

On a fresh workspace, the plan will usually say `BUILD` because no materialisation record exists.

Then:

```bash
phlo-transform run --adapter duckdb
```

conceptually performs:

```text
compile
  ↓
plan
  ↓
load required seeds
  ↓
execute model DAG
  ↓
run tests
  ↓
persist run/materialisation state
```

The state store records what happened.

Run again without changing anything:

```bash
phlo-transform run --adapter duckdb
```

and valid current outputs become `SKIP`.

## Step 4: edit one model

Suppose the model changes to:

```sql
select
    sample_id,
    result,
    result * dilution as corrected_result
from raw.assay_results
```

The compiler derives a new semantic version.

The next plan can explain the change instead of merely noticing that a timestamp moved.

```text
BUILD assay.results
      SQL semantics changed
```

Any downstream model version that depends on `assay.results` changes transitively.

The graph turns a local edit into an explicit impact set.

## Step 5: use Git to narrow the selection

In a large repository:

```bash
phlo-transform plan --since main
```

maps changed files to model identities and expands only the required graph closure.

Git answers:

> Which definitions changed?

Phlo's compiler/state system answers:

> Given those definitions and current physical evidence, what actually needs work?

Those are different questions and both are useful.

## Step 6: move to the lakehouse path

The production architecture can use:

```text
Trino      query/execution engine
Iceberg    analytical table format
Nessie     versioned catalog/reference layer
```

Phlo's Trino adapter talks to Trino's HTTP protocol directly.

The workspace still contains ordinary SQL. The adapter changes physical execution, not the semantic programming model.

A base run might be:

```bash
phlo-transform run --adapter trino --catalog phlo_main
```

Successful materialisations can record strong Iceberg snapshot identities.

That enables much stronger state verification than simply checking whether a relation exists.

## Step 7: create a candidate environment

```bash
phlo-transform ref create ci/pr-42 --from main
```

Then:

```bash
phlo-transform run --ref ci/pr-42
```

Phlo resolves the candidate's Nessie branch and branch-scoped catalog, recompiles the workspace against that physical target, and plans the candidate.

Unchanged inherited tables can be `CACHED` when the candidate target reports the same verified Iceberg output identity recorded on the base.

Changed models build on the candidate.

Production `main` remains untouched.

## Step 8: understand why candidate reuse is safe

Suppose `main` recorded:

```text
model version:       V
output identity:     snapshot:S
physical slot:       analytics.assay__results
adapter:             trino
```

The candidate wants the same semantic version `V` in the same content slot but through its own catalog.

Phlo checks the candidate target live.

If it also reports:

```text
snapshot:S
```

then the candidate already sees the desired physical output.

The runner re-verifies the identity immediately before adoption.

If it still matches:

```text
CACHED
```

and no model SQL runs.

If it changed:

```text
BUILD
```

The engine does more work instead of making an unsupported equivalence claim.

## Step 9: audit the candidate

A run proves execution/test status, but publication needs broader evidence.

A branch diff can inspect candidate versus target:

```bash
phlo-transform diff --from ci/pr-42 --to main --full
```

Depending on model information, the diff can use:

- row keys;
- added/removed/modified counts;
- column-level changes;
- row counts;
- partition metadata;
- sampling;
- numeric tolerances.

Lineage differences can be recorded too.

Schema/contract changes are evaluated separately because a dataset can keep similar rows while breaking consumers structurally.

## Step 10: persist evidence where another machine can see it

A local JSON file is useful for a developer but insufficient for a multi-stage delivery pipeline.

Phlo persists environment, branch-diff and lineage-diff evidence into the configured state backend.

With Postgres:

```text
CI stage A: build + audit
             │
             ▼
       shared evidence store
             │
             ▼
CI stage B / operator: promote
```

The evidence is immutable and bound to candidate/target hashes.

If a workspace artifact is newer but cannot be persisted into the configured authority, it cannot authorise promotion locally.

There should not be one truth for the build machine and another for the approval machine.

## Step 11: preview promotion

```bash
phlo-transform promote ci/pr-42 --to main --check
```

Promotion evaluation brings together evidence from several layers:

```text
successful candidate run
candidate current head
base current head
branch/data diff
schema/contract safety
lineage evidence
merge/conflict check
```

If `main` advanced after the audit, the target-staleness gate fails.

If the candidate advanced after its run/diff, candidate-bound evidence is stale.

If a required diff never happened, “no evidence” does not mean “no changes”.

## Step 12: promote the exact audited state

```bash
phlo-transform promote ci/pr-42 --to main
```

The resulting promotion record captures:

- candidate and target identities;
- before/after target hashes;
- run and plan ids;
- gate results;
- conflicts if any;
- actor and timestamp;
- exact immutable evidence IDs consulted by evaluation.

That last point closes a subtle concurrency gap: the record does not perform a later “latest evidence” lookup that could accidentally point at evidence created after the decision.

Promotion history is therefore an audit trail of the actual decision.

## Step 13: clean up only what Phlo can prove it owns

Candidate cleanup may remove the branch and its branch-scoped catalog.

The catalog is dropped only when ownership is recorded.

If Phlo cannot prove that a catalog belongs to the candidate, it leaves the catalog alone.

This is the same fail-closed philosophy used throughout the system.

## Where resume and retry fit

Real runs fail.

If a process was interrupted before the run reached a terminal state:

```bash
phlo-transform run --resume <run-id>
```

continues the run while revalidating what earlier work can still be reused.

If a finished run failed:

```bash
phlo-transform run --retry-failed <run-id>
```

creates a new linked run over the failed portion.

A failed model that has become safely cache-adoptable can execute as `CACHED` on retry; cache reuse is part of execution semantics rather than a special planner display state.

## Where the daemon fits

Everything described above is accessible to machines through:

```bash
phlo-transform daemon
```

Read endpoints expose compiler/state truth.

Tracked operations submit runs, retries, tests and promotions.

Idempotency keys prevent accidental duplicate mutation when clients retry uncertain requests.

The daemon reuses the same compiler, planner, environment resolver, runner and promotion logic as the CLI.

That makes it an integration surface rather than a second transformation implementation.

## The crate structure follows the same boundaries

The Rust workspace is split roughly by responsibility:

```text
phlo-transform-sql          parsing/directives/relation extraction
phlo-transform-core         semantic model/compiler/DAG
phlo-transform-engine       planner/state/runner/audit/promotion
phlo-transform-duckdb       embedded local adapter
phlo-transform-trino        Trino adapter
phlo-transform-nessie       Nessie client
phlo-transform-openlineage  lineage export
phlo-transform-dbt          dbt translator
phlo-transform-daemon       machine API
phlo-transform-cli          user-facing binary
```

The compiler core does not need to know how to provision a Nessie catalog or execute a Trino query.

The adapter does not decide which model should rebuild.

The daemon does not reimplement promotion policy.

Those boundaries are as important as the features themselves.

## What v0.1 can honestly claim

Phlo Transform v0.1 has been exercised as more than a compiler prototype.

The release validation includes:

- local DuckDB install/run workflow;
- real Trino + Iceberg + Nessie execution;
- WAP promotion;
- executable cross-environment cache adoption;
- incremental MERGE;
- portable Postgres-backed evidence;
- daemon environment isolation;
- dbt migration fixtures and external projects;
- 100 / 1,000 / 5,000-model planning benchmarks.

That does not mean every part is finished forever.

## The important limitations

A few v0.1 boundaries are worth keeping visible.

### Warm planning cost

Strong Iceberg output verification currently reads per-table snapshot identity. At 5,000 models, a warm plan on the measured Trino/Nessie setup was around tens of seconds, while `--since` stayed much faster by probing only the relevant subset.

The correctness check is intentional; batching it is a future performance problem.

### Postgres TLS

The Postgres state client currently uses `NoTls`, so shared state belongs on trusted networks in v0.1.

### Nessie views

The Nessie-backed Iceberg catalog path cannot store views, so those projects materialise such outputs as tables.

### No generic cross-engine cache

Cache reuse solves the verified Phlo/Iceberg/Nessie case. It is not a claim that arbitrary outputs from different engines can be reused interchangeably.

### Dynamic dbt/Jinja semantics

The dbt translator handles what it can prove and classifies the rest for review/unsupported rather than becoming a permanent Jinja runtime.

## The through-line

The project has many features, but the same small set of principles keeps recurring.

### Infer what can be inferred safely

Dependencies come from SQL. Column lineage comes from expressions. Physical state is observed where adapters can prove it.

### Keep intent separate from mechanics

Models describe computation and incremental intent. Adapters own warehouse-specific execution.

### Plan before mutation

The system should explain why a model will build, skip or be adopted before it writes.

### Treat physical state as evidence, not assumption

A version hash alone does not prove a table still contains that output. Strong identity is checked live where available.

### Publish only audited state

Candidates are built in isolation and promotion is bound to exact run/diff/lineage/base evidence.

### Fail closed

When provenance, ownership, identity or compatibility cannot be established, Phlo rebuilds, rejects or leaves resources alone rather than making an optimistic correctness claim.

That is the core of Phlo Transform.

It is not “SQL with fewer braces” and it is not merely “dbt rewritten in Rust”.

It is an attempt to treat SQL transformation as the thing it becomes at scale: a compiled, stateful build system for data, with explicit evidence between source code and production state.

---

Further reading:

- [`SPEC.md`](../../SPEC.md) — complete design/specification
- [`docs/architecture.md`](../architecture.md) — compiler architecture
- [`docs/engine.md`](../engine.md) — execution engine
- [`docs/state.md`](../state.md) — state model
- [`docs/wap.md`](../wap.md) — environment and promotion semantics
- [`docs/daemon.md`](../daemon.md) — machine API
- [`docs/dbt-migration-guide.md`](../dbt-migration-guide.md) — dbt migration workflow
- [`docs/v0.1-release-notes.md`](../v0.1-release-notes.md) — v0.1 release notes