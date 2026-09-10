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

Each `OutputColumn` carries its direct `ColumnRef` inputs. Model schemas form a
column graph:

- `lineage <model>` — upstream/downstream models.
- `lineage <model>.<column>` — direct inputs and transitive leaf columns.
- `impact <model>.<column>` — downstream columns, downstream models and the
  tests attached to those models.

Both commands support `--json`. The implementation reuses the same resolved
representation as execution; there is no separate lineage parser.

## Assertions

`-- @key x` implies `unique x` and `not_null x`; `-- @not-null a,b` implies
`not_null` for each. They are recorded on the compiled model and surfaced by
`inspect`. Runtime execution of these assertions is deferred to the test
runtime.

## Explicit limitations

- Catalogue-enriched compilation is available through `SchemaProvider`; the
  CLI currently compiles offline, so `lineage`/`impact` are only as precise as
  known schemas. Wiring the Trino provider into the CLI is a follow-up.
- Config-file schema contracts (`[model.…contract]`) are not implemented yet;
  directive-derived assertions are.
- Full Trino type-system parity is out of scope; nested types are coarse.
- `lineage.json` artifact emission is not yet added.
