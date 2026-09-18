# 6. Safe by default: branches, audits and diffs

Everything so far has been about building the right data.

Shipping it safely is a different problem.

A model can compile. Its SQL can execute. Its tests can pass. And it can still be the wrong thing to publish to production.

Phlo Transform's lakehouse workflow is built around a simple rule:

> **Do not test a proposed production change by writing it directly into production.**

Instead:

```text
WRITE      build the change in isolation
  │
  ▼
AUDIT      inspect and test what was actually built
  │
  ▼
PUBLISH    move the audited state into the target
```

This is Write-Audit-Publish, or WAP.

## First: what are Iceberg and Nessie doing here?

Apache Iceberg is a table format for large analytical datasets. An Iceberg table has metadata describing snapshots of the table over time.

Project Nessie provides version-control-like references over lakehouse metadata.

The useful mental model is deliberately Git-like:

```text
main ────── A ────── B
              \
               └──── candidate
```

A branch is a separate reference to lakehouse state. Writes on the candidate can advance that candidate without advancing `main`.

Phlo Transform uses that primitive as the isolation boundary for production changes.

## What is an environment in Phlo Transform?

An **environment** is the logical execution context for a run.

On the Nessie path, the environment is anchored by a Nessie reference such as:

```text
main
ci/pr-42
release/2026-09
```

But a SQL engine such as Trino also needs a physical catalog configuration telling it which Nessie reference to use.

So Phlo Transform keeps two concepts separate:

```text
logical environment/reference
          │
          ▼
physical catalog binding
```

For a candidate, Phlo Transform can provision a branch-scoped catalog such as:

```text
phlo_ci_pr_42_<hash>
```

The hash prevents references that normalise to similar names from accidentally sharing a physical catalog.

That catalog points Trino/Iceberg at the candidate reference.

## Creating a candidate

A typical flow begins with:

```bash
phlo-transform ref create ci/pr-42 --from main
```

Then:

```bash
phlo-transform run --ref ci/pr-42
```

The environment resolver makes sure the candidate reference and its physical catalog binding are coherent.

If the candidate catalog is created by Phlo Transform, ownership is recorded. If the catalog already exists but cannot be proven to belong to this candidate, Phlo Transform fails closed rather than adopting a possibly foreign resource.

This ownership detail matters later when cleanup decides whether a catalog is safe to drop.

## A branch starts by inheriting the base

A new Nessie candidate created from `main` initially points at the same existing table snapshots.

```text
main                  ci/pr-42
│                     │
└─ assay.results S10  └─ assay.results S10
```

This is where executable cache reuse becomes especially useful.

If the candidate's compiled model is unchanged and its target verifies the same Iceberg snapshot, `run --ref` can adopt the inherited materialisation as `CACHED` rather than rebuilding it.

Only changed or unproven outputs need physical model SQL.

The candidate therefore becomes an isolated *delta* over a verified base rather than a full duplicate build by default.

## A successful run is evidence, not permission by itself

The candidate run records:

- the environment;
- the run and plan;
- model, seed and test outcomes;
- the Nessie reference hash the completed run validated.

The post-run reference hash is important because candidate writes themselves advance the branch. Promotion needs to know the exact candidate state that was actually tested.

But a green run is only one part of promotion evidence.

## What does “audit” mean?

An audit asks several different questions.

### Did execution and tests succeed?

A candidate with failed, blocked or cancelled work is not publishable.

### Did the data change in an acceptable way?

A data diff compares candidate and base relations.

### Did the schema or contract change?

Removing a required column or changing a type incompatibly can break consumers even when row-level tests pass.

### Did lineage change?

A transformation can alter which inputs feed which outputs without changing obvious row counts.

### Is the target still the target we audited against?

If `main` advanced after the audit, the candidate was reviewed against an older reality.

Promotion must not silently merge over newer work.

These are different gates because they protect against different failure modes.

## Data diffs happen in the warehouse

A diff should not download an entire table into the CLI merely to compare it.

Phlo Transform asks the warehouse to calculate the comparison.

For a keyed model, row identity allows a `FULL OUTER JOIN` style comparison that can classify rows as:

```text
added
removed
modified
unchanged
```

and count changes per column.

For keyless data, aggregate comparison is the honest fallback.

Partition-aware models can use Iceberg partition metadata to reduce work.

Numeric tolerances can be configured so irrelevant floating-point noise does not become a failed promotion.

Sampling is available when a full comparison is unnecessary or too expensive.

A branch-wide diff can be produced with the candidate and base refs:

```bash
phlo-transform diff --from ci/pr-42 --to main --full
```

The resulting report is bound to the candidate and base reference hashes it actually inspected.

## Why a diff result needs provenance

Imagine this sequence:

```text
1. diff ci/pr-42 against main
2. candidate changes
3. promote using the old diff
```

A boolean `diff_passed=true` is useless if we cannot prove *which commits* it refers to.

So audit evidence carries provenance such as:

