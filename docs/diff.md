# Phase 6 native data diff

Data diff compares candidate and base model outputs at the data level, driven
by warehouse-side SQL rather than downloading tables. It operates at two
scopes: a single model, or a whole branch.

## Branch diff

```bash
phlo-transform diff --from ci/pr-1 --to main
phlo-transform diff --from ci/pr-1 --to main --full   # deep keyed diffs
phlo-transform diff --from ci/pr-1                    # --to defaults to main
```

With no model argument, `diff` compares the materialised datasets of two
Nessie references — `--from`, `--ref` or `--environment` names the candidate
(naming it twice with different values is an error, matching `promote`) and
`--to` the base (the model-diff flags `--base`, `--base-relation`,
`--partition` and `--sample` do not apply). When a Nessie endpoint is
configured both references must exist; a typo errors rather than producing a
misleading report. Candidate relations resolve through the provisioned
catalog recorded in the environment evidence (per-candidate, exported to
`environment_<ref>.json`, falling back to the `phlo_<ref>_<hash>`
convention); the base resolves through the
workspace catalog for `main` or the same convention for other refs.
Recorded materialisation targets take precedence over both — and for
`main`, records from runs with no `--ref` (the default environment) count
too.

Every dataset in the union of the workspace and recorded materialisations is
classified:

```text
added      materialised on the candidate only
removed    materialised on the base only
changed    present on both, recorded versions (or snapshot ids) differ
unchanged  present on both, versions equal or table inherited unchanged
absent     in the workspace but materialised on neither side
```

A Nessie branch inherits the base's physical tables, so a dataset visible on
both sides with no candidate record is `unchanged` — its data literally is
the base's. When no state records exist on either side, the adapter's
snapshot/source state (the Iceberg snapshot id) decides instead.

The report also carries:

- **schema** — per-model added/removed columns, type changes and nullability
  changes, each with a safety classification;
- **contracts** — per-model contract changes: the workspace's declared
  contract against what the base environment last recorded, classified
  `safe`/`review`/`breaking` (see *Contract diff* below);
- **impacts** — for every breaking schema or contract change, the downstream
  models and tests lineage says it would break;
- **rows** — `count(*)` per side plus the delta;
- **diffs** — with `--full`, a keyed value diff per changed model that
  declares keys, plus an aggregate diff for any changed model that declares
  a diff policy but no key — so a declared `require_keyed_diff` or row
  threshold is evaluated rather than skipped (the same `diff` engine as
  single-model diffs);
- **upstream** — each dataset's model dependencies, the lineage hook for
  branch-aware graph diffs.

Output is deterministic (datasets sorted by name) and identical in content
between human and `--json` output. The report is persisted as branch-diff
evidence in the state store — exported to `.phlo/transform/branch_diff.json`
(including `deep`, whether `--full` value-level diffs ran) — which
`promote --require-diff` consumes. The
evidence only authorises the candidate→target pair it names; it is rejected
when its recorded versions no longer match either side's current
materialisations (stale — including a dataset that materialised on either
ref after the diff), when a diff entry compared a relation to itself, or
when it was produced without `--full` — a required data-diff audit must
evaluate value-level policies.

## Model diff

```bash
phlo-transform diff assay.results
phlo-transform diff assay.results --ref feature/x --base main
phlo-transform diff assay.results --base-relation memory.default.assay__results
phlo-transform diff assay.results --full
phlo-transform diff assay.results --sample 0.05 --json
```

`--base-relation` supplies the base physical relation. Otherwise relations
resolve the same way as a branch diff: a recorded materialisation names the
relation the environment actually wrote (and the version that wrote it, for
the audit's staleness check); a `--ref` environment resolves through its
provisioned catalog and the base defaults to `main`. A diff that ends up
comparing a relation to itself is still printable, but
`promote --require-diff` rejects it — it measured nothing. Diff results are
written to `.phlo/transform/diff.json`.

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
added, removed (`full_rebuild_required`), changed (`review` for numeric
widening, `error` otherwise) and nullability (`error` when relaxing to
nullable — it removes a guarantee consumers may rely on — `review` when
tightening to not-null) columns appear alongside the row-level result.

A column rename declared in `phlo.toml` turns an unexplained breaking
removal into an explicit `renamed` entry — still a breaking change for
consumers selecting the old name, so promotion gates on it — and lets the
diff compare the new column against its predecessor's type:

```toml
[model."assay.results".renames]
result_value = "concentration"
```

## Contract diff

`contract_changes` compares each model's declared contract (`contract_hash`
made legible) against the contract the base environment last materialised
with — contracts are persisted on materialisation records, so the report
describes what promotion would actually change rather than a hash that
moved:

```text
contract_added       contract declared where none was recorded   safe
contract_removed     contract dropped                            breaking
enforcement_changed  enforcement toggled                         review (on) / breaking (off)
added                contract column added                       safe
removed              contract column removed                     breaking
renamed              declared rename (new in desired, old in base) breaking
rename_ambiguous     several new names claim one old name          review
type_changed         numeric widening `review`, else             breaking
nullability_changed  relaxed (lost NOT NULL guarantee)           breaking
                     tightened                                   review
key_changed          effective key changed                       breaking
key_removed          effective key dropped                       breaking
key_added            effective key declared                      review
```

The effective key is the union of a `key` incremental strategy's columns
and unique-assertion columns — the same identity concept from either
source — compared against the key the base environment recorded at
materialisation time.

A stale rename declaration — one whose new or old name is absent from the
contracts — resolves nothing: the underlying removal or addition reports
normally. Rename declarations must be one-to-one: when several new names
claim the same old name, none resolve — the old column reports `removed`,
every new column reports `added`, and `rename_ambiguous` explains why. The
`impacts` section resolves every `breaking` entry through the column
lineage graph into the downstream models and tests that consume it.

At promotion time the same comparison runs live (workspace contract against
the target's recorded contract), so a contract edited after `diff` cannot
reach `promote` on the artifact's stale analysis.

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
policy fails the command and blocks promotion. Row thresholds need keyed
coverage to measure anything — on a keyless model (or under `--partition`,
which counts partitions not rows) they fail rather than pass on unmeasured
zeros, and `require_full_diff` likewise fails without a stable key.

## WAP integration

`PromotionRequest` accepts `diff_passed` and `require_diff`. `phlo-transform
promote --require-diff` reads the branch-diff evidence recorded in the
state store (exported to `branch_diff.json`) and requires a passing
value-level diff that covers the exact candidate→target pair being promoted:
evidence naming different refs, produced without `--full`, whose
recorded versions no longer match either side's materialised state, or a
self-comparison is rejected, failing the `data_diff` promotion gate. The
single-model `diff.json` is not promotion evidence — it examined one model
and cannot certify a branch.

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
