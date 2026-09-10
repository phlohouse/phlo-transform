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

Phase 0 (compiler spike) is implemented. The compiler discovers multi-root
workspaces, parses ordinary SQL, resolves relations without `ref()`, and builds
a deterministic dependency DAG. It does **not** execute anything yet.

See [`docs/architecture.md`](docs/architecture.md) for the implementation.

## Toolchain

The toolchain is pinned with [mise](https://mise.jdx.dev/) in
[`.mise.toml`](.mise.toml) (Rust 1.93 with `rustfmt` and `clippy`):

```bash
mise install
mise exec -- rustc --version
```

## CLI

```bash
cargo run -p phlo-transform-cli -- --root <workspace> check
cargo run -p phlo-transform-cli -- --root <workspace> list
cargo run -p phlo-transform-cli -- --root <workspace> inspect assay.results
cargo run -p phlo-transform-cli -- --root <workspace> --json check
```

`--json` is available on every command and exposes the same information as the
human output. `check` exits non-zero when the workspace has errors.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Fixture workspaces live in [`fixtures/`](fixtures) and are exercised by the
integration and snapshot tests.

## Documentation

- [Full specification](SPEC.md)
- [Implementation roadmap](docs/roadmap/README.md)
- [Phase 0 compiler architecture](docs/architecture.md)