```text
candidate ref
candidate hash
target ref
target hash
timestamp
payload/fingerprint
```

If the reference moves, the old evidence becomes stale.

## Audit evidence is portable state

Early versions of Phlo Transform used workspace JSON artifacts as the primary audit record.

That is not enough for real CI.

A common pipeline is:

```text
machine A / CI stage 1
    run candidate
    create diffs

machine B / CI stage 2
    approve/promote
```

Machine B should not need a copied `.phlo/` directory merely to know what A proved.

In v0.1, promotion evidence is persisted in the state store as immutable evidence records.

Evidence kinds include:

```text
environment binding
branch diff
lineage diff
```

SQLite supports the same model locally. Postgres makes it portable across machines and CI stages.

JSON files remain useful exports and compatibility inputs, but when a state store is configured the store is authoritative.

A file that cannot be persisted into the configured evidence store is not allowed to authorise a local-only promotion. That would create two different truths on two machines.

## What does “immutable evidence” mean?

An audit result is not updated in place when the candidate changes.

A new audit creates a new evidence record.

That gives history rather than a mutable “current result” slot:

```text
candidate X → main at hashes A/B   evidence e1
candidate X → main at hashes C/B   evidence e2
candidate X → release at C/R       evidence e3
```

The promotion logic selects evidence for the exact candidate/target pair and verifies it still applies.

Old evidence remains historical context until environment cleanup removes lifecycle-specific binding evidence; completed promotion history is stored separately.

## Promotion records point to the evidence actually consulted

This is a subtle but important v0.1 feature.

Suppose two CI workers can write audit evidence concurrently.

If promotion first evaluates evidence record `e1`, then later performs a new “give me the latest evidence” query to populate its audit trail, another worker might have inserted `e2` in between.

The promotion record would then claim the wrong evidence authorised the decision.

Phlo Transform avoids that race by carrying the selected evidence ID *with the evidence read itself*.

A promotion record can therefore contain exact IDs for the records consulted:

```text
environment evidence id
branch-diff evidence id
lineage-diff evidence id
```

The historical record says which immutable evidence the gates actually used, not whichever row became latest afterwards.

## Promotion is a checked state transition

The workflow can be previewed:

```bash
phlo-transform promote ci/pr-42 --to main --check
```

and then performed:

```bash
phlo-transform promote ci/pr-42 --to main
```

Depending on policy/options, gates include:

- candidate quality/run status;
- required data diff;
- schema/contract safety;
- lineage evidence;
- target staleness;
- Nessie merge/conflict check.

The target hash is checked again. If `main` advanced after the evidence was established, promotion refuses rather than redefining what was audited.

That is optimistic concurrency applied to data delivery.

## Promotion history is durable

A completed promotion record includes the important identities around the decision:

```text
promotion id
candidate ref/hash
target ref/hash before and after
plan id
run id
gate results
evidence ids
actor
timestamp
merge/conflict outcome
```

It is persisted in state and exported as a human-readable artifact.

That makes promotion a queryable event rather than an ephemeral terminal message.

## Cleanup also has to be evidence-driven

After a candidate is merged or abandoned, cleanup may remove:

- the Nessie branch;
- the Phlo Transform-owned candidate catalog;
- the environment binding evidence.

But Phlo Transform only drops a physical catalog when recorded evidence says Phlo Transform owns it.

An unproven catalog is left alone.

This sounds conservative because it is. Resource cleanup must not become a mechanism for deleting somebody else's catalog.

The same cleanup path is used by explicit ref deletion:

```bash
phlo-transform ref delete ci/pr-42
```

and promotion cleanup where requested.

Retries are idempotent enough to finish cleanup even if the branch disappeared in an earlier partial attempt.

## What WAP changes conceptually

Without WAP, a production transformation often looks like:

```text
run production
      │
      ▼
see whether tests pass
```

With WAP:

```text
base production state
      │
      ├── create candidate
      │
      ▼
build candidate
      │
      ▼
test + diff + lineage + contract audit
      │
      ▼
verify base has not moved
      │
      ▼
promote exact audited candidate
```

The difference is not “branches are cool”.

The difference is that **evidence precedes publication**.

## The current limits are explicit

v0.1 is intentionally conservative.

- Nessie-backed Iceberg catalogs do not support views, so Nessie-targeted projects materialise those outputs as tables.
- Shared Postgres state currently uses `NoTls`, so it belongs on trusted networks.
- A candidate that does not physically inherit a reusable output cannot magically cache it; it builds.
- If strong physical identity cannot be proven, Phlo Transform rebuilds rather than assuming reuse is safe.

Those limits all follow the same rule:

> When certainty is missing, do more work rather than make a stronger correctness claim than the evidence supports.

The final architectural question is how all of this actually executes: concurrency, failures, retries, cancellation, resume, seeds and tests. We cover that after the migration chapter.

*Next: [Leaving dbt without losing your work](07-leaving-dbt.md).*