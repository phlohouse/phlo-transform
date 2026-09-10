# Phase 5 Nessie environments and WAP

Versioned lakehouse state is a first-class primitive: Nessie references are the
environment, and production changes go through Write-Audit-Publish rather than
writing `main` directly.

## Nessie boundary

`phlo-transform-nessie` provides a `NessieClient` trait, separate from SQL
execution:

```text
get_reference
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

`--ref <reference>` selects the environment for `plan`/`apply`/`run` and is
recorded in plans, runs and state. It defaults to `--environment` when set.
There is no separate environment abstraction layered on top of Nessie.

## WAP

```text
PLAN → WRITE candidate branch → AUDIT (tests) → PUBLISH (promote)
```

`apply --ref ci/pr-1` writes the candidate; a failed run or audit leaves the
candidate isolated and does not advance `main`.

## Promotion

```bash
phlo-transform promote ci/pr-1 --to main
phlo-transform promote ci/pr-1 --to main --check
```

`promote`:

1. requires a successful recorded run for the candidate reference (quality
   gate);
2. checks the candidate can merge and that the target has not advanced when an
   expected target hash is supplied;
3. merges, or reports without merging for `--check`;
4. writes `promotion.json` to `.phlo/transform/`.

The promotion record captures promotion id, candidate/target refs and hashes,
plan/run ids, dry-run/merged flags, conflicts, timestamp and actor.

Conflicts and stale targets fail with `PROMOTION` diagnostics rather than
silently merging.

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

- The REST client covers the reference/merge operations needed for promotion;
  it is not exercised against a live Nessie in CI. WAP orchestration is covered
  by in-memory tests.
- Schema policy gates and Iceberg snapshot tracking are not yet wired into the
  audit stage.
