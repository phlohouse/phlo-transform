# Phlo Transform

Phlo Transform is a workspace-native SQL transformation compiler and execution engine written in Rust.

It is intentionally not a dbt compatibility project. The goal is a smaller, more predictable SQL build system that treats the repository as one workspace, understands SQL structurally, and makes planning, state, lineage, testing, diffing and safe promotion first-class concepts.

## Core ideas

- SQL-first: ordinary SQL relations should create dependencies without requiring `ref()`.
- Workspace-native: transforms can live in `transforms/**`, `workflows/*/transforms/**`, and configured roots while remaining one graph.
- Compiler-driven: parse and resolve SQL into a typed semantic representation before execution.
- Minimal configuration: infer model identity, dependencies, sources, schemas and lineage where safe.
- State-aware: treat desired versus materialised model versions as a build-system problem.
- Plan before apply: explain what will change and why before mutation.
- Iceberg/Nessie-native: use versioned table state and branches for environments, WAP and promotion.
- Agent-native: expose structured compiler truth through JSON artifacts and APIs.

## Getting started

Build the CLI:

```bash
cargo install --path crates/phlo-transform-cli   # installs `phlo-transform`
# or: cargo build --release -p phlo-transform-cli
```

Scaffold and run a workspace locally — no external services required, the
bundled DuckDB adapter executes in-process:

```bash
phlo-transform init                       # creates phlo.toml, transforms/, tests/
phlo-transform check                      # compile and diagnose the workspace
phlo-transform plan --adapter duckdb      # what would change, and why
phlo-transform run --adapter duckdb       # plan + apply + tests
phlo-transform explain example.daily_events
phlo-transform doctor                     # workspace/config/adapter health
```

To target Trino instead, set `PHLO_TRINO_ENDPOINT` (plus
`PHLO_TRINO_USER`/`PHLO_TRINO_CATALOG`/`PHLO_TRINO_SCHEMA`) or pass
`--trino-endpoint` and friends; see [`docs/engine.md`](docs/engine.md).

Migrating a dbt project:

```bash
phlo-transform -r path/to/dbt-project translate --from dbt --check   # analyse
phlo-transform -r path/to/dbt-project translate --from dbt --out generated/ --verify
```

See [`docs/dbt-migration.md`](docs/dbt-migration.md) for what is translated,
the `CLEAN`/`REVIEW`/`UNSUPPORTED` classification, and the report format.

## Status

Audited against code and tests. Phases 0–1 are complete; Phases 2–8 are
implemented but **partial**, with gaps documented in each roadmap phase's
implementation notes and in [`docs/roadmap/README.md`](docs/roadmap/README.md).

- **Phase 0 — compiler spike: done.** Multi-root discovery, stable IDs,
  ordinary-SQL dependency resolution without `ref()`, deterministic DAG,
  `check`/`list`/`inspect` with JSON. See [`docs/architecture.md`](docs/architecture.md).
- **Phase 1 — MVP build engine: done.** Trino adapter, view/table
  materialisations, `plan`/`apply`/`run`/`test`, bounded-concurrency scheduler,
  custom SQL tests, SQLite run history, cancellation and versioned artifacts.
  See [`docs/engine.md`](docs/engine.md). Retries are not automated.
- **Phase 2 — typed compiler and lineage: partial.** Typed semantic IR, schema
  provider boundary and catalogue enrichment, column resolution and type
  inference, inferred output schemas, `lineage`/`impact`, config-file schema
  contracts, `@key`/`@not-null` assertions with generated SQL tests, and a
  `lineage.json` artifact. Nested `array`/`map`/`row` types parse recursively;
  `lineage` direction flags and model-level `impact` work. See
  [`docs/semantic.md`](docs/semantic.md). Gaps: offline column lineage/impact
  need schemas; unsupported SQL is `Unknown`.
- **Phase 3 — state-aware execution: partial.** Content-addressed model
  versions, materialised-version state, state-aware plan
  (`build`/`skip`/`cached` with reasons) and stale-plan rejection. Source
  states are wired through `Adapter::source_state` (Iceberg snapshots) and CLI
  enrichment; cached classification is tested. See [`docs/state.md`](docs/state.md).
  Gaps: non-Iceberg sources have no observable state; `cached` is a
  classification without cross-environment reuse execution.
- **Phase 4 — incremental models: partial.** `@incremental`
  append/key/partition/time-window intent, version hashing, full-rebuild
  detection, adapter `append`/`merge`/`replace_partitions`, typed time-window
  watermarks and planner schema-change classification. Trino `MERGE` is
  verified live on Iceberg. See [`docs/incremental.md`](docs/incremental.md).
  Gaps: incremental partition replacement is column-list based (no metadata
  pruning).
