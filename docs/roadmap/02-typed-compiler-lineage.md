# Phase 2 — Typed compiler and lineage

## Objective

Promote Phlo Transform from a dependency-aware SQL runner into a semantic SQL compiler.

At the end of this phase the engine should understand model outputs, columns and common expression types well enough to catch many errors before execution and derive model/column lineage and downstream impact directly from the compiler representation.

This phase is a major product differentiator and should be treated as core architecture, not optional metadata extraction.

## Required capabilities

### Catalogue schema loading

Extend the Trino adapter to introspect external relation schemas.

Capture at least:

- column name;
- physical type;
- nullability where available;
- relation identifier;
- catalogue/schema/table context.

Catalogue metadata must enter compilation through a clear provider interface so compilation remains testable with fixtures.

### Resolved SQL representation

Introduce a semantic layer between raw SQL AST and execution SQL.

Conceptually:

```text
Parsed AST
    ↓
Resolved AST
    ↓
Typed AST
    ↓
Model schema + lineage
```

The resolved representation must identify each relation and column reference by logical source rather than textual alias.

### Column resolution

Resolve:

- qualified columns;
- unqualified columns where unambiguous;
- aliases;
- `SELECT *`;
- `table.*`;
- CTE outputs;
- subquery outputs;
- common joins;
- set operations;
- projections.

Ambiguous unqualified column references must fail at compile time.

### Type inference

Infer common SQL result types across:

- literals;
- direct column references;
- arithmetic;
- comparisons;
- boolean expressions;
- casts;
- `CASE`;
- `COALESCE`;
- common aggregates;
- common Trino scalar functions;
- simple window expressions.

Do not aim for complete Trino type-system parity in one step. Unsupported expressions may be marked unknown with explicit compiler confidence/limitations where safe.

### Output schema inference

Every compiled model should expose an inferred output schema when possible.

Example:

```text
assay.results

experiment_id   VARCHAR    NOT NULL?
sample_id       VARCHAR    ?
result          DOUBLE     ?
concentration   DOUBLE     ?
```

Unknown nullability is distinct from nullable.

### Static validation

Catch before execution where possible:

- unknown columns;
- ambiguous columns;
- obviously incompatible operators;
- invalid references after projection/CTE boundaries;
- duplicate output names where target semantics disallow them;
- invalid schema contracts;
- incompatible union branches where determinable.

### Column lineage

Derive direct and transitive lineage.

Example:

```sql
select
    r.sample_id,
    r.signal / s.volume as concentration
from assay.raw_results r
join assay.samples s using (sample_id)
```

Lineage:

```text
assay.results.sample_id
  <- assay.raw_results.sample_id

assay.results.concentration
  <- assay.raw_results.signal
  <- assay.samples.volume
```

Represent transformation expressions as metadata where useful, but do not require expression-level visualization for MVP.

### Model lineage

Use the same resolved semantic graph as execution. Do not maintain a separate lineage parser.

### `lineage` command

Implement:

```bash
phlo transform lineage assay.results
phlo transform lineage assay.results.concentration
phlo transform lineage assay.results --upstream
phlo transform lineage assay.results --downstream
```

Support JSON output.

### `impact` command

Implement:

```bash
phlo transform impact assay.results.result
```

Return at least:

- directly affected downstream columns;
- transitively affected columns;
- downstream models;
- tests associated with those models;
- workflow ownership where known.

### Schema contracts

Support explicit contracts only when users need stronger guarantees than inference.

Example:

```toml
[model.assay_results.contract]
enforced = true

[model.assay_results.columns.experiment_id]
type = "VARCHAR"
nullable = false
```

Contracts validate inferred/actual output semantics; they should not be mandatory schema duplication.

### Lightweight directives

Support:

```sql
-- @key experiment_id
-- @not-null sample_id,result
```

`@key` should imply uniqueness + non-null runtime assertions later and should be represented in the semantic model now.

### Inferred test declarations

