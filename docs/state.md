# Phase 3 state-aware execution

This document describes content-addressed model versions and state-aware
planning. It complements [`docs/engine.md`](engine.md).

## Model versions

Every compiled model carries a `ModelVersion` with separately inspectable
component hashes:

```text
hash = H(
  sql_semantic_hash
  + config_hash
  + contract_hash
  + dependency_version_hash
  + source_state_hash
  + compiler_semantics_version
  + target_semantics_hash
)
```

- `sql_hash` hashes the canonical serialised AST, so formatting and comments do
  not change it.
- `config_hash` covers semantics-affecting configuration (materialisation,
  target schema) but **not** owner/tags.
- `contract_hash` covers contracts and assertions.
- `dependency_hash` folds in upstream model versions, so an upstream change
  invalidates downstream desired versions even when downstream SQL is
  unchanged.
- `source_state_hash` folds in `SourceStateProvider` values for external
  sources.
- `compiler_semantics_version` is an explicit constant bumped only when
  compilation/materialisation semantics change.
- `target_hash` covers the physical target and materialisation.

`phlo-transform-core` exposes `compile_with_options(project, schemas,
source_state)`; `compile` and `compile_with_provider` use empty providers.

Source states are populated from the warehouse: `Adapter::source_state` returns
the latest Iceberg `snapshot_id` for a relation (via the `$snapshots` metadata
table), falling back to a stable schema fingerprint for non-Iceberg relations,
and `collect_source_states` lowers observed states into a provider before
compilation. `inspect`, `lineage`, `impact` and `plan`/`apply`/`run`
enrich compilation this way when a target is configured (or with
`--catalogue`), so an Iceberg source commit invalidates dependent model
versions and produces a `source_change` build reason.

## Cached reuse

`Planner` classifies a model as `cached` when its desired version is not
materialised in the current environment but exists for another environment in
the same state store. This is covered by
`cache_reuse_across_environments_is_reported_as_cached`.

## Materialised state

The SQLite state store gains a `model_versions` table recording, per model and
environment, the `ModelVersion` attached to the most recent successful
materialisation (plus target, run id and timestamp). API:

```text
record_materialized(record)
materialized_version(model_id, environment)
materialized_by_hash(version_hash)
```

The runner records a materialisation after each `passed` build.

## State-aware planning

`Planner` now takes an optional `StateStore`. For each model it decides:

- `build` — missing relation, unknown state, or any component changed;
- `skip` — the desired hash already matches the materialised version here;
- `cached` — the desired hash is materialised in another environment.

Each `build` carries structured `ChangeReason`s (`sql_semantic_change`,
`config_change`, `contract_change`, `dependency_change`, `source_change`,
`target_change`, `compiler_semantics_change`, `missing_relation`,
`unknown_state`). `plan.json` exposes the desired/current hashes and reasons.

## Stale plans

A plan records the desired version of every planned model. At apply time the
runner recomputes against the freshly compiled workspace and rejects a plan
whose desired versions no longer match (`EngineError::StalePlan`).

## Inspect

`inspect` shows desired/current versions and `status` (`new`/`changed`/
`unchanged`); JSON exposes the component hashes and state.

## Tests

- canonical hash equivalence (formatting/comments);
- SQL, config, materialisation and upstream-change invalidation;
- owner/tag changes do not rebuild;
- first run builds, unchanged second run skips (fake adapter + SQLite);
- stale plan rejection.

## Deferred

Cross-environment cache reuse (beyond the `cached` classification), advanced
SQL equivalence, incremental strategies, Nessie promotion and data diff.
