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
9. all behaviour is covered by CI tests.

## Explicitly deferred

- warehouse writes;
- live Trino catalogue introspection beyond an optional exploratory spike;
- types/column lineage;
- materialisation;
- state/version hashing;
- incremental execution;
- Nessie;
- data diff;
- daemon.
