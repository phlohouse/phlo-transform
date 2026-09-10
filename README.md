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

## Status

- **Phase 0 — compiler spike: done.** Multi-root discovery, stable IDs,
  ordinary-SQL dependency resolution without `ref()`, deterministic DAG,
  `check`/`list`/`inspect` with JSON. See [`docs/architecture.md`](docs/architecture.md).
- **Phase 1 — MVP build engine: done.** Trino adapter, view/table
  materialisations, `plan`/`apply`/`run`/`test`, bounded-concurrency scheduler,
  custom SQL tests, SQLite run history and versioned artifacts. See
  [`docs/engine.md`](docs/engine.md).

No column lineage, state-aware skipping, incremental models, Nessie, WAP, data
diff or daemon yet — those are later phases.

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

# Engine (requires a Trino target)
export PHLO_TRINO_ENDPOINT=http://localhost:8080
export PHLO_TRINO_CATALOG=memory
export PHLO_TRINO_SCHEMA=default
phlo-transform --root <workspace> plan
phlo-transform --root <workspace> apply
phlo-transform --root <workspace> run
phlo-transform --root <workspace> test

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
