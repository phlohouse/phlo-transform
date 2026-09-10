# Phase 0 compiler architecture

This document describes the compiler spike that actually exists in the
repository. It is the implementation companion to
[`docs/roadmap/00-compiler-spike.md`](roadmap/00-compiler-spike.md); where the
implementation made a concrete decision that the roadmap left open, that
decision is recorded here.

Phase 0 proves one claim: a Rust compiler can discover transforms across a
multi-root workspace, parse ordinary SQL, resolve workspace relations without
`ref()`, and produce one deterministic dependency DAG.

## Crate layout

```text
crates/
├── phlo-transform-sql/    SQL parsing, directives and relation extraction
├── phlo-transform-core/   discovery, identity, resolution, DAG, diagnostics, reports
└── phlo-transform-cli/    the `phlo-transform` binary
```

Crates are intentionally coarse. There is no executor, adapter, state or
daemon crate yet; those are later phases and adding them now would be
speculative.

Dependency direction is one-way: `cli → core → sql`. The SQL crate knows
nothing about workspaces, namespaces or model identity.

## Frontend → semantic → compiler boundary

The native filesystem frontend and the compiler are separated by a small,
frontend-agnostic semantic representation:

```text
native Phlo files ── discovery (native frontend) ──┐
                                                   ├──> SemanticProject ──> compile() ──> Compilation
future importer ──── lowering (future frontend) ───┘
```

- `SemanticProject` / `SemanticModel` (in `core::model`) contain only logical
  identity, SQL text, directives, optional transform-root context and a
  generic `ModelOrigin`. They contain no dbt, Jinja or YAML concepts.
- `compile(&SemanticProject)` performs parsing, resolution and graph
  construction. It never touches the filesystem.
- `SemanticModel::in_memory` and `SemanticProject::in_memory` are the factory
  path used by the architecture test, proving the resolver and DAG are not
  coupled to the native frontend.

A future dbt importer only needs to lower its resources into
`SemanticProject`; it does not need to change `core`.

## Discovery and namespace derivation

Default discovery globs are `transforms/**` and
`workflows/*/transforms/**`, plus any includes from `phlo.toml`. Default
excludes cover `target`, `.phlo`, `.git` and `node_modules`.

Namespaces are derived from the path, never guessed:

| Path | Namespace | Logical path |
|---|---|---|
| `workflows/<name>/transforms/<rest>.sql` | `<name>` | `<rest>` |
| `transforms/<ns>/<rest>.sql` | `<ns>` | `<rest>` |
| `transforms/<file>.sql` | `phlo.toml` `default_namespace` or `default` | `<file>` |
| configured `domains/<ns>/transforms/<rest>.sql` | `<ns>` | `<rest>` |

A `transform.toml` at the namespace directory may override the namespace. A
configured root without a `transforms` segment falls back to the nearest
ancestor containing a `transform.toml`.

## Model identity

`ModelId` is a namespace plus a namespace-relative path.

- display form: `assay.staging.raw`
- canonical URI: `model://assay/staging/raw`

A model may pin its identity with `-- @id assay.raw`. Pinned identity wins over
the derived one, so a file can move without losing its logical identity.
Identifier segments are restricted to `[A-Za-z0-9_-]+` so that dotted display
names can always be parsed back unambiguously.

## SQL parsing and relation extraction

Parsing uses `sqlparser-rs` 0.62 with a permissive dialect (Trino currently
maps to `GenericDialect`; DuckDB and PostgreSQL dialects are wired but unused
by fixtures).

Relations are extracted by walking the real AST, not by searching text. The
visitor:

- records `FROM`/`JOIN` table factors (including nested queries);
- ignores table aliases and column aliases, which cannot create false edges;
- skips table-valued functions such as `unnest(...)`;
- tracks CTE scope per query, so a CTE name — even one that shadows a workspace
  model — is never mistaken for a relation.

`sqlparser` 0.62 parse errors do not carry line/column spans, so parse
diagnostics point at the model file rather than an exact offset. Directive
diagnostics do include their 1-based line number.

## Reference resolution

Resolution runs in deterministic precedence order and never guesses:

