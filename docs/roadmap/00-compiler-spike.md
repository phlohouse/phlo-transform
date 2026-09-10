# Phase 0 — Compiler spike

## Objective

Prove the core architectural claim: a Rust compiler can discover transforms across a workspace, parse ordinary SQL, resolve workspace relations without requiring `ref()`, and produce one deterministic dependency DAG.

This phase must remain intentionally small. Do not build execution, state caching, Nessie integration, incremental models or a generalized plugin system yet.

## Required capabilities

### Workspace discovery

Discover by convention:

```text
/transforms/**
/workflows/*/transforms/**
```

Support optional additional includes/excludes from `phlo.toml`.

Recognize transform roots and derive namespaces from paths. A local `transform.toml` may override the namespace.

### Model identity

Derive a deterministic logical model ID from namespace + relative model path.

Example:

```text
workflows/assay/transforms/staging/raw.sql
```

becomes:

```text
model://assay/staging/raw
```

and user-facing:

```text
assay.staging.raw
```

Support optional pinned identity:

```sql
-- @id assay.raw
```

### SQL parsing

Use `sqlparser-rs` as the initial parser unless implementation evidence shows it cannot support the required Trino grammar.

Parse each model into an AST. Preserve useful source spans for diagnostics.

### Frontend-agnostic semantic boundary

Native Phlo file discovery/parsing must feed a semantic model that is not intrinsically tied to the filesystem frontend.

The immediate implementation remains native-Phlo-only, but the boundary should allow a later importer such as [`dbt-migration.md`](dbt-migration.md) to produce the same semantic inputs without teaching the compiler core about dbt.

Conceptually:

```text
native Phlo files ── native frontend ──┐
                                      │
future dbt project ─── dbt frontend ──┼──> semantic project/model representation ──> resolver/DAG/compiler
                                      │
future importer ───── other frontend ─┘
```

Do **not** build a generalized plugin system or dbt importer in Phase 0. The requirement is only to avoid making core types depend on assumptions such as "every model originated as a native `.sql` file discovered from a Phlo transform root" where that assumption is not semantically necessary.

Core semantic types must not contain dbt-specific fields or Jinja-specific concepts.

### Relation extraction

Extract relation references from parsed SQL, including:

- `FROM`;
- `JOIN`;
- CTE handling;
- nested queries;
- aliases;
- schema-qualified names.

CTE names must not be mistaken for workspace relations.

### Reference resolution

Resolve relation names deterministically against the model registry.

Rules should begin with:

1. explicit fully qualified workspace model;
2. current namespace;
3. current transform root where applicable;
4. globally unique workspace model;
5. unresolved external relation placeholder.

Ambiguous references are compile errors.

At this stage unresolved external relations do not need live catalogue introspection; they may be represented as external/source candidates.

### DAG construction

Build a directed acyclic graph of model dependencies.

Detect and report cycles with the concrete cycle path.

Example:

```text
error[G001]: transformation cycle detected

assay.a -> assay.b -> assay.c -> assay.a
```

### Initial CLI

Implement:

```bash
phlo transform check
phlo transform list
phlo transform inspect <model>
```

Every command must also support structured JSON output.

## Example fixture

```text
workflows/
├── assay/
│   └── transforms/
│       ├── raw.sql
│       └── results.sql
│
└── reporting/
    └── transforms/
        └── monthly.sql
```

`raw.sql`:

```sql
select * from external.raw_assay_results
```

`results.sql`:

```sql
select * from assay.raw
```

`monthly.sql`:

```sql
select * from assay.results
```

Expected graph:

```text
external.raw_assay_results
          ↓
      assay.raw
          ↓
    assay.results
          ↓
 reporting.monthly
```

## Error cases to cover

- duplicate model IDs;
- duplicate explicit IDs;
- malformed directives;
- invalid SQL;
- ambiguous short model names;
- unresolved names represented correctly;
- self-reference;
- two-node cycle;
- multi-node cycle;
- CTE shadowing a workspace model name;
- aliases not creating false dependencies;
- duplicate transform namespaces.

