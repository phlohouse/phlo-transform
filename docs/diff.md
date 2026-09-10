# Phase 6 native data diff

Data diff compares candidate and base model outputs at the data level, driven
by warehouse-side SQL rather than downloading tables.

## CLI

```bash
phlo-transform diff assay.results
phlo-transform diff assay.results --ref feature/x --base main
phlo-transform diff assay.results --base-relation memory.default.assay__results
phlo-transform diff assay.results --full
phlo-transform diff assay.results --sample 0.05 --json
```

`--base-relation` supplies the base physical relation; without it the model's
own target is used (a no-op comparison). Diff results are written to
`.phlo/transform/diff.json`.

## Identity and strategies

A diff records the model, candidate/base references and versions, the physical
relations, the strategy, cover/part columns and coverage.

- **keyed** — default when the model has a stable key (reused from
  `@incremental key=` or `@key`). Reports added/removed/modified/unchanged
  rows and per-column change counts using a `FULL OUTER JOIN` and
  `IS DISTINCT FROM` (null-safe).
- **aggregate** — row-count comparison when no key is known; coverage is
  marked as aggregate only.
- **full** — full keyed comparison.
- **sampled** — compares `TABLESAMPLE BERNOULLI (<fraction*100>)`; the
  fraction is recorded in the report for reproducibility.
- **partition** — compares partitions using Iceberg `$partitions` metadata
  (`partition`, `record_count`) where available, falling back to grouped
  partition row counts. Reports `partitions_added`, `partitions_removed` and
  `partitions_changed`.

Keys are never configured twice: the same declaration drives incremental merge,
assertions and diffing.

## Numeric tolerances

Per-column tolerances are configured under the model diff policy and applied in
the warehouse comparison (rows within `absolute` or `relative` tolerance are
unchanged):

```toml
[model."assay.results".diff.columns.concentration]
absolute_tolerance = 1e-9
relative_tolerance = 1e-6
```

## Schema diff

`schema_changes` is populated from the candidate and base relation schemas:
added, removed (`full_rebuild_required`) and changed (`review` for numeric
widening, `error` otherwise) columns appear alongside the row-level result.

## Policies

Policies are read from `phlo.toml` when diffing a model:

```toml
[model."assay.results".diff]
max_removed_rows = 0
max_changed_fraction = 0.05
max_added_rows = 10000
require_keyed_diff = false
require_full_diff = false
```

Supported gates: `max_added_rows`, `max_removed_rows`, `max_modified_rows`,
`max_changed_fraction`, `require_keyed_diff`, `require_full_diff`. A failing
policy fails the command and blocks promotion.

## WAP integration

`PromotionRequest` accepts `diff_passed` and `require_diff`. `phlo-transform
promote --require-diff` reads `diff.json`, requires a passing diff, and rejects
a diff whose recorded candidate model version no longer matches the candidate's
materialised version (stale-diff invalidation).

## Execution and coverage

All comparisons run in the warehouse; only summaries return to Rust. A keyless
model can still be diffed (aggregate coverage); if policy requires keyed/full
coverage, promotion blocks.

## Tests

- policy threshold pass/fail and removed-row gating (unit);
- tolerance predicate and sampling unit tests;
- live Trino keyed, tolerance, partition-aware and sampled diff;
- WAP E2E covers keyed diff before promotion.

## Deferred

Statistical distribution summaries and example-value redaction (no example
values are emitted; partition comparison uses Iceberg metadata rather than
scanning data).
