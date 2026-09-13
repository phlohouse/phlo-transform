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
materialisation (plus target, run id, timestamp, incremental strategy/key and
`version_detail`). API:

```text
record_materialized(record)
materialized_version(model_id, environment)
materialized_by_hash(version_hash)
```

The runner records a materialisation after each `passed` build.

`version_detail` is a `VersionDetail`: the named inputs behind the opaque
component hashes — each dependency's logical name → version hash and each
source's logical name → observed state. It exists so a changed hash can be
*explained* (`upstream version changed: assay.raw`, `source raw.lims changed
(snap:… → snap:…)`) rather than just detected. Rows recorded before detail
was tracked simply yield generic dependency/source reasons.

## State-aware planning

`Planner` now takes an optional `StateStore`. For each model it decides:

- `build` — missing relation, unknown state, any component changed, or
  `--force`;
- `skip` — the desired hash already matches the materialised version here;
- `cached` — the desired hash is materialised in another environment.

Every decided model carries structured `PlanReason`s (stable `kind` codes
such as `sql_semantic_change`, `dependency_change`, `source_change`,
`missing_relation`, `unchanged`, `forced`, plus membership reasons
`selected_dependency`/`selection_expansion`) with a human-readable `detail`
and an optional `subject` naming the input that moved. `plan.json` exposes
the desired/current hashes, reasons, membership and the resolved selection.

`changed_models(compilation, state, environment)` returns the set of models
whose desired version differs from the recorded one — the state-derived
change set behind the `changed` selector term. Ephemeral models are never
reported (they are never materialised, so they have no recorded version;
their edits propagate to dependents through the dependency hash instead).

The same `changed` term accepts a second provider: `--since <ref>` feeds
`phlo-transform-core::git`'s diff-derived change set instead of recorded
state. The two answer different questions — "what differs from what's
materialised" vs. "what changed relative to a Git ref" — and differ in
shape: the Git set is *direct* changes only (downstream propagation comes
from `changed+`, not the diff) and includes ephemeral models when their
files changed. See `docs/engine.md` §Selectors.

## Stale plans

A plan records the desired version of every planned model. At apply time the
runner recomputes against the freshly compiled workspace and rejects a plan
whose desired versions no longer match (`EngineError::StalePlan`).

## Run progress

Runs are persisted while they execute, not just at the end. `start_run`
writes the run row — id, plan id, environment, started timestamp — together
with the stored `plan` itself *before* the scheduler dispatches anything,
and each model/seed/test record is written as it reaches a terminal status
(`model_runs`, `seed_runs`, `test_runs`, plus `model_attempts` for every
retry). A process killed mid-run therefore leaves an accurate partial
record: passed work is marked passed, everything else stays unfinished.

Each execution record carries the fields needed to reconstruct what
happened: status (`passed`/`failed`/`skipped`/`cached`/`blocked`/
`cancelled`), attempt count, per-attempt failure detail (`category`,
message, adapter error code, retryable flag), timestamps, desired version
and query id. `finish_run` stamps the run's final status and failed count.

This is what `--resume` and `--retry-failed` rebuild from:

- **`run --resume <run-id>`** reloads the stored plan and the prior per-node
  records, then reruns everything that did not reach `passed`/`cached`.
  Reuse is verified, not trusted: a previously-passed model is kept only if
  its desired version still matches the freshly compiled one, and a seed is
  reused only when its recorded hash matches the current file. If the
  workspace changed incompatibly — a stored model is gone, or a passed
  model's desired version moved — resume refuses with an explicit error
  rather than guessing. The run keeps the same run id.
- **`run --retry-failed <run-id>`** creates a new run (`continued_from`
  links back) whose plan contains only the failed/blocked portion of the
  finished run plus the dependencies it still needs; the rest is skipped.
  If nothing failed there is nothing to retry and the command says so.

Safe to reuse: `passed` model records whose desired version still matches,
`cached`/`skipped` plan decisions, and `passed` seed records with matching
hashes. Never trusted: `failed`/`blocked`/`cancelled` records, unfinished
records from a killed run, and any record whose desired version differs
from the current compile.

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
