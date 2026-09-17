# The Phlo Transform series

This series explains Phlo Transform from first principles.

It does not begin with “here is a dbt alternative” because that skips the more useful questions:

- Why does a collection of SQL queries become a build system at all?
- What should a SQL compiler infer rather than ask users to configure?
- What is the difference between desired model state and physical warehouse state?
- What does it actually mean to prove a table is unchanged?
- How can an existing lakehouse output be reused without lying about provenance?
- Why should production changes be written and audited before publication?
- What evidence should a promotion record contain?
- How do retries, resume, cancellation and machine clients fit into the same model?

The posts build those concepts up in sequence. You only need to be comfortable reading basic SQL; terms such as DAG, materialisation, Iceberg snapshot, Nessie reference, WAP and content-addressed state are introduced before they are relied on.

## The series

| # | Post | What it builds up |
|---|---|---|
| 1 | [Why does SQL need a build system?](01-why-sql-needs-a-build-system.md) | Queries → models → relations → dependencies → materialisations → desired/current state |
| 2 | [SQL should stay SQL](02-sql-should-stay-sql.md) | What the compiler does: discovery, parsing, relation resolution, logical vs physical names, directives |
| 3 | [One workspace, one graph](03-one-workspace-one-graph.md) | DAGs, multi-root workspaces, types, column lineage, impact, contracts, keys and tests |
| 4 | [Plan before you apply](04-plan-before-apply.md) | State stores, semantic versions, source state, output identity, `BUILD`/`SKIP`/`CACHED`, verified cross-environment adoption |
| 5 | [Incremental models without macros](05-incremental-without-macros.md) | Append, keyed merge, partition replacement, time windows, watermarks and failure-safe state |
| 6 | [Safe by default: branches, audits and diffs](06-safe-by-default.md) | Iceberg/Nessie, environments, WAP, branch/data/schema/lineage evidence, portable Postgres-backed audit records and promotion provenance |
| 7 | [Leaving dbt without losing your work](07-leaving-dbt.md) | Semantic migration, `CLEAN`/`REVIEW`/`UNSUPPORTED`, incrementals/tests/seeds/config translation and verified execution |
| 8 | [How the execution engine works](08-how-execution-works.md) | DAG scheduling, bounded concurrency, seeds, tests, retries, cancellation, resume and retry-failed |
| 9 | [A transformation engine for humans and machines](09-the-machine-interface.md) | The daemon API, read vs operation surfaces, idempotency, durable operation history, auth and agent-friendly semantics |
| 10 | [Putting Phlo Transform together](10-putting-it-all-together.md) | Complete v0.1 lifecycle from blank workspace to verified cache reuse, audit and promotion |

## A few terms up front

You do not need to memorise these; each is explained properly in the relevant post.

- **Model** — SQL owned by the workspace that produces a relation.
- **Source** — an input relation that the workspace reads but does not build.
- **Relation** — a queryable table/view-like object.
- **Materialisation** — the physical representation of a model, such as a table, view or incremental table.
- **DAG** — the directed acyclic graph formed by model/source dependencies.
- **State** — durable evidence of what Phlo previously ran or materialised.
- **Plan** — the comparison between desired compiled state and current recorded/live state.
- **Output identity** — strong physical identity for a materialisation, such as an Iceberg snapshot id.
- **Environment** — the logical execution context; on the lakehouse path it is tied to a Nessie reference and physical catalog binding.
- **WAP** — Write-Audit-Publish: build in isolation, audit the result, then promote the exact audited state.

## What is current in this series

The series reflects the v0.1 implementation, including the features added during the final hardening passes:

- catalog-independent semantic model versions with explicit physical-target staleness checks;
- executable `CACHED` adoption across Nessie environments with run-time output-identity re-verification;
- Trino/Iceberg strong snapshot identity checks and transient identity-read retries;
- real Trino seed loading;
- composite-key test semantics;
- resumable/retryable execution with `Cached` treated as real executable work;
- portable immutable environment/branch-diff/lineage evidence in SQLite/Postgres;
- promotion records bound to the exact evidence IDs the gates consulted;
- shared CLI/daemon environment and promotion semantics;
- versioned daemon status, tracked operations, cancellation and durable idempotency;
- the one-way dbt translator and its verified migration fixtures.

Commands shown in the posts are written against the real CLI. Where v0.1 has a known limitation, the series states it rather than describing a future design as if it already exists.

For reference material rather than narrative explanation, see:

- [`SPEC.md`](../../SPEC.md)
- [`docs/architecture.md`](../architecture.md)
- [`docs/engine.md`](../engine.md)
- [`docs/state.md`](../state.md)
- [`docs/wap.md`](../wap.md)
- [`docs/daemon.md`](../daemon.md)
- [`docs/v0.1-release-notes.md`](../v0.1-release-notes.md)
