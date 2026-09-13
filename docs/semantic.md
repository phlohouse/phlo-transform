# Phase 2 semantic compiler

This document describes the typed semantic layer added in Phase 2. It
complements [`docs/architecture.md`](architecture.md) (Phase 0 compiler) and
[`docs/engine.md`](engine.md) (Phase 1 engine).

## What exists

- A typed semantic IR: `DataType`, `Nullability`, `ColumnRef`, `RelationRef`,
  `OutputColumn`, `ModelSchema` and `Assertion` (`core::semantic`).
- A `SchemaProvider` boundary (`core::schema`) with an `EmptySchemaProvider`
  and a `StaticSchemaProvider`; the Trino adapter already exposes the
  column metadata needed for a catalogue-backed provider.
- A pragmatic analyzer (`core::analyze`) that resolves columns and infers
  types/lineage for common Trino transformation SQL.
- Inferred output schemas on every `CompiledModel` (`schema`, `limitations`,
  `assertions`).
- `lineage` and `impact` commands in human and JSON form.
- `@key` and `@not-null` directives represented as logical assertions.
- Integration test comparing inferred schema against a real Trino relation
  schema.

## Types and unknown states

`DataType` has an explicit `Unknown`; `Nullability` has `Unknown` distinct from
`Nullable`. Inference never invents a type: unsupported expressions produce
`Unknown` and are listed in `CompiledModel::limitations`.

`ModelSchema::known` is false when any input schema was unavailable. In that
case unresolved column references become limitations rather than errors, so
offline `check` does not fail merely because a warehouse schema is unknown.

## Column resolution

The analyzer handles:

- qualified and unqualified columns;
- table aliases and `USING` joins (using columns exposed once);
- `SELECT *` and `alias.*` expansion from known input schemas;
- CTEs and derived subqueries;
- set operations (positional merge, column-count validation);
- predicates, `GROUP BY` and `HAVING` (validated for unknown columns).

Unknown columns are `TYPE001`; ambiguous columns are `TYPE002`. Duplicate
output names are a warning (`TYPE003`); mismatched set-operation branches are
`TYPE004`.

## Type inference

Covered expressions include literals, column references, arithmetic,
comparisons and boolean operators, `CAST`/`TRY_CAST`, `CASE`, `COALESCE` and
common scalar/aggregate functions (`count`, `sum`, `avg`, `min`, `max`,
`approx_percentile`, `date_trunc`, string functions, window ranking functions,
...). Everything else is `Unknown` plus a limitation.

## Lineage and impact

Each `OutputColumn` carries `ColumnInput`s — a `ColumnRef` plus `directness`
(`direct`/`indirect`), `transformation` (identity, transformation,
aggregation, join, filter, group_by, sort, window, conditional) and the
responsible expression. Every output column also records a `confidence`:
`exact` when the AST proved the whole input set, `unknown` when part of the
query could not be analysed.

Compilation lowers those inputs into the canonical `LineageGraph`
(`compilation.lineage`) — model, dataset, column and test nodes joined by
`input`/`output`/`derives`/`contains`/`tests` edges. See
[`docs/lineage.md`](lineage.md) for the graph model and query API. Reports are
read off the graph:

- `lineage <model>` — upstream/downstream models.
- `lineage <model>.<column>` — direct inputs, indirect inputs (join keys,
  filters, grouping/sort keys) and transitive leaf columns, plus confidence
  when it is not `exact`.
- `impact <model>.<column>` — downstream columns, downstream models and the
  tests attached to those models. `impact <source-or-seed>.<column>` works
  the same way.
- `lineage --format graph` — the canonical document; `--format openlineage`
  exports it as OpenLineage.

Both commands support `--json`. The implementation reuses the same resolved
representation as execution; there is no separate lineage parser.

## Assertions

`-- @key x` implies `unique x` and `not_null x`; `-- @not-null a,b` implies
`not_null` for each. They are recorded on the compiled model and surfaced by
`inspect`. Runtime execution of these assertions is deferred to the test
runtime.

## Contracts

Explicit contracts are declared in `phlo.toml`:

```toml
[model.assay_results.contract]
enforced = true

[model.assay_results.columns.experiment_id]
type = "VARCHAR"
nullable = false
```

Model keys accept dots (`assay.results`) or underscores (`assay_results`).
Contracts validate the inferred output schema: missing columns, type
mismatches and `nullable = false` violations. Violations are errors when
`enforced = true`, warnings otherwise. A contract whose input schema is unknown
produces a "cannot be fully validated" diagnostic rather than a false failure.

## Generated runtime tests

`@key x` implies `unique x` and `not_null x`; `@not-null a,b` implies
`not_null` for each. These are represented as `Assertion`s on the compiled
model **and** lowered into generated SQL tests:

- `not_null`: `select * from <target> where "x" is null`;
- `unique`: `select "x", count(*) from <target> group by "x" having count(*) > 1`.

Generated tests appear in `list`, `plan` and `run`, and are marked `generated`
in reports.

## Catalogue-enriched compilation

The CLI compiles offline by default. `inspect`, `lineage` and `impact`
automatically enrich compilation by fetching external source schemas from the
configured Trino target (or use `--catalogue` on any command). The fetch builds
a `StaticSchemaProvider`, so the analyzer and its tests remain
warehouse-independent.

## Lineage and impact CLI

`lineage <model> --upstream` and `--downstream` filter the direction (either
may be shown; both by default). `impact` accepts a `model.column`, a
`source.column`/`seed.column` or a model name; the model form reports
downstream models/tests without requiring catalogue schemas (useful offline),
and column impact renders registered consumers.

## Remaining limitations

- Full Trino type-system parity is out of scope; `array`, `map` and `row`
  nesting is parsed recursively, but arbitrary type parameters are collapsed
  (for example `decimal(10,2)` does not retain precision).
- The analyzer covers common transformation SQL; unsupported constructs are
  `Unknown` plus recorded limitations rather than errors.
- Runtime execution of contract-derived assertions happens through the
  generated SQL tests; richer assertion types can be added later.
- Offline column lineage/impact is only as precise as known schemas; columns
  whose inputs cannot be fully resolved carry `confidence: unknown` rather
  than a partial claim presented as exact. Seed schemas come from CSV headers,
  so seed reads resolve without a catalogue.

