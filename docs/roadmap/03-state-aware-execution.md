# Phase 3 — State-aware execution

## Objective

Make state a first-class build-system primitive.

At the end of this phase, Phlo Transform should determine whether a model needs rebuilding by comparing a canonical desired model version with the materialised version recorded from prior successful execution.

The goal is not simply faster runs. The deeper goal is deterministic, explainable model identity and reproducibility.

## Core model

Each model has a desired immutable version derived from semantics rather than raw file text.

Conceptually:

```text
ModelVersion = hash(
  canonical SQL representation
  + effective config
  + compiler semantic version
  + dependency model versions
  + relevant source state
  + target semantics
)
```

Example:

```text
assay.results@b74a02e
```

The currently materialised model has a recorded version. Planning reconciles:

```text
desired == current  -> SKIP/CACHED

desired != current  -> BUILD
```

## Canonical SQL hashing

Do not hash raw SQL text as the primary model identity.

Formatting-only changes such as:

```sql
select * from assay.results
```

versus:

```sql
SELECT
    *
FROM assay.results
```

should not cause a rebuild once both parse to an equivalent canonical semantic representation.

Comments unrelated to directives should normally not affect the model version.

### Initial canonicalisation

A practical first implementation may hash a canonical serialized AST plus normalized semantic metadata.

Do not attempt advanced algebraic SQL equivalence in this phase.

## Version inputs

Separate version inputs by category so the planner can explain why a model changed.

Suggested fields:

```text
sql_semantic_hash
config_hash
contract_hash
dependency_version_hash
source_state_hash
compiler_semantics_version
target_semantics_hash
```

The final content hash combines them.

## Compiler version semantics

A normal binary patch release should not automatically rebuild every model.

Introduce an explicit `compiler_semantics_version` that is bumped only when compilation/materialisation semantics could alter outputs.

## Dependency versions

Downstream model versions should incorporate relevant upstream model versions.

This means a changed upstream model naturally invalidates downstream desired versions even if downstream SQL did not change.

The planner must explain this as:

```text
BUILD assay.summary
  reason: upstream model version changed
  dependency: assay.results
```

## Source versions

Introduce a `SourceState` abstraction.

For generic sources this may initially use configurable metadata such as:

- relation schema fingerprint;
- last-modified/freshness metadata where available.

For Iceberg, support richer state later/where already practical:

- snapshot ID;
- schema ID;
- partition spec ID;
- catalog reference.

Do not force all source adapters into Iceberg semantics.

## Materialisation record

Persist the model version attached to each successful physical materialisation.

Suggested record:

```rust
struct MaterializedState {
    model_id: ModelId,
    model_version: ModelVersionHash,
    relation: RelationId,
    environment: EnvironmentId,
    run_id: RunId,
    materialized_at: Timestamp,
}
```

## State store

Upgrade the Phase 1 operational state store to support:

- model versions;
- current materialised version by model/environment;
- dependency versions;
- source-state fingerprints;
- plan inputs;
- run-to-version mapping.

Keep storage behind a repository/trait boundary.

Local default should remain SQLite unless implementation evidence favours another embedded store.

## Planner changes

`phlo transform plan` must become state-aware.

Example:

```text
SKIP  assay.raw
  reason: desired version already materialised

BUILD assay.results
  reason: SQL semantics changed
  old: 21ac5d2
  new: 819bb4a

BUILD reporting.monthly
  reason: dependency version changed
  dependency: assay.results
```

The reason is part of the structured plan schema.

## Skip versus cached

Use distinct concepts:

- `SKIP`: no physical work required because the correct model version is already materialised in the target environment.
- `CACHED`: the desired version is not currently materialised there, but a reusable compatible materialisation/artifact already exists elsewhere.

Actual cross-environment cache reuse can be deferred. The distinction should exist in the state model.

## Change categories

Planner should classify changes where possible:

```text
SQL_SEMANTIC_CHANGE
CONFIG_CHANGE
CONTRACT_CHANGE
DEPENDENCY_CHANGE
SOURCE_CHANGE
TARGET_CHANGE
COMPILER_SEMANTICS_CHANGE
MISSING_RELATION
UNKNOWN_STATE
```

This enables clear explanations and later policy.

## Stale plans

Plans must record the exact state they were based on.

At apply time verify at least:

- workspace/model fingerprints unchanged;
- dependency desired versions unchanged;
- relevant source state unchanged where state was part of planning;
- environment/reference has not advanced incompatibly.

