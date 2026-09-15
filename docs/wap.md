# Phase 5 Nessie environments and WAP

Versioned lakehouse state is a first-class primitive: Nessie references are the
environment, and production changes go through Write-Audit-Publish rather than
writing `main` directly.

## Nessie boundary

`phlo-transform-nessie` provides a `NessieClient` trait, separate from SQL
execution:

```text
get_reference
list_references
create_branch
delete_branch
merge            (with expected target hash)
can_merge
assign_reference (rollback/reset)
```

Two implementations ship: `NessieRestClient` (Nessie REST v2) and
`InMemoryNessie` (offline/tests). WAP orchestration is tested against the
in-memory client.

## Environments

`--ref <reference>` (alias `--reference`) selects the environment for
`plan`/`apply`/`run` and is recorded in plans, runs and state. It defaults to
`--environment` when set. There is no separate environment abstraction layered
on top of Nessie.

## Reference management

```bash
phlo-transform ref list
phlo-transform ref show ci/pr-1
phlo-transform ref create ci/pr-1 --from main
phlo-transform ref delete ci/pr-1
```

`list`/`show` are read-only. `create` and `delete` are the only commands that
mutate Nessie references directly, and they do exactly what they say — nothing
creates or deletes a branch as a side effect of another operation except the
explicit provisioning on `plan`/`apply`/`run --ref` and `--cleanup` on
`promote`. `ref delete main` is refused outright: `main` is every
environment's default base, not a scratch branch.

## WAP

```text
PLAN → WRITE candidate branch → AUDIT (tests) → PUBLISH (promote)
```

`apply --ref <candidate> --from <base>` provisions the candidate automatically:

1. the Nessie branch is created from `--from` (default `main`) when missing;
2. a branch-scoped Trino catalog is created dynamically (Trino cannot switch
   the Nessie reference of an existing catalog at query time), pointing at the
   branch;
3. models are compiled with that catalog as the physical target and applied
   there. `main` is untouched until promotion.

The provisioned catalog name defaults to `phlo_<sanitized ref>` and can be
overridden with `--catalog`; `--warehouse` sets the Iceberg warehouse (for
example `local:///tmp/phlo-warehouse` or `s3://bucket/wh`). Provisioning is
recorded in `.phlo/transform/environment.json` — the workspace's current
environment — plus a per-candidate copy named
`environment_<sanitised ref>_<hash>.json`, so one candidate's evidence
survives another being provisioned and similarly-named refs never share a
file. Both are removed when the branch is deleted.

Each artifact records the commit the candidate was *provably* cut from as
`created_from` — set when `ref create` or provisioning creates the branch,
and preserved across later re-provisioning. A branch Phlo did not create has
unrecorded provenance unless an earlier artifact captured it; `promote`
refuses such candidates rather than treat today's target as their base.

A failed run or audit leaves the candidate isolated and does not advance
`main`. The live `nessie_wap_e2e` test exercises candidate isolation, branch
diff, gate evaluation, promotion, stale-promotion rejection and branch cleanup
against Nessie + Trino/Iceberg.

## Branch diff

```bash
phlo-transform diff --from ci/pr-1 --to main
phlo-transform diff --from ci/pr-1 --to main --full   # deep keyed diffs
```

With no model argument, `diff` compares two references: every dataset known to
the workspace or recorded in state is classified `added`, `removed`,
`changed`, `unchanged` or `absent`, schema and nullability changes are listed
per model, and row counts come from the catalogs. `--full` additionally runs
keyed value diffs on changed models that declare keys. The report is written
to `.phlo/transform/branch_diff.json` and is the audit artifact `promote`
consumes. See `docs/diff.md`.

## Promotion

```bash
phlo-transform promote ci/pr-1 --to main
phlo-transform promote --from ci/pr-1 --to main        # equivalent
phlo-transform promote ci/pr-1 --to main --check
```

Promotion is authorised by named gates, printed and emitted identically in
JSON:

