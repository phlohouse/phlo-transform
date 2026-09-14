# 6. Safe by default: branches, audits and diffs

So far this series has covered the developer loop: write SQL, compile, plan,
run, test. The harder problem is *shipping*. A transformation that looks
fine in a plan can still corrupt production data. Phlo's answer is
Write-Audit-Publish on Nessie branches: write to an isolated candidate,
audit the result, then promote on evidence.

## Branches are the environment

There is no separate "environment" abstraction. A Nessie reference *is* the
environment:

```bash
phlo-transform apply --ref ci/pr-1 --from main
```

That one command does three things:

1. creates the Nessie branch `ci/pr-1` from `main` if the branch is missing;
2. provisions a branch-scoped Trino catalog (named `phlo_<ref>_<hash>` by
   default — the hash makes punctuation-equivalent refs collision-proof —
   overridable with `--catalog`) that points at the branch;
3. compiles the workspace against that catalog and applies the plan there.

`main` is untouched. Tests run inside the audit step against the candidate
data, so a failing model fails a branch, not a dashboard.

## Promotion is a gate, not a merge

```bash
phlo-transform promote ci/pr-1 --to main
phlo-transform promote ci/pr-1 --to main --check      # report only
phlo-transform promote ci/pr-1 --to main --cleanup    # drop branch + catalog after merge
```

`promote` refuses unless the evidence exists:

- the candidate needs a successful recorded run. No green run, no merge;
- with `--require-diff`, a passing data diff must be on record;
- breaking schema changes from the audited diff block the merge unless you
  pass `--allow-breaking-schema` explicitly;
- a target that advanced since the audit fails as a stale promotion rather
  than merging over someone's newer work.

A successful promotion writes `promotion.json` under `.phlo/transform/`:
promotion id, candidate and target hashes, plan and run ids, conflicts, the
timestamp, and the actor. The audit trail is a file in the repo's state
directory, not a log line.

Rollback is the inverse operation on the same primitive:

```bash
phlo-transform rollback --ref main --to <nessie-hash>
```

## Diffs answer "did the data actually change?"

Tests check assertions. A diff checks the data itself, computed with
warehouse-side SQL rather than pulled over the wire:

```bash
phlo-transform diff assay.results --ref feature/x --base main
phlo-transform diff assay.results --sample 0.05 --json
```

The strategy follows what the model already declares. A model with `@key`
or `@incremental key=` gets a keyed diff: added, removed, modified, and
unchanged row counts plus per-column change counts, using a null-safe
`FULL OUTER JOIN`. A model without a key gets an aggregate row-count
comparison. `--sample` uses a real `TABLESAMPLE`. `--full` compares
everything. Partition-aware models compare Iceberg `$partitions` metadata.
Numeric columns can carry tolerances in `phlo.toml` so a meaningless
floating-point wobble does not fail a promotion.

The same key declaration drives the incremental merge, the generated
uniqueness tests, and the diff. You state identity once and three features
agree on it.

## The honest limits

This layer is the newest part of Phlo and the roadmap marks it partial. The
promotion record does not yet capture the Iceberg snapshot id, a candidate
is not automatically rebased when the target advances, and the Nessie
Iceberg catalog has no views. The shape is proven end to end against live
Nessie + Trino + Iceberg. The polish is still landing.

*Next: [Leaving dbt without losing your work](07-leaving-dbt.md).*
