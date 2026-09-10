# 3. One workspace, one graph

dbt's unit of organisation is the project: one `dbt_project.yml`, one
namespace, with other projects bolted on through a package mechanism.
Phlo's unit is the workspace, the whole repository, and it dissolves the
project boundary on purpose.

## Discovery instead of declaration

A Phlo workspace finds models wherever they live:

```text
transforms/                        # shared, repo-level transforms
workflows/assay/transforms/        # transforms owned by a workflow
workflows/reporting/transforms/
```

Drop a `.sql` file into any of those roots and the file joins the graph.
Same compiler, same rules, same namespaces. A model under
`workflows/assay/transforms/` belongs to the `assay` workflow and to the
one workspace graph, so `reporting.monthly` can depend on `assay.results`
with an ordinary `from` clause:

```text
$ phlo-transform list

Models (3)
  assay.results         [table] <- assay.raw
  assay.raw             [table]
  reporting.monthly     [view]  <- assay.results
```

No package installation, no cross-project `ref`, no import step. The graph
is derived from the files that exist.

## Why one graph matters

Split the graph and every downstream feature gets worse:

- **Lineage stops at the boundary.** "What consumes `assay.results`?" turns
  into a question for three tools and a wiki page.
- **Rebuild planning cannot see upstream.** An upstream project rebuilt.
  Downstream staleness gets handled by convention and cron ordering.
- **Tests cannot gate across the boundary** without orchestration glue.

One graph means `lineage`, `impact`, `plan` and `test` all see the same
truth:

```text
$ phlo-transform lineage reporting.monthly
Model:      reporting.monthly
Upstream:   assay.results, assay.raw
Downstream: (none)
```

```text
$ phlo-transform impact assay.raw
assay.results
reporting.monthly
```

## The graph is typed, not textual

The compiler resolves columns and types, so `explain` reports more than
edges. `marts.daily` reads `staging.stg_events`, and its `day` column
derives from `staging.stg_events.occurred_at` as a `DATE`:

```text
Columns (3):
  day    DATE     unknown   <- staging.stg_events.occurred_at
  kind   VARCHAR  unknown   <- staging.stg_events.kind
  n      BIGINT   not null
```

Column-level truth is what powers the generated tests, the schema
contracts, and column-level impact analysis. `@key` on a model implies
unique and not-null assertions compiled to real SQL tests.

The compiler also needs the catalogue for full fidelity. Offline, an
external source's column types come back `unknown`. Pass `--adapter` and
Phlo enriches the types from the real schema.

## Ownership without fragmentation

Workflows still own their folders. `workflows/assay/` is the assay team's
directory, and workspace policy can gate which workflows may consume which
namespaces. Ownership is metadata on one graph, not a wall between graphs.
That is the balance: monorepo coherence with per-team boundaries.

*Next: [Plan before you apply](04-plan-before-apply.md).*
