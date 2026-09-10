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
- **full** — explicit full keyed comparison.
- **sampled** — deterministic sample; coverage is recorded.

Keys are never configured twice: the same declaration drives incremental merge,
assertions and diffing.

## Policies

```toml
[model.assay_results.diff]
max_removed_rows = 0
max_changed_fraction = 0.05
max_added_rows = 10000
```

Supported gates: `max_added_rows`, `max_removed_rows`, `max_modified_rows`,
`max_changed_fraction`, `require_keyed_diff`, `require_full_diff`. A failing
policy fails the command and blocks promotion.

## WAP integration

`PromotionRequest` accepts `diff_passed` and `require_diff`; when a diff gate
is required and no passing diff exists, promotion is refused. Promotion
artifacts record the promotion; wiring the exact diff id into the promotion
record is a follow-up.

## Execution and coverage

All comparisons run in the warehouse; only summaries return to Rust. A keyless
model can still be diffed (aggregate coverage); if policy requires keyed/full
coverage, promotion blocks.

## Tests

- policy threshold pass/fail and removed-row gating (unit);
- keyed diff correctness against live Trino (added/removed/modified/unchanged
  and per-column counts) in the ignored integration suite.

## Deferred

Partition-metadata pruning, statistical distribution summaries, example-value
redaction policy, and stale-diff invalidation beyond the promotion gate flag.
