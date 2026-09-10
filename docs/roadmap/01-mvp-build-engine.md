# Phase 1 — MVP build engine

## Objective

Turn the compiler spike into a genuinely useful transformation runner for Trino.

At the end of this phase, a developer should be able to discover a multi-root workspace, compile its model DAG, inspect an execution plan, build table/view models in dependency order, run tests, and persist enough state to explain the run.

This phase deliberately does **not** yet attempt typed column analysis, content-addressed invalidation, declarative incremental models, Nessie branches or data diffing.

## Required capabilities

### Trino connection and configuration

Add a minimal target configuration with credentials kept outside committed project configuration where possible.

The engine must support:

- endpoint/catalog/schema configuration;
- authentication hooks appropriate to the deployment;
- query execution;
- query cancellation;
- relation existence checks;
- basic relation metadata introspection;
- structured Trino errors.

Do not expose every Trino client setting through Phlo configuration initially.

### Materialisations

Support:

```text
view
table
```

Default materialisation should be configurable at workspace/root/folder/model levels using the precedence defined in `SPEC.md`.

Model directives:

```sql
-- @view
```

```sql
-- @table
```

The adapter owns dialect-specific DDL generation.

### Planning

Implement:

```bash
phlo transform plan
```

The initial planner should show:

- selected models;
- topological execution order / parallelisable groups;
- current relation existence;
- desired materialisation;
- create/replace/no-op intent where determinable;
- tests that will run;
- unresolved/external sources;
- compilation errors that block execution.

Do not pretend this is full state-aware planning yet. The plan may conservatively rebuild selected models.

### Apply

Implement:

```bash
phlo transform apply
```

Requirements:

- dependency-aware execution;
- bounded concurrency;
- no downstream execution after an upstream failure;
- independent DAG branches may continue;
- cancellation support;
- structured per-model status;
- adapter query IDs captured where available.

### Convenience `run`

```bash
phlo transform run
```

For development, `run` may perform `plan + apply` in one command.

Keep explicit `plan` and `apply` as the underlying model.

### Execution states

Use a small stable state machine:

```text
pending
ready
running
passed
failed
skipped
blocked
cancelled
```

`cached` is reserved for the later state-aware phase.

### Tests

Support custom SQL tests returning violating rows.

Default convention:

```text
tests/**/*.sql
```

A test passes when its result set has zero rows.

Tests must be representable in the graph/artifacts and associated with the model(s) they validate where that can be inferred or explicitly declared.

### Basic model directives

Support the minimal directive parser from Phase 0 plus:

```text
@table
@view
@tags
@owner
```

Do not add incrementality or full contract semantics yet.

### Selectors

Support a small set:

```bash
--select assay.results
--select 'assay.*'
--upstream
--downstream
--tag qc
--workflow assay
```

Avoid implementing a selector expression language.

### Run artifacts

Write structured artifacts under:

```text
.phlo/transform/
```

At minimum:

```text
manifest.json
graph.json
plan.json
run.json
```

Artifacts are versioned schemas and should not simply serialize arbitrary internal structs.

### Operational state

Introduce a small state-store abstraction and one local implementation.

SQLite is the preferred default unless a concrete implementation reason favours DuckDB.

Persist at least:

- run ID;
- plan ID;
- model ID;
- materialisation;
- execution status;
- timestamps;
- SQL/compiled SQL hash;
- target relation;
- query ID;
- error code/message;
- test result.

This is operational history, not yet the content-addressed desired-state engine of Phase 3.

## Scheduler

Use Tokio with bounded task execution.

Pseudo-behaviour:

```text
while unfinished nodes remain:
  mark nodes READY when all dependencies passed/skipped
  execute READY nodes up to concurrency limit
  mark dependants BLOCKED when required dependency fails
```

The scheduler should not know Trino-specific details.

## Adapter boundary

Keep the initial adapter trait compact, for example:

```rust
trait Adapter {
    async fn relation_exists(&self, relation: &Relation) -> Result<bool>;
    async fn execute(&self, sql: &str) -> Result<QueryResult>;
    async fn create_or_replace_view(&self, model: &CompiledModel) -> Result<QueryResult>;
    async fn create_or_replace_table(&self, model: &CompiledModel) -> Result<QueryResult>;
    async fn cancel(&self, query_id: &str) -> Result<()>;
}
```

Do not over-generalize for unimplemented warehouses.

## Safety behaviour

Execution must fail before writes when:

- workspace compilation fails;
- graph contains a cycle;
- selected target relation names collide;
- configuration is ambiguous or invalid;
- a selected model has an unresolved workspace dependency that was expected to be internal.

External catalogue relations are valid inputs.

## CLI examples

```bash
phlo transform check
phlo transform plan
phlo transform plan --select 'assay.*'
phlo transform apply
phlo transform run --workflow assay
phlo transform test
phlo transform inspect assay.results
phlo transform list --json
```

## Integration fixtures

Provide a realistic small project with:

```text
transforms/shared/
workflows/assay/transforms/
workflows/reporting/transforms/
tests/
```

Tests should verify cross-root execution ordering.

Use a disposable Trino-backed test environment where practical. Unit tests should not require a live warehouse.

## Observability

Emit structured lifecycle events:

```text
compile_started
compile_finished
plan_created
model_queued
model_started
model_finished
test_started
test_finished
run_finished
```

Human CLI rendering should consume the same event/result model.

## Acceptance criteria

Phase 1 is complete when:

1. table and view models execute successfully against Trino;
2. models across multiple workflow transform roots execute in correct dependency order;
3. independent graph branches can run concurrently;
4. downstream models are blocked after an upstream failure while independent branches continue;
5. `plan` clearly shows selected work before mutation;
6. custom SQL tests run and can fail the command;
7. `manifest.json`, `graph.json`, `plan.json` and `run.json` are emitted with documented schemas;
8. run history persists locally;
9. `check`, `plan`, `apply`, `run`, `test`, `inspect` and `list` have JSON output where semantically applicable;
10. an end-to-end integration fixture is exercised in CI.

## Explicitly deferred

- column/type inference;
- column lineage;
- schema contracts beyond relation-level basics;
- content-addressed model versions;
- smart skip/caching;
- incremental materialisations;
- Nessie/WAP;
- data diff;
- daemon/service.

## Implementation notes

Phase 1 is implemented as described in [`docs/engine.md`](../engine.md).
Decisions worth calling out against this plan:

- The binary remains `phlo-transform`; the `phlo transform` host namespace
  does not exist yet.
- Crate layout is `phlo-transform-engine` (adapter trait, planner, scheduler,
  state, artifacts) and `phlo-transform-trino` (adapter). The compiler core
  stays synchronous and dependency-free of Tokio/Trino/SQLite.
- Physical targets use one configured schema and a flattened table name
  (`namespace__path`), which is an intentional MVP simplification; schema
  contracts and per-namespace schemas come later.
- Planning is conservative: existing relations are always `replace`; there is
  no skip/caching yet.
- `apply`/`run` plan first, so model mutations never begin when compilation or
  planning fails.
- The state store is SQLite (`rusqlite`, bundled) at
  `.phlo/transform/state.db`.
- Artifacts carry `schema_version = 1`.
- Planner/scheduler/state/artifacts are tested with a fake adapter; the Trino
  adapter is tested against a disposable Trino container and the test is
  `#[ignore]`d by default but run in CI.