```text
PASS run        — run ba168bc5 passed candidate@3f8c…
PASS tests      — 3 tests passed
PASS blocked    — no blocked or cancelled work
PASS schema     — no breaking schema changes
PASS data_diff  — data diff passed
PASS base       — target unchanged since the evidence base was recorded
PASS conflicts  — candidate merges cleanly
```

- `run` — the candidate's latest recorded run finished fully passed **and**
  validated the exact commit being promoted: a passed run is bound to the
  candidate branch's **post-run** head (`reference_hash`), since a Nessie
  branch advances with every write the run makes. A branch that advanced
  since its run — or a run recorded before commit binding — fails this
  gate.
- `tests` — no test in that run failed.
- `blocked` — no model, seed or test was left blocked or cancelled.
- `schema` — a fresh audited diff inspected this pair at these commits and
  found no breaking schema or contract changes, or they were waived with
  `--allow-breaking-schema`. Physical schema breaks come from the audited
  diff; contract breaks (a dropped contract column, a removed contract,
  enforcement turned off, an incompatible type change, a relaxed
  nullability guarantee, a declared rename, a changed or dropped effective
  key) are computed live at promotion time — the workspace's desired
  contracts against the contracts the target environment last recorded —
  so editing a contract after `diff` cannot bypass the gate on a stale
  artifact. A state-store error fails promotion rather than reading as "no
  contracts recorded". Without an audited artifact the gate fails closed:
  "no evidence" never reads as "no changes". The waiver covers the schema
  gate only — it does not waive provenance or run binding.
- `data_diff` — only evaluated with `--require-diff`; the audited diff
  (`branch_diff.json` from a `--full` branch diff) must exist, pass its
  policies, cover this exact candidate→target pair **at the commits being
  promoted** (the artifact records both refs' resolved heads; an artifact
  bound to older heads, or to none, is rejected), and still be fresh (its
  recorded versions match both sides' current materialisations). A shallow
  branch diff — schema and row counts only, no value-level policies — or a
  diff that compared a relation to itself does not satisfy it. A
  single-model `diff.json` is never promotion evidence: it examined one
  model and cannot certify a branch.
- `base` — the target's hash still equals the commit the evidence was
  established against: the hash-bound diff artifact's recorded base, or
  failing that the immutable `created_from` provenance recorded when the
  candidate branch was cut. A candidate whose origin is unrecorded and which
  was never audited against this target fails this gate — its base is never
  silently redefined as today's head. A target that advanced fails too; the
  merge itself asserts the hash the gates were evaluated against (the
  evidence hash, or the hash resolved at promotion time when none was
  recorded) so a racing commit is rejected by Nessie rather than silently
  merged. The same holds on the candidate side: a branch that advanced
  between gate evaluation and merge is refused rather than promoted
  unaudited.
- `conflicts` — a non-destructive merge check reported no conflicts. Against
  a real Nessie this is the server's `dryRun` merge: the same conflict
  detection the merge performs, without committing anything.

`--check` evaluates and prints the gates without merging. A promotion that
passes every gate merges, writes `promotion.json` to `.phlo/transform/` and
persists the record in the state store's `promotions` table — promotion id,
candidate/target refs and hashes, plan/run ids, gate results, merged flag,
conflicts, timestamp — for later APIs and audit. With `--cleanup`, the
candidate branch and its catalog are removed after a successful merge only;
a cleanup failure is reported and fails the command (the promotion record
already persisted still shows the merge succeeded).

## Rollback

```bash
phlo-transform rollback --ref main --to <nessie-hash>
```

Environment-level rollback moves a reference to a known prior hash.

## State

Runs record the environment (Nessie reference). The same logical model version
materialised on one reference is not treated as current on another: state is
keyed by model and environment.

## Limitations

- Trino's Nessie Iceberg catalog does not support views; candidate environments
  therefore support table and incremental models, and view models must use a
  different catalog type.
- Iceberg snapshot ids are used for source state and as the branch diff's
  physical version signal when no state record exists, but are not recorded in
  the promotion record itself.
- Promotion merges at the Nessie reference level; a candidate planned against a
  base that has advanced is rejected rather than rebased. `--cleanup` removes
  the candidate after a successful merge.
