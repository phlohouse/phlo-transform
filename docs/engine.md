# Engine architecture

This document describes the build engine in `phlo-transform-engine`. It
complements [`docs/architecture.md`](architecture.md) (the compiler) and the
feature docs [`state.md`](state.md), [`incremental.md`](incremental.md),
[`wap.md`](wap.md), [`diff.md`](diff.md) and [`daemon.md`](daemon.md).

## Crate layout

```text
crates/
├── phlo-transform-sql/      parser, directives, relation extraction
├── phlo-transform-core/     discovery, semantic model, compiler, DAG
├── phlo-transform-engine/   adapter trait, planner, scheduler, execution,
│                            state, artifacts, environments, audit, promotion
├── phlo-transform-trino/    Trino HTTP adapter
├── phlo-transform-duckdb/   in-process DuckDB adapter
├── phlo-transform-nessie/   Nessie REST + in-memory clients
├── phlo-transform-openlineage/  canonical lineage graph → OpenLineage export
├── phlo-transform-dbt/      dbt project translator
├── phlo-transform-daemon/   local HTTP/JSON semantic service
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
physical target, and resolves external sources through the same mapping the
engine uses for source probes and seed loads: when a `default_catalog` is
configured, a source written with fewer than three parts is qualified to
`<default_catalog>.<schema>.<table>` (a source written as `external.feed`
compiles to `memory.external.feed` in the config above), while a source
already carrying an explicit catalog is left untouched. With no
`default_catalog` configured, source names pass through unchanged. The
rewrite mirrors relation extraction, including CTE scope, so a CTE that
shadows a workspace model name is never rewritten.

## Adapter boundary

```rust
#[async_trait]
pub trait Adapter: Send + Sync {
    fn name(&self) -> &str;
    async fn relation_exists(&self, relation: &Relation) -> Result<bool, AdapterError>;
    /// Batched existence probe; the default calls `relation_exists` per
    /// relation, adapters with a queryable information schema override it.
    async fn relations_exist(&self, relations: &[Relation]) -> Result<Vec<bool>, AdapterError>;
    async fn execute(&self, sql: &str) -> Result<QueryResult, AdapterError>;
    async fn create_or_replace_view(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError>;
    async fn create_or_replace_table(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError>;
    async fn append(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError>;
    async fn merge(&self, relation: &Relation, key_columns: &[String], sql: &str) -> Result<QueryResult, AdapterError>;
    async fn replace_partitions(&self, relation: &Relation, partition_columns: &[String], sql: &str) -> Result<QueryResult, AdapterError>;
    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError>;
    /// A per-attempt view that tracks the queries started through it, so
    /// the runner can cancel this attempt's in-flight warehouse work on
    /// timeout or shutdown. `None` (default) = cannot report in-flight ids.
    fn track_attempt(&self) -> Option<Arc<dyn Adapter>> { None }
    /// Query ids currently executing through a tracked view (default empty).
    fn in_flight_queries(&self) -> Vec<String> { Vec::new() }
    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError>;
    /// Batched column listing; the default calls `relation_columns` per
    /// relation. An absent relation yields `Err` — never a silent empty
    /// schema, which would misclassify every column as added.
    async fn relation_columns_many(&self, relations: &[Relation])
        -> Vec<Result<Vec<ColumnInfo>, AdapterError>>;
    /// Whether `ensure_catalog` can provision a catalog bound to a Nessie
    /// ref (default `false`) — read-only environment resolution fails a
    /// preview whose generated catalog the adapter could never create.
    fn supports_catalog_provisioning(&self) -> bool;
    async fn ensure_catalog(&self, request: &CatalogRequest) -> Result<CatalogStatus, AdapterError>;
    /// Drop a catalog the caller proved phlo owns; quoting is the
    /// adapter's business (default reports UNSUPPORTED).
    async fn drop_catalog(&self, catalog: &str) -> Result<(), AdapterError>;
    async fn ensure_schema(&self, relation: &Relation) -> Result<(), AdapterError>;
    async fn source_state(&self, relation: &Relation) -> Result<Option<String>, AdapterError>;
    /// A content-independent identity for the materialised output (e.g. the
    /// Iceberg snapshot id) — the evidence cache reuse is gated on.
    async fn output_identity(&self, relation: &Relation) -> Result<Option<String>, AdapterError>;
    async fn partition_counts(&self, relation: &Relation, partition_columns: &[String]) -> Result<Option<Vec<(String, i64)>>, AdapterError>;
    async fn load_csv(&self, relation: &Relation, path: &Path) -> Result<QueryResult, AdapterError>;
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
- `skip` — this environment's own materialisation record vouches for the
  desired version at this target;
- `cached` — no local record vouches, but the identical version is
  recorded in another environment and this environment's target provably
  already holds the same physical output — the run adopts it (see
  *Cache adoption* below);
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

`Runner::apply` consumes a plan — it never re-decides *what* should run — and
schedules models with Tokio `JoinSet`s bounded by a `Semaphore`
(`--jobs`, default 4). Scheduling is indegree-based: each model becomes
runnable the moment every required upstream model and seed reaches a
satisfied terminal state, and independent branches proceed concurrently.
Seeds run before models; a failed seed blocks its consumers like any other
dependency.

States:

```text
pending → ready → running → passed | failed
                          → skipped | cached   (satisfied without executing)
                          → blocked            (a required upstream failed)
                          → cancelled          (fail-fast or external cancel)
```

`skipped`/`cached` satisfy dependents; `failed`/`blocked`/`cancelled` do
not. Dependents of a failed node are `blocked` transitively — a blocked
model is not reported as a SQL failure. Models that would write the same
physical relation are serialised on a per-target lock, so a skipped parent
and a rebuilt child can never race the same table.

### Cache adoption

`cached` is executable, not just a label. The plan's evidence is the
source record (`reuse`: source environment and target, the output
identity, the producing run id and materialisation timestamp), and the
runner re-reads the target's live `output_identity` before adopting —
evidence can go stale between plan and run, so an identity that no longer
matches (or cannot be read) flips the action to a full `build` with a
`cache_miss` reason rather than claiming unverified output.

A successful adoption writes an environment-local materialisation record
that preserves the source's `run_id` and `materialized_at` — the run
reused an existing output; it did not produce a new one — and copies the
source environment's time-window watermark, since identical content has
the same frontier. Subsequent plans against this environment then resolve
through its own record — `skip`, not another `cached` hop. This is the
Nessie fast path: a candidate branch inherits the base's Iceberg tables,
so a `run --ref` on unchanged content adopts every output and issues no
model SQL at all.

### Failures and retries

Every failure is a structured `Failure`: a stable `category` (`adapter`,
`sql`, `test`, `timeout`, `cancelled`, `dependency`, `state`, `internal`),
a message, the adapter error code/message when applicable, the attempt
number and a `retryable` flag. Every attempt is recorded (`Attempt`:
number, timing, query id, failure) so run history shows each try.

`RetryPolicy` centralises retry decisions (`--retries`, default 0 extra
attempts; bounded exponential backoff from 200 ms doubling to a 5 s
ceiling). Only adapter failures the adapter itself marked `retryable` are
retried — SQL-semantic error codes (`SYNTAX_ERROR`, `COLUMN_NOT_FOUND`,
`TABLE_NOT_FOUND`, `TYPE_MISMATCH`, …) fail once, and test, timeout,
dependency and cancellation failures are never retried. Compilation and
planning errors are not execution retries.

### Fail-fast, timeouts, cancellation

`--fail-fast` changes shutdown only: on the first unrecoverable failure the
scheduler stops dispatching new work, aborts in-flight tasks, marks
dependents of the failure `blocked` and unrelated not-started work
`cancelled`. Without it, independent branches run to completion.

`--model-timeout 30s|5m|1h` bounds each attempt; expiry is a `timeout`
failure. Each attempt runs through a *tracked* adapter view
(`track_attempt`/`in_flight_queries`): adapters that report in-flight
query ids get real warehouse cancellation — the engine calls
`Adapter::cancel` for every query the attempt still has running, both on
timeout and when fail-fast/cancellation aborts in-flight tasks. The Trino
adapter tracks statements by the id Trino assigns at `POST /v1/statement`
and kills them with `DELETE /v1/query/{id}`; adapters that cannot report
in-flight ids degrade to dropping the attempt's future. Cancellation is
the same cooperative path as Ctrl-C: a `CancelHandle` stops scheduling,
aborts tasks, cancels tracked queries and reports the run as `cancelled`.
Adapters that cannot cancel still surface the real outcome — the engine
never claims a model was cancelled when it actually completed.

### Seeds and tests

Seeds load before models and follow the same failure/retry policy; a failed
seed blocks its consumers. Tests run after models, only when every dataset
they read is available; a test fails when its query errors or returns rows.
A failed test fails the run (`test` category) without rewriting the model's
own execution status.

### Resume and retry-failed

`--resume <run-id>` continues an **interrupted** run — one still `running`
after a killed process, or `cancelled` — **under the same run id**: the
stored plan is reloaded, genuinely passed work is reused (a model's
earlier `passed` counts only when its desired version still matches *and*
its target relation still exists; a seed's earlier `passed` counts only
when its content hash still matches and its target exists), and everything
else is re-planned against current state — a stored `skip`/`cached`
decision is never trusted once the version underneath it moved. Resume
refuses with an explicit explanation when the workspace changed
incompatibly (a stored model or seed no longer exists), and refuses a
finished run outright: a `failed` run is continued with `--retry-failed`
so its history stays immutable.

`--retry-failed <run-id>` starts a **new** run (`continued_from` links it)
covering the failed/blocked/cancelled models of a finished run, plus the
upstream work they still need, plus any tests that failed — tests whose
datasets rebuilt re-verify too. Everything else is skipped. Neither
accepts selection flags — the prior run defines the work.

### Run summary

`RunResult` is the machine-readable summary (`run_id`, `plan_id`,
`continued_from`, `status`, `counts`, per-model/seed/test results with
`failure`, `attempts`, `query_id`, timings) — `run.json` and `--json` both
serialise it. Human output derives from the same value: status counts, a
FAILED/BLOCKED breakdown naming categories, and the suggested
`--retry-failed`/`--resume` commands. Results are reported in plan order
regardless of execution order.

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
records runs, per-model/seed/test executions and materialised model
versions (including `version_detail` — the named dependency/source inputs
behind each version hash) at `.phlo/transform/state.db`. Runs are persisted
incrementally: `start_run` stores the plan with the run record before
execution begins, and every model/seed/test result is written as it lands —
a killed process leaves an accurate partial record, which is what
`--resume`/`--retry-failed` rebuild from. Only `passed` builds update the
materialised-version table. See [`docs/state.md`](state.md).

## Artifacts

Written under `.phlo/transform/` with `schema_version = 3`:

| File | Contents |
|---|---|
| `manifest.json` | workspace root, roots, models, sources, tests |
| `graph.json` | dependency-graph nodes and edges |
| `lineage.json` | the canonical lineage graph document (models, datasets, columns, tests; direct/indirect/transformation/confidence on column edges) |
| `openlineage.json` | the same graph exported as an OpenLineage design-time document (a JSON array of valid `JobEvent`/`DatasetEvent`s) |
| `plan.json` | plan id, adapter, environment, planned models/tests, diagnostics |
| `run.json` | run id, plan id, `continued_from`, status, per-status counts, model/seed/test results with structured failures and attempts, events |
| `environment.json` | provisioned base/candidate references and candidate catalog |
| `diff.json` | data/schema diff report (strategy, rows, columns, partitions, policies) |
| `lineage_diff.json` | semantic lineage diff vs a Git baseline (nodes/edges added, removed, changed; orphaned consumers and affected downstream paths) bound to `base_kind`/`base_commit`, the candidate's git head + worktree state, and the Nessie pair when resolved |
| `promotion.json` | promotion id, references/hashes, gates, conflicts, timestamp |

`schema_version` makes the interface explicit and versionable. The
promotion-evidence artifacts (`environment*.json`, `branch_diff.json`,
`lineage_diff.json`) are exports: their authority is the immutable
`evidence` records in the state store (see `docs/state.md` §Audit
evidence), which is what `promote` reads — including across machines on a
shared backend.

## Observability

The engine emits structured `EngineEvent`s (`compile_started`,
`plan_created`, `run_started`, `seed_started`, `seed_retrying`,
`seed_finished`, `model_queued`, `model_started`, `model_retrying`,
`model_finished`, `test_started`, `test_finished`, `run_finished`). Human
CLI output, `run.json` and state persistence all derive from the same
stream — terminal output is never the execution API.

## Testing strategy

- Planner, scheduler, blocking, concurrency, state and artifacts are tested
  with a fake adapter (no warehouse required).
- The Trino adapter is tested end-to-end against a disposable Trino container
  (testcontainers) in `crates/phlo-transform-trino/tests/trino_e2e.rs`. It is
  `#[ignore]`d by default and run explicitly in CI on Docker-enabled runners.

## Beyond this document

Later-phase features documented elsewhere:

- contracts and schema-change classification — [`state.md`](state.md);
- content-addressed versions and cache reuse — [`state.md`](state.md);
- incremental materialisations — [`incremental.md`](incremental.md);
- Nessie environments, WAP promotion and catalog ownership —
  [`wap.md`](wap.md);
- data diff — [`diff.md`](diff.md);
- the HTTP/JSON service — [`daemon.md`](daemon.md).
