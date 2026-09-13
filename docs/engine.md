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
├── phlo-transform-openlineage/  canonical lineage graph → OpenLineage export
└── phlo-transform-cli/      `phlo-transform` binary
```

Dependency direction stays one-way:
`cli → trino → engine → {core, openlineage} → core → sql`. The compiler core
gains no async, warehouse or storage dependencies.

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
    async fn append(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError>;
    async fn merge(&self, relation: &Relation, key_columns: &[String], sql: &str) -> Result<QueryResult, AdapterError>;
    async fn replace_partitions(&self, relation: &Relation, partition_columns: &[String], sql: &str) -> Result<QueryResult, AdapterError>;
    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError>;
    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError>;
    async fn ensure_catalog(&self, request: &CatalogRequest) -> Result<(), AdapterError>;
    async fn ensure_schema(&self, relation: &Relation) -> Result<(), AdapterError>;
    async fn source_state(&self, relation: &Relation) -> Result<Option<String>, AdapterError>;
    async fn partition_counts(&self, relation: &Relation, partition_columns: &[String]) -> Result<Option<Vec<(String, i64)>>, AdapterError>;
}
```

`AdapterError` carries a stable `code`, message and `retryable` flag.

The Trino adapter (`phlo-transform-trino`) speaks the Trino client protocol
directly (`POST /v1/statement`, follow `nextUri`), supports optional basic
auth and session catalog/schema, returns query IDs, and maps Trino error names
into adapter error codes. `create_or_replace_table` performs
`DROP TABLE IF EXISTS` then `CREATE TABLE ... AS`.

The DuckDB adapter (`phlo-transform-duckdb`) embeds DuckDB in-process and is
the zero-infrastructure local path. `merge` is emulated as delete-then-insert
on the key columns so behaviour does not depend on the bundled DuckDB
version; `source_state` falls back to a schema fingerprint. Select it with
`--adapter duckdb` (or `--duckdb-path <file>`; `:memory:` is transient). The
default database file is `.phlo/transform/local.duckdb`.

```bash
phlo-transform run --adapter duckdb
phlo-transform plan --adapter duckdb --duckdb-path /tmp/demo.duckdb
```

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

`Planner::plan` resolves the selection, expands it to be dependency-closed
(excluded models are never pulled back in; a dependent plans against the
existing materialisation with a warning, or the plan is rejected when none
exists), orders models topologically and compares each model's desired
content-addressed version against the version recorded for the target
environment. Actions:

- `build` — the relation is missing, the recorded version differs, or
  `--force` was passed;
- `skip` — the recorded version matches;
- `cached` — the identical version is materialised in another environment;
- `unknown` — compilation errors block a decision.

Every decided model carries structured `PlanReason`s — a stable `kind`
(`sql_semantic_change`, `dependency_change`, `source_change`,
`missing_relation`, `unchanged`, `forced`, `selected_dependency`,
`selection_expansion`, …), a human-readable `detail`, and an optional
`subject` naming the dependency or source that moved. Which input moved
comes from `VersionDetail`: the per-dependency version hashes and per-source
observed states recorded alongside each materialised version in the state
store. Each planned model also records its `membership` — `selected`,
`expanded` (`+` terms, `--upstream`/`--downstream`) or `dependency`
(closure) — and the plan echoes the resolved selection (terms, excludes,
matched, expanded, required) so `plan.json` is self-describing.

`plan` performs no mutation. If the workspace has compilation errors the
plan is marked `blocked`, carries the diagnostics, and `apply` refuses to
run. `phlo-transform explain <model>` shares the same diff
(`diff_reasons`), so a per-model explanation cannot drift from what `plan`
reports.

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

One selector engine (`phlo-transform-core::select`) is shared by `plan`,
`apply`, `run`, `test`, `lineage`, `impact` and `list` — commands differ
only in what they do with the resolved `Selection`, never in how terms
parse or match. Per term:

```text
assay.results        exact name, model:// URI, or unique name suffix
assay.*              prefix glob
tag:qc               -- @tags membership
namespace:assay      model namespace
source:lims          model reads a matching source
changed              desired version differs from recorded state
all | *              everything
```

`+name` adds transitive dependencies, `name+` adds transitive dependents,
`+name+` does both. Include terms (positional and `--select`) union;
`--tag`/`--workflow` intersect; `--exclude` subtracts last and is absolute.
`--changed` is shorthand for the `changed` term. Its change set has two
providers:

- **State-derived** (`changed_models()`): desired version vs. the
  materialised version recorded for the environment (no state ⇒ everything
  is changed; ephemeral models are never reported — their edits propagate
  through dependents' dependency versions).
- **Git-derived** (`--since <ref>`, `phlo-transform-core::git`): semantic
  inputs that differ from `merge-base(<ref>, HEAD)` through the working
  tree — staged, unstaged and untracked files all count. This is the
  *directly changed* set only: unlike the state-derived set it does not
  include downstream models unless they were themselves touched; `changed+`
  adds the blast radius through the graph.

`--since` alone implies `--select changed`; combined with other include
terms it requires a `changed` term somewhere in the set (else the flag
would be silently ignored — an error instead). The provider runs a fixed
handful of Git invocations — `rev-parse`, `merge-base`, one
`diff --name-status -M`, `ls-files --others`, one `cat-file --batch` — and
maps each changed path once: model files by semantic signature (canonical
SQL + directives, so comment/format-only edits don't count), `seeds/*.csv`
to their consumers, `tests/*.sql` to their targets, `transform.toml` to
every model beneath it, `phlo.toml` narrowed by changed section
(`[transform]`/`[dependencies]`/defaults widen to all models). Deleted or
renamed-away models mark their dependents (they now read a source where a
model was). Anything not provably narrow widens rather than guesses.

Git changes surface as `git_change` *selection provenance* on the plan —
why the model was selected — while rebuild reasons stay state/version-
based. `plan.git` carries the whole change set: requested ref, resolved
merge-base/HEAD, changed paths with statuses, direct models with causes,
seeds and consumers, tests, deleted model identities and out-of-workspace
paths.

Members carry provenance — which terms matched them directly and which
pulled them in through `+` — so the planner can explain membership and the
JSON plan can echo the resolved selection back to the caller.

## Operational state

A `StateStore` trait with a SQLite implementation (`rusqlite`, bundled)
records runs, per-model executions, per-test executions and materialised
model versions (including `version_detail` — the named dependency/source
inputs behind each version hash) at `.phlo/transform/state.db`. The version
records are what make planning and the `changed` selector state-aware; see
[`docs/state.md`](state.md).

## Artifacts

Written under `.phlo/transform/` with `schema_version = 2`:

| File | Contents |
|---|---|
| `manifest.json` | workspace root, roots, models, sources, tests |
| `graph.json` | dependency-graph nodes and edges |
| `lineage.json` | the canonical lineage graph document (models, datasets, columns, tests; direct/indirect/transformation/confidence on column edges) |
| `openlineage.json` | the same graph exported as an OpenLineage design-time document (a JSON array of valid `JobEvent`/`DatasetEvent`s) |
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

Contracts, content-addressed state, smart skip/caching, incremental
materialisations, Nessie/WAP, data diff and the daemon remain later phases.