## Suggested internal types

```rust
struct ModelId(String);
struct Namespace(String);

struct Model {
    id: ModelId,
    path: PathBuf,
    namespace: Namespace,
    sql: ParsedSql,
    dependencies: Vec<RelationRef>,
}

struct Workspace {
    roots: Vec<TransformRoot>,
    models: ModelRegistry,
    graph: TransformGraph,
}
```

Keep these types small; do not add future state/execution fields until needed.

Where practical, keep frontend/source metadata separate from the semantic model rather than letting filesystem-specific concerns define compiler APIs.

## Tests

### Unit

- path-to-namespace conversion;
- directive parsing;
- model ID derivation;
- relation extraction;
- resolution precedence;
- ambiguity detection.

### Integration

Fixture repositories covering:

- global `transforms/`;
- `workflows/X/transforms/`;
- both simultaneously;
- cross-workflow references;
- custom transform roots;
- cycle detection.

### Architecture test

Add a small test/factory path that constructs the semantic project/model representation without going through filesystem discovery, then runs the same resolver/DAG logic.

This does not need to represent dbt. It simply proves that the compiler core is not inseparable from the native file frontend.

### Snapshot tests

Useful for:

- diagnostics;
- `inspect` output;
- JSON manifest output.

## Deliverables

- Rust workspace and CLI binary;
- workspace discovery implementation;
- SQL parser wrapper;
- model registry;
- relation resolver;
- DAG builder;
- frontend-to-semantic-model boundary;
- typed diagnostic framework foundation;
- `check`, `list`, `inspect`;
- fixture projects and integration tests;
- concise architecture notes reflecting the code that actually exists.

## Acceptance criteria

Phase 0 is complete when:

1. a repository containing several `workflows/*/transforms` roots is discovered with zero explicit root configuration;
2. ordinary SQL references between those roots create the correct graph edges;
3. no `ref()` calls are required;
4. CTEs and aliases do not create incorrect dependencies;
5. ambiguous names fail with actionable diagnostics;
6. cycles fail with an understandable cycle path;
7. `phlo transform check` exits successfully for a valid fixture and non-zero for invalid fixtures;
8. `list` and `inspect` expose equivalent semantic information in human and JSON formats;
9. resolver/DAG compilation can be exercised from an in-memory semantic project representation without filesystem discovery;
10. core semantic types contain no dbt-specific compatibility fields;
11. all behaviour is covered by CI tests.

## Explicitly deferred

- dbt translation implementation (see [`dbt-migration.md`](dbt-migration.md));
- generalized project frontend/plugin discovery;
- warehouse writes;
- live Trino catalogue introspection beyond an optional exploratory spike;
- types/column lineage;
- materialisation;
- state/version hashing;
- incremental execution;
- Nessie;
- data diff;
- daemon.

## Implementation notes

Phase 0 is implemented as described in
[`docs/architecture.md`](../architecture.md). Decisions worth calling out
against this plan:

- The binary is `phlo-transform` with `check`/`list`/`inspect` subcommands,
  because the `phlo` host that would provide `phlo transform ...` does not
  exist yet.
- Three crates were added: `phlo-transform-sql`, `phlo-transform-core` and
  `phlo-transform-cli`. No adapter/executor crates were created.
- The semantic model keeps raw SQL text and parses it in the compiler rather
  than storing a parsed AST on `SemanticModel`. This keeps the frontend
  boundary free of parser types; directives are parsed by the frontend because
  `@id` affects identity.
- Unresolved relations become external source candidates (`SourceId`), not
  errors, matching the plan.
- Unknown model directives are warnings; malformed `@id` directives are
  errors.
- `sqlparser` 0.62 parse errors do not expose line/column spans, so parse
  diagnostics reference the file only. Directive diagnostics include line
  numbers.

