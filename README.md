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

See [`docs/dbt-migration-guide.md`](docs/dbt-migration-guide.md) for a guided
walkthrough and [`docs/dbt-migration.md`](docs/dbt-migration.md) for what is translated,
the `CLEAN`/`REVIEW`/`UNSUPPORTED` classification, and the report format.

## Status

Phlo Transform is an early release (v0.1). The end-to-end workflow —
compile, plan, run, test, content-addressed state, lineage, contracts,
diffs, Nessie branch environments and audited promotion, the daemon API and
dbt translation — is implemented and exercised against live
Trino/Iceberg/Nessie, Postgres and DuckDB. Known limitations are called out
in the [release notes](docs/v0.1-release-notes.md); per-area implementation
status is tracked in [docs/roadmap/](docs/roadmap/README.md).

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
- [Blog series: what Phlo Transform is, from first principles](docs/blog/README.md)
- [Implementation roadmap](docs/roadmap/README.md)
- [v0.1 release notes](docs/v0.1-release-notes.md) and the [final release review](docs/v0.1-final-review.md)
- [Compiler architecture](docs/architecture.md)
- [Engine architecture](docs/engine.md)
- [Semantic compiler](docs/semantic.md)
- [Canonical lineage graph and OpenLineage export](docs/lineage.md)
- [State-aware execution](docs/state.md)
- [dbt migration](docs/dbt-migration.md) and the [migration guide](docs/dbt-migration-guide.md)
- [Incremental models](docs/incremental.md)
- [Nessie and WAP](docs/wap.md)
- [Native data diff](docs/diff.md)
- [Workflow integration](docs/workflow.md)
- [Daemon and agent APIs](docs/daemon.md)
