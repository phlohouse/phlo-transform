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
  compilation/materialisation semantics change (v3: the catalog left the
  target input — see below).
- `target_hash` covers the *content slot* — `schema.table` — and the
  materialisation. The catalog is deliberately absent: it is each
  environment's binding of the slot to a physical location, not model
  content, so identical content must hash identically in `main` and in a
  candidate catalog for cross-environment reuse to be possible. Target
  *moves* (a different catalog) are still detected — `diff_reasons`
  compares full target displays, and `changed_models` does too.

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
materialised in the current environment but a record for another environment
vouches for the same physical output — reachable here. A version hash alone
is *not* enough: the record must also

- name the same content slot (`schema.table`; the catalog may differ —
  under Nessie each environment's catalog is a branch view of the same
  repository, so the base's slot *is* the candidate's slot, bound under a
  different name);
- have been produced by the same adapter — execution semantics differ across
  engines, so another adapter's output cannot be assumed byte-identical; and
- carry a strong `output_identity` that still matches the relation's current
  `output_identity` — the record is historical, so without proof the
  relation still holds what was written, a version hash is metadata, not
  evidence (a later writer may have overwritten it).

The matched record rides along on the plan as `reuse` (source environment,
source target, identity, producing run, materialisation timestamp). At run
time the identity is re-verified and the output is *adopted*: an
environment-local materialisation record is persisted naming the source's
run id and timestamp — the run produced no new output — and the source
environment's time-window watermark is copied. A stale or unreadable
identity flips the action back to `build` with a `cache_miss` reason.
Adoption is also how a catalog retarget lands: a same-version record whose
recorded target differs by catalog adopts the output at the new target when
the identity verifies, rather than rebuilding identical content.

`output_identity` is the adapter's strong physical identity: an unchanged
value proves the same materialised output (Trino reports the Iceberg
snapshot id; a non-Iceberg relation — or DuckDB, whose schema+row-count
`source_state` cannot detect in-place updates — reports `None`). Adapters
without provable identity cannot authorise cross-environment reuse. The
broader `source_state` fingerprint remains for source-change detection;
only `output_identity` is cache evidence.

A same-version record that fails any check yields `build` with a
`cache_miss` reason (the detail names the mismatch: different relation,
foreign adapter, stale or absent output identity). Records written
before adapter or output-identity tracking cannot authorise reuse. When the
*current* environment's recorded materialisation was produced by a
different adapter — or predates adapter tracking entirely — the plan
rebuilds with an `adapter_change` reason; when its recorded output
identity no longer matches the physical relation, it rebuilds with
`output_drift`.

This is covered by `cache_reuse_across_environments_is_reported_as_cached`,
`cache_reuse_matches_across_catalogs_on_the_same_content_slot`,
`cache_reuse_rejects_a_different_content_slot`,
`cache_reuse_requires_same_adapter`,
`cache_reuse_rejects_unrecorded_adapter`,
`cache_hit_requires_the_relation_to_still_hold_the_recorded_output`,
`cache_hit_rejects_records_without_an_output_fingerprint`,
`a_cached_run_adopts_the_verified_output_into_the_environment`,
`a_cached_run_falls_back_to_build_when_identity_drifts`,
`a_catalog_retarget_adopts_the_visible_output`,
`a_catalog_retarget_without_verifiable_identity_rebuilds`,
`materialisation_by_another_adapter_rebuilds`,
`materialisation_by_an_unrecorded_adapter_rebuilds` and
`output_drift_invalidates_the_current_environments_record`. The real
Trino/Iceberg/Nessie path is covered by
`candidate_reuses_inherited_materialisations_without_rebuilding`
(`phlo-transform-trino/tests/nessie_wap_e2e.rs`).

## Materialised state

The state store gains a `model_versions` table recording, per model and
environment, the `ModelVersion` attached to the most recent successful
materialisation (plus target, run id, timestamp, incremental strategy/key,
`version_detail`, the producing `adapter`, the strong `output_identity`
the adapter observed on the relation right after writing, the declared
`contract`, and the `effective_key` — the row-identity key the model
materialised with, persisted so later comparisons need not reconstruct it).
`effective_key` is tri-state: a recorded set of claims, an empty set
(recorded "no key"), or NULL for rows that predate the column. Opening the
database backfills legacy rows that recorded an incremental `key` strategy;
rows with no surviving key evidence read as *unknown* — promotion treats an
unverifiable key as a breaking change, not as proof the base was keyless.
API:

```text
record_materialized(record)
materialized_version(model_id, environment)
materialized_by_hash(version_hash)
```