1. exact fully-qualified workspace model (`assay.raw`);
2. current namespace (`raw` inside `assay`);
3. current transform root (root-relative path);
4. globally unique workspace model by suffix (`results`);
5. external source candidate.

If more than one model matches at any level the result is an ambiguity and
`compile` emits `RESOLUTION001` listing every candidate. Unresolved relations
are registered as external `SourceId`s (`source://external/raw_assay_results`);
no live catalogue introspection happens in Phase 0.

## Graph and determinism

`Compilation` builds a `petgraph` directed graph whose edges point from a model
to each relation it depends on. Given the same input, output is deterministic:

- files, models, nodes and edges are sorted;
- topological order uses Kahn's algorithm with a `BTreeSet` ready queue, so
  dependencies always come before dependents and ties break by identity;
- cycles are detected and reported with a concrete path
  (`assay.a -> assay.b -> assay.c -> assay.a`) via a coloured DFS.

## Diagnostics

Diagnostics are typed, serialisable and carry stable codes. Phase 0 uses:

| Code | Severity | Meaning |
|---|---|---|
| `PROJECT001` | error | workspace root not found |
| `PROJECT002` | error | duplicate model id |
| `PROJECT003` | error | duplicate transform namespace |
| `PROJECT004` | error | invalid pinned model id |
| `PROJECT005` | error | path cannot be mapped to a transform root |
| `PROJECT006` | error | model file could not be read |
| `CONFIG001` | error | invalid `phlo.toml` / `transform.toml` / glob |
| `PARSE001` | error | SQL parse failure |
| `PARSE002` | error | malformed directive |
| `PARSE003` | warning | unknown directive |
| `RESOLUTION001` | error | ambiguous relation |
| `GRAPH001` | error | dependency cycle |

Directive handling is deliberately small: `@id` is the only meaningful
directive. Unknown directives are warnings so that future directives do not
break older compilers, while a malformed or duplicate `@id` is an error.

## CLI

The binary is `phlo-transform` (the `phlo` host does not exist yet, so
`phlo transform ...` is not available). It supports:

```bash
phlo-transform --root <workspace> check            # compile, report diagnostics
phlo-transform --root <workspace> list             # models and sources
phlo-transform --root <workspace> inspect <model>  # one model's context
phlo-transform --root <workspace> --json <command> # structured output
```

`--json` is global and emits the same information as the human output. `check`
exits non-zero when any error-severity diagnostic exists.

## Tests

- Unit tests: directives, relation extraction, identity, resolution, graph
  ordering and cycle paths.
- Integration tests (`crates/phlo-transform-core/tests/compilation.rs`) over
  fixture workspaces covering multi-root discovery, global-only roots,
  configured roots and excludes, cross-workflow references, pinned identity,
  CTE shadowing, aliases, nested queries, ambiguity, self-reference, two- and
  three-node cycles, invalid SQL, malformed directives, unknown directives,
  duplicate ids and duplicate namespaces.
- Snapshot tests (`tests/snapshots.rs`, `crates/phlo-transform-cli/tests/cli.rs`)
  for JSON reports, diagnostics and human CLI output.
- An architecture test compiles an in-memory `SemanticProject` with no
  filesystem discovery.
- A Trino syntax suite (`crates/phlo-transform-sql/tests/trino.rs` plus the
  `trino-syntax` fixture) proves the generic dialect parses the Trino
  constructs we expect (CTEs, `UNNEST ... WITH ORDINALITY`, `LATERAL`,
  `QUALIFY`, `TABLESAMPLE`, `GROUPING SETS`/`CUBE`/`ROLLUP`, `FILTER`,
  `TRY_CAST`, `ROW`/`MAP`/`ARRAY`, intervals and more) and that relation
  extraction still behaves.

CI runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` via
`.github/workflows/ci.yml`, using the mise-pinned toolchain.

## Deferred

Execution, materialisations, state/version hashing, incremental models,
catalogue introspection, column lineage, Nessie, WAP, data diff, daemon and the
dbt importer are all out of scope for Phase 0, as the roadmap specifies.