Generate logical test definitions from contracts/directives, even if some are executed by the Phase 1 runtime test system.

Examples:

- `@key x` -> unique + not-null x;
- explicit relationship -> referential-integrity test;
- non-null contract -> runtime assertion where required.

### Schema artifacts

Extend `manifest.json` and `lineage.json` with documented versioned schemas.

Avoid serializing parser-specific AST internals as the public artifact contract.

## Compiler architecture guidance

Do not mutate the original parser AST heavily in-place. Prefer separate semantic IDs and mappings so parser upgrades do not define the public architecture.

Suggested concepts:

```rust
struct RelationId(...);
struct ColumnId(...);

struct ResolvedColumnRef {
    relation: RelationId,
    column: ColumnName,
}

struct ModelSchema {
    columns: Vec<OutputColumn>,
}

struct ColumnLineage {
    output: ColumnId,
    inputs: Vec<ColumnId>,
}
```

Use an explicit `Unknown`/`Unresolved` type state rather than fake defaults.

## Handling unsupported SQL

The compiler should distinguish:

```text
ERROR      definitely invalid
WARNING    potentially unsafe/unknown
UNKNOWN    semantic inference unavailable but execution may proceed
```

Whether unknown semantics block execution should be configurable only at clear policy boundaries, not per-expression configuration sprawl.

## `SELECT *`

`SELECT *` must expand against known input schemas when possible so lineage and downstream schema impact remain accurate.

If source schema is unavailable, preserve an unresolved wildcard representation and communicate the limitation.

## Tests

### Unit

Cover:

- column resolution;
- alias scopes;
- nested CTEs;
- subqueries;
- `SELECT *` expansion;
- arithmetic type inference;
- CASE/coalesce inference;
- joins with duplicate names;
- aggregate lineage;
- union lineage.

### Golden compiler fixtures

For each SQL fixture, snapshot:

- resolved relations;
- inferred output schema;
- column lineage;
- diagnostics.

### Integration

Compare inferred schema against actual Trino-created relation schemas for a representative SQL suite.

## Acceptance criteria

Phase 2 is complete when:

1. the compiler can infer output columns for common Trino transformation SQL;
2. unknown and ambiguous column references are caught before warehouse execution;
3. model lineage and column lineage derive from the same compiler representation;
4. `SELECT *` expands correctly when source schemas are known;
5. `lineage` and `impact` work in human and JSON form;
6. explicit schema contracts can validate model output;
7. `@key` and `@not-null` are represented semantically and generate logical assertions;
8. compiler limitations are explicit rather than silently guessed;
9. integration tests compare inferred and real Trino schemas for representative models.

## Explicitly deferred

- full Trino SQL/type coverage;
- SQL optimizer/rewrite engine;
- automatic code refactoring;
- content-addressed state invalidation;
- incremental materialisation;
- Nessie;
- data diff;
- daemon.

## Implementation notes

Phase 2 is implemented. See [`docs/semantic.md`](../semantic.md).

Implemented:

- typed semantic IR with explicit `Unknown` type/nullability states;
- `SchemaProvider` boundary (empty + static providers; Trino adapter exposes
  `relation_columns` and the CLI enriches compilation from the catalogue);
- column resolution (qualified/unqualified, aliases, `USING`, `SELECT *` and
  `alias.*`, CTEs, subqueries, set operations);
- type inference for common expressions and functions, with limitations
  recorded rather than guessed;
- inferred output schema on every compiled model, with `known` tracking so
  unknown source schemas do not cause false errors;
- `lineage` and `impact` commands (human + JSON);
- config-file contracts (`[model.…contract]`) and `@key` / `@not-null`
  directives, both represented as assertions;
- generated SQL tests from assertions, executed by the Phase 1 runtime;
- `lineage.json` artifact;
- integration test comparing inferred and real Trino schemas.

Remaining (explicit): full Trino type-system parity and broader SQL coverage;
richer assertion types.