The runner records a materialisation after each `passed` build — but only
while the relation still holds what the run wrote: the post-build
`source_state` is re-read at record time, and a drifted output is skipped
with a warning rather than claimed (a concurrent writer's version stands).
A `cached` action records one too — the *adopted* record carries the source
materialisation's `run_id`/`materialized_at` verbatim, so history shows
which run produced the bytes, not which run noticed them.

Because the store is shared, same-key writes are ordered rather than
last-writer-wins:

- `model_versions` applies only a record whose `materialized_at` is not
  older than the stored one — an earlier run finishing later cannot regress
  the row;
- `seed_loads` orders identically on `loaded_at`;
- `incremental_state` orders by *run generation* — the watermark a
  later-started run observed supersedes an earlier run's, whatever order
  the writes land in. Writers without a `runs` row fall back to update
  order.

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
- `skip` — this environment's record already vouches for the desired
  version at this target;
- `cached` — another environment's record plus a live identity proof let
  this environment adopt the output (see *Cached reuse* above).

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
and each model/seed/test record is written as it transitions
(`model_runs`, `seed_runs`, `test_runs`; per-attempt detail lives in
`attempts_json`). Every failed attempt is persisted before its retry
backoff begins — a process killed mid-retry still shows the attempts it
made. State-store write failures are fatal to the run rather than silently
dropped: an execution record that cannot be written cannot be trusted for
resume. A killed process therefore leaves an accurate partial record:
passed work is marked passed, everything else stays unfinished.

Each execution record carries the fields needed to reconstruct what
happened: status (`passed`/`failed`/`skipped`/`cached`/`blocked`/
`cancelled`), attempt count, per-attempt failure detail (`category`,
message, adapter error code, retryable flag), timestamps, desired version
and query id. `finish_run` stamps the run's final status and failed count.

This is what `--resume` and `--retry-failed` rebuild from:

- **`run --resume <run-id>`** continues an *interrupted* run — still
  `running` after a kill, or `cancelled` — under the same run id. It
  reloads the stored plan's model set and prior per-node records, then
  re-runs every model through the normal planner so the action, the
  `full_rebuild` decision (incremental strategy/key changes and
  schema-change classification) and the time-window watermark all reflect
  current state. Reuse is verified, not trusted: a previously-passed model
  is kept only if its desired version still matches the fresh compile
  *and* its target relation still exists; a seed is reused only when its
  recorded content hash matches the current file and its target exists.
  Stored `skip`/`cached` decisions — and stored incremental decisions —
  are never trusted: a version that moved since the interruption becomes a
  build. A finished run is not resumable: `failed` redirects to
  `--retry-failed`, `passed` is a no-op.
- **`run --retry-failed <run-id>`** creates a new run (`continued_from`
  links back) over the failed/blocked/cancelled models of a finished run,
  the dependencies they still need, and any tests that failed — tests over
  rebuilt models re-verify as well. If nothing failed there is nothing to
  retry and the command says so.

Safe to reuse: `passed` model records whose desired version still matches
and whose target still exists, and `passed` seed records with matching
hashes and existing targets. Never trusted: `failed`/`blocked`/`cancelled`
records, unfinished records from a killed run, stored `skip`/`cached`
actions whose underlying version moved, and any record whose desired
version differs from the current compile.

## Inspect

`inspect` shows desired/current versions and `status` (`new`/`changed`/
`unchanged`); JSON exposes the component hashes and state.

`phlo-transform state` inspects the store directly (read-only):

- `state runs` — recorded runs, newest first, with environment
  (`--environment` filters);
- `state show <run-id-or-prefix>` — one run's model, seed and test records;
- `state model <name>` — the recorded materialised version for the
  effective environment: version components, target, adapter, contract
  summary, run;
- `state promotions` — promotion history;
- `state evidence [candidate-ref]` — the audit evidence promotion reads:
  the newest environment, branch-diff and lineage-diff record per
  candidate/target pair, or every record for one candidate.

All five honour `--json`.

## Audit evidence

The `evidence` table is the portable authority for promotion audits:
environment bindings, branch diffs and lineage diffs are appended as
immutable `EvidenceRecord`s — kind, subject (the candidate ref), target
ref, candidate/target commit hashes, a definitional fingerprint, the typed
payload the artifact files also export, an optional run id, and the
evidence's own timestamp. Records are never updated; a later audit of the
same subject appends, so the store carries the history.

Because the records live in the store, a shared PostgreSQL backend makes
promotion evidence portable across machines and CI stages: one stage runs
and diffs the candidate, another promotes it, and no artifact files need
to be copied. The `.phlo/transform/*.json` artifacts remain the
human-readable exports and the compatibility path: a workspace that
predates the table keeps working, and file evidence the store has not
seen is imported on read so it becomes portable from then on. Store read
failures fail closed — never a silent fallback to files that could
disagree with what another stage recorded — and so does the import: a
file whose record cannot be persisted is rejected rather than audited
locally while the authoritative store knows nothing of it. Environment
evidence is removed when its branch is deleted; branch- and lineage-diff
records are kept as audit history.

## Shared and concurrent state

`StateStore` has two backends:

- **SQLite** (default) at `.phlo/transform/state.db`, or an explicit file
  path via `--state <path>`/`PHLO_STATE_URL`. A five-second `busy_timeout`
  lets concurrent writers (parallel `phlo-transform` processes sharing one
  state file) wait out lock contention instead of erroring.
- **PostgreSQL** via `--state postgres://…`/`--state postgresql://…` (or
  `PHLO_STATE_URL`) — the shared backend: CI jobs and developers can record
  into one store, so materialised versions and watermarks are visible
  across machines. The schema mirrors SQLite's; upserts keep writes atomic
  and an identity column preserves deterministic newest-first ordering.
  An unreachable `--state` location is a hard error — it never falls back
  to local state silently, and credentials embedded in the URL are stripped
  before it appears in errors or `doctor` output.

## Tests

- canonical hash equivalence (formatting/comments);
- SQL, config, materialisation and upstream-change invalidation;
- owner/tag changes do not rebuild;
- first run builds, unchanged second run skips (fake adapter + SQLite);
- stale plan rejection;
- cache reuse gated on same target + same adapter;
- concurrent SQLite writers on a shared state file;
- Postgres backend round-trip (opt-in via `PHLO_TEST_POSTGRES_URL`);
- evidence append/read ordering, latest-per-(subject, target) semantics,
  cross-connection visibility and corrupt-row rejection on both backends.

## Deferred

Advanced SQL equivalence.
