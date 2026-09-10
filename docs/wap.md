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
recorded in `.phlo/transform/environment.json`.

A failed run or audit leaves the candidate isolated and does not advance
`main`. The live `nessie_wap_e2e` test exercises candidate isolation, data
diff, promotion and stale-promotion rejection against Nessie + Trino/Iceberg.

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

- Trino's Nessie Iceberg catalog does not support views; candidate environments
  therefore support table and incremental models, and view models must use a
  different catalog type.
- Schema-policy gates and Iceberg snapshot tracking are not yet wired into the
  audit stage.
- Temporary candidate catalogs/branches are not cleaned up automatically.
- Promotion merges at the Nessie reference level; a candidate planned against a
  base that has advanced is rejected rather than rebased.