- **Phase 5 — Nessie and WAP: partial.** `NessieClient` (REST + in-memory),
  environment provisioning (`apply --ref <candidate> --from <base>` creates the
  branch and a branch-scoped Trino catalog), candidate writes isolated from
  `main`, audited `promote` with staleness/conflict checks, a diff gate and a
  breaking-schema gate, candidate `--cleanup`, `rollback`, and a promotion
  artifact. See [`docs/wap.md`](docs/wap.md). Missing: Iceberg snapshot in the
  promotion record, automatic rebase; Nessie Iceberg catalogs have no view
  support. Live Nessie + Iceberg E2E is in CI.
- **Phase 6 — native data diff: partial.** Keyed diff with per-column change
  counts, config-driven policies and numeric tolerances, real sampling,
  Iceberg `$partitions` metadata comparison (row-count fallback), populated
  schema diffs, `diff.json`, and stale-aware promotion gating. See
  [`docs/diff.md`](docs/diff.md). Gaps: no statistical distribution summaries;
  no example-value redaction.
- **Phase 7 — workflow integration: partial (transform-side).** Workflow
  ownership, a unified typed graph artifact (`model`/`source`/`quality_gate`),
  cross-workflow dependency policy, and registered consumers in impact. See
  [`docs/workflow.md`](docs/workflow.md). Missing: host workflow graph/tasks, a
  transform-group invocation API, run correlation, gating API and e2e.
- **Phase 8 — daemon and agent APIs: partial (local service).** A versioned
  local HTTP/JSON semantic service with a coherent snapshot, file watcher and
  reload. See [`docs/daemon.md`](docs/daemon.md). Missing: a plan endpoint,
  dependency-aware targeted invalidation (reload is a full recompile), the
  benchmark, and push diagnostics.
- **dbt migration: implemented.** `translate --from dbt` lowers dbt
  projects (`ref()`/`source()`/`var()`, materialisations, keys, tests, tags,
  config inheritance) into native `.sql` directives + `phlo.toml`/
  `transform.toml`, classifies every resource `CLEAN`/`REVIEW`/`UNSUPPORTED`,
  and writes a report + `.phlo/migration/dbt-translation.json` manifest. See
  [`docs/dbt-migration.md`](docs/dbt-migration.md).
- **Adapters: Trino + DuckDB.** `--adapter duckdb` runs everything locally
  with no infrastructure; `--adapter trino` targets a Trino cluster. See the
  adapter boundary in [`docs/engine.md`](docs/engine.md).

All eight numbered phases have implementations; only Phases 0–1 are complete
against their acceptance criteria.

## Toolchain

The toolchain is pinned with [mise](https://mise.jdx.dev/) in
[`.mise.toml`](.mise.toml) (Rust 1.93 with `rustfmt` and `clippy`):

```bash
mise install
mise exec -- rustc --version
```

## CLI

```bash
# Compiler
phlo-transform --root <workspace> check
phlo-transform --root <workspace> list
phlo-transform --root <workspace> inspect assay.results
phlo-transform --root <workspace> explain assay.results

# Engine — local, no infrastructure (DuckDB)
phlo-transform --root <workspace> plan --adapter duckdb
phlo-transform --root <workspace> apply --adapter duckdb
phlo-transform --root <workspace> run --adapter duckdb
phlo-transform --root <workspace> test --adapter duckdb

# Engine — Trino target
export PHLO_TRINO_ENDPOINT=http://localhost:8080
export PHLO_TRINO_CATALOG=memory
export PHLO_TRINO_SCHEMA=default
phlo-transform --root <workspace> plan

# Migration and diagnostics
phlo-transform --root <dbt-project> translate --from dbt --check
phlo-transform doctor

# Every command supports --json
phlo-transform --root <workspace> --json plan
```

Selectors: `--select assay.results`, `--select 'assay.*'`, `--upstream`,
`--downstream`, `--tag qc`, `--workflow assay`.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Live Trino integration test (requires Docker)
cargo test -p phlo-transform-trino --test trino_e2e -- --ignored
```

Fixture workspaces live in [`fixtures/`](fixtures). The Trino end-to-end test
runs a disposable container and is executed explicitly in CI.

## Documentation

- [Full specification](SPEC.md)
- [Implementation roadmap](docs/roadmap/README.md)
- [Phase 0 compiler architecture](docs/architecture.md)
- [Phase 1 engine architecture](docs/engine.md)
- [Phase 2 semantic compiler](docs/semantic.md)
- [Phase 3 state-aware execution](docs/state.md)
- [dbt migration](docs/dbt-migration.md)
- [Phase 4 incremental models](docs/incremental.md)
- [Phase 5 Nessie and WAP](docs/wap.md)
- [Phase 6 native data diff](docs/diff.md)
- [Phase 7 workflow integration](docs/workflow.md)
- [Phase 8 daemon and agent APIs](docs/daemon.md)