If any relevant input changed:

```text
error[P014]: plan is stale

source `raw.lims.samples` changed since plan creation
planned snapshot: 81342
current snapshot: 81391

Run `phlo transform plan` again.
```

Do not silently apply a materially stale plan.

## Rebuild propagation

Rebuild decisions should be graph-based, but avoid assuming every metadata change changes data.

Examples:

- owner/tag change: likely no physical rebuild;
- formatting/comment change: no rebuild;
- table -> view: rebuild/change physical materialisation;
- SQL AST change: rebuild;
- key/contract change: classify appropriately;
- upstream version change: rebuild unless future semantic analysis proves independence.

Keep rules conservative and explainable.

## Inspect state

Extend:

```bash
phlo transform inspect assay.results
```

with:

```text
State
  desired:  b74a02e
  current:  a621ee3
  status:   changed

Change reasons
  SQL semantic hash changed
```

JSON output must expose component fingerprints and change reasons.

## Run artifacts

Extend artifacts with:

```text
model_version
previous_model_version
change_reasons
source_states
dependency_versions
compiler_semantics_version
```

Do not embed credentials or sensitive connection metadata in hashes/artifacts.

## Reproducibility

A successful run should be explainable from persisted artifacts/state:

- which model semantic version was requested;
- which dependency versions were used;
- which source versions/snapshots were observed;
- which compiler semantics version produced the plan;
- which physical relation resulted.

## Testing

### Canonical hash tests

Prove equivalent formatting/comment changes do not change semantic hashes.

### Invalidation tests

Cover:

- direct model SQL change;
- upstream model change;
- config change;
- owner/tag-only change;
- source-state change;
- missing target relation;
- compiler semantics bump.

### State persistence tests

Run fixture twice:

1. first run builds;
2. second unchanged run skips;
3. edit upstream SQL;
4. upstream + downstream rebuild;
5. restore equivalent formatted SQL;
6. no unnecessary rebuild.

### Stale-plan tests

Create plan, mutate a relevant input, verify apply rejects it.

## Acceptance criteria

Phase 3 is complete when:

1. every successfully materialised model has a persisted model-version hash;
2. unchanged second runs skip already-correct models;
3. formatting/comment-only SQL changes do not rebuild models;
4. semantic upstream changes invalidate required downstream models;
5. planner gives structured, human-readable reasons for each build/skip decision;
6. apply rejects materially stale plans;
7. state records dependency/source/version inputs sufficiently to explain a historical run;
8. state behaviour is covered by end-to-end integration tests.

## Explicitly deferred

- advanced SQL equivalence/optimizer semantics;
- cross-environment materialisation reuse;
- incremental model write strategies;
- Nessie promotion;
- data diff;
- daemon.

## Implementation notes

Phase 3 is **partially implemented** (audited against code and tests). See
[`docs/state.md`](../state.md).

- `ModelVersion` component hashes computed in dependency order during
  compilation (`core::version`); canonical AST hashing means formatting and
  comments do not rebuild.
- `SourceStateProvider` interface (empty + static implementations);
  `Adapter::source_state` reads Iceberg snapshot ids (falling back to a schema
  fingerprint) and `collect_source_states` lowers them into a provider, wired
  into CLI plan/apply/run (and inspect/lineage/impact) enrichment.
- SQLite `model_versions` table with `record_materialized`,
  `materialized_version` and `materialized_by_hash`.
- `Planner` is state-aware: `build` / `skip` / `cached` with structured
  `ChangeReason`s. `cached` executes verified adoption: the plan carries the
  source materialisation's provenance and the runner re-verifies the live
  output identity (Iceberg snapshot) before adopting, falling back to `build`
  on stale evidence.
- Runner records materialisations and rejects stale plans.
- `inspect` exposes desired/current versions and status.

Tests cover formatting/comment stability, SQL/config/materialisation/upstream
and source-state invalidation, owner/tag non-invalidation, second-run skip,
cross-environment `cached` adoption, stale-identity build fallback, and
stale-plan rejection — plus the live Nessie e2e adopting inherited
materialisations with zero model SQL.

Remaining gaps: no direct test for `compiler_semantics_change`. Non-Iceberg
sources fall back to a schema fingerprint rather than data state. `cached`
adoption is same-adapter only and requires the relation to be visible through
the target environment's binding (true for Nessie branch inheritance).

Deferred, as listed above: optimiser semantics.

