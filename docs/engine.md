# Phase 1 engine architecture

This document describes the MVP build engine added in Phase 1. It complements
[`docs/architecture.md`](architecture.md) (the Phase 0 compiler) and
[`docs/roadmap/01-mvp-build-engine.md`](roadmap/01-mvp-build-engine.md).

## Crate layout

```text
crates/
├── phlo-transform-sql/      parser, directives, relation extraction
├── phlo-transform-core/     discovery, semantic model, compiler, DAG
├── phlo-transform-engine/   adapter trait, planner, scheduler, state, artifacts
├── phlo-transform-trino/    Trino HTTP adapter
└── phlo-transform-cli/      `phlo-transform` binary
```

Dependency direction stays one-way:
`cli → trino → engine → core → sql`. The compiler core gains no async,
warehouse or storage dependencies.

## Reserved name

The `phlo` host command is not built yet, so the binary is still
`phlo-transform` with `plan`, `apply`, `run` and `test` added to
`check`/`list`/`inspect`.

## Configuration and physical targets

Config precedence for materialisation is workspace → transform root → folder →
model directive.

Workspace (`phlo.toml`):

```toml
[transform]
default_materialization = "view"
default_catalog = "memory"
default_schema = "default"
```

Root/folder (`transform.toml`):

```toml
materialized = "table"
owner = "assay-team"
tags = ["assay"]

[folder.marts]
materialized = "view"
schema = "marts"
```

Model directives: `@view`, `@table`, `@materialized view|table`, `@tags a,b`,
`@owner <value>` (plus `@id` from Phase 0). Conflicting or malformed
directives are errors; unknown directives remain warnings.

Physical target derivation:

- `schema` = model override → workspace `default_schema` → the model namespace;
- `table` = `namespace__path__segments` when a shared schema is configured,
  otherwise just `path__segments`;
- `catalog` = workspace `default_catalog`.

When no `default_schema` is configured this yields the convenient
`assay.raw` → `memory.assay.raw`. When a shared schema is configured it folds
the namespace into the table name to keep targets unique. Colliding targets
are a compilation error (`PROJECT007`).

## Compiled SQL

Models are written as ordinary SQL against logical names. The compiler
rewrites every relation that resolves to a workspace model into that model's
physical target, and leaves external sources untouched. The rewrite mirrors
relation extraction, including CTE scope, so a CTE that shadows a workspace
model name is never rewritten.

## Adapter boundary

```rust
#[async_trait]
pub trait Adapter: Send + Sync {
    fn name(&self) -> &str;
    async fn relation_exists(&self, relation: &Relation) -> Result<bool, AdapterError>;
    async fn execute(&self, sql: &str) -> Result<QueryResult, AdapterError>;
    async fn create_or_replace_view(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError>;
    async fn create_or_replace_table(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError>;
    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError>;
    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError>;
}
```

`AdapterError` carries a stable `code`, message and `retryable` flag.

The Trino adapter (`phlo-transform-trino`) speaks the Trino client protocol
directly (`POST /v1/statement`, follow `nextUri`), supports optional basic
auth and session catalog/schema, returns query IDs, and maps Trino error names
into adapter error codes. `create_or_replace_table` performs
`DROP TABLE IF EXISTS` then `CREATE TABLE ... AS`.

Configuration is supplied by the CLI and environment, not committed project
files:

```bash
export PHLO_TRINO_ENDPOINT=http://localhost:8080
export PHLO_TRINO_USER=phlo
export PHLO_TRINO_CATALOG=memory
export PHLO_TRINO_SCHEMA=default
```

or `--trino-endpoint`, `--trino-user`, `--trino-password`, `--trino-catalog`,
`--trino-schema`.

## Planning

`Planner::plan` expands the selection to be dependency-closed, orders models
topologically, checks relation existence and assigns a conservative action:
missing → `create`, existing → `replace`. `plan` performs no mutation. If the
workspace has compilation errors the plan is marked `blocked`, carries the
diagnostics, and `apply` refuses to run.

## Execution

`Runner::apply` schedules models with Tokio and a bounded `Semaphore`.
Dependencies always run before dependents. On failure, dependents are marked
`blocked` transitively while independent branches continue. States are:

```text
pending ready running passed failed skipped blocked cancelled
```

After models, custom tests whose target models all passed are executed; a test
passes when it returns zero rows. Any failed model or test fails the run.

Cancellation is cooperative: `Runner::apply` races its scheduling loop against
a `CancelHandle`. On cancellation it aborts in-flight model tasks, marks every
unfinished model `cancelled`, skips tests and reports the run as `cancelled`.
The CLI wires `Ctrl-C` to the handle; tests can drive it programmatically.

## Tests

Custom SQL tests are discovered from `tests/**/*.sql`. A test is associated
with every workspace model it reads. `phlo-transform test` runs them against
the current target; `apply`/`run` run them after building.

## Selectors

Small and orthogonal, no expression language:

```bash
--select assay.results
--select 'assay.*'
--upstream
--downstream
--tag qc
--workflow assay
```

## Operational state

A `StateStore` trait with a SQLite implementation (`rusqlite`, bundled)
records runs, per-model executions and per-test executions at
`.phlo/transform/state.db`. This is operational history, not the
content-addressed desired-state engine of Phase 3.

## Artifacts

Written under `.phlo/transform/` with `schema_version = 1`:

| File | Contents |
|---|---|
| `manifest.json` | workspace root, roots, models, sources, tests |
| `graph.json` | graph nodes and edges |
| `lineage.json` | inferred columns and column inputs per model |
| `plan.json` | plan id, adapter, environment, planned models/tests, diagnostics |
| `run.json` | run id, plan id, status, model/test results, events |
| `environment.json` | provisioned base/candidate references and candidate catalog |
| `diff.json` | data/schema diff report (strategy, rows, columns, partitions, policies) |
| `promotion.json` | promotion id, references/hashes, gates, conflicts, timestamp |

`schema_version` makes the interface explicit and versionable.

## Observability

The engine emits structured `EngineEvent`s (`compile_started`,
`plan_created`, `model_started`, `model_finished`, `test_started`,
`test_finished`, `run_finished`, ...). Human CLI output is derived from the
same results.

## Testing strategy

- Planner, scheduler, blocking, concurrency, state and artifacts are tested
  with a fake adapter (no warehouse required).
- The Trino adapter is tested end-to-end against a disposable Trino container
  (testcontainers) in `crates/phlo-transform-trino/tests/trino_e2e.rs`. It is
  `#[ignore]`d by default and run explicitly in CI on Docker-enabled runners.

## Deferred

Column/type inference, column lineage, contracts, content-addressed state,
smart skip/caching, incremental materialisations, Nessie/WAP, data diff and
the daemon remain later phases.
