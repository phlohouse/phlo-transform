# Release validation — v0.1.0 candidate

Validated `phlohouse/phlo-transform` in a completely fresh container
(`rust:1.93-bookworm`, aarch64, OrbStack). Nothing from the host toolchain was
used: the image runs `cargo install --path crates/phlo-transform-cli --locked`
against a clean copy of the repo and clones the external test projects from
GitHub. Build from scratch: ~5m40s (dominated by the bundled DuckDB C++
compile).

External projects:

- `dbt-labs/canvas-exemplar` — realistic dbt translation stress test
- `dbt-labs/jaffle_shop_duckdb` — DuckDB-compatible end-to-end execution test

## Getting-started path

The documented path works verbatim with no undocumented setup:

```text
phlo-transform init / check / plan --adapter duckdb / run --adapter duckdb
phlo-transform explain example.daily_events / phlo-transform doctor
```

All succeed; `doctor` reports workspace/compile/adapter/state health and
warns (correctly, exit 0) when no adapter is configured for a command that
does not need one.

## Translation coverage

### canvas-exemplar (14 models, 6 sources, 6 macros, 3 packages, 25 exposures)

| Classification | Initial | After fixes | After compat pass |
|---|---|---|---|
| CLEAN models | 5 (35.7%) | 9 (64.3%) | **13 (92.9%)** |
| REVIEW models | 9 | 5 | 1 |
| UNSUPPORTED | 5 macros, 25 exposures (19 metrics + 6 semantic models) | unchanged | 1 macro (`generate_schema_name` — dynamic body), 29 property-only resources (exposures/metrics/semantic models + `unit_tests`/`groups`) |

Compatibility pass (current): the five REVIEW models converted cleanly —
`cents_to_dollars` inlines via `adapter.dispatch` → `default__` variant
(statically selected), `dbt_utils.generate_surrogate_key` lowers to
`md5(concat_ws(…))`, and `dbt_utils.star` lowers to `* exclude (…)` for the
provable argument subset. `metricflow_time_spine` uses
`dbt_date.get_base_dates`, which lowers to a DuckDB-specific
`generate_series` spine only when the source profile declares
`type: duckdb`; canvas ships no `profiles.yml`, so the model stays REVIEW
rather than emit backend-specific SQL for an unknown destination.
Semantic-layer resources remain UNSUPPORTED by design.

Note: the analysis report counts all 6 declared sources; the generated
workspace reports 3 — only sources actually referenced by models are
inferred into the Phlo graph. Intended, but worth knowing when comparing
counts.

### jaffle_shop_duckdb (5 models, 3 seeds)

| | Initial | After fixes | After compat pass |
|---|---|---|---|
| CLEAN models | 1 | 1 | **5 (100%)** |
| REVIEW models | 4 | 4 | 0 |
| CLEAN seeds | 0 | 0 | **3** |

The `orders.sql` pivot (`{% set payment_methods = [...] %}` + `{% for %}` +
`{% if not loop.last %}`) now expands statically at translation time, and
seeds became runnable inputs rather than manual prerequisites.

## End-to-end execution (jaffle_shop_duckdb → DuckDB)

The translated workspace runs verbatim with no manual seed loading: the
three CSVs are copied to `seeds/` and the runner loads them
(`CREATE OR REPLACE TABLE main.raw_* AS SELECT * FROM read_csv_auto(…)`)
before building models.

- `doctor`, `check`, `plan`, `run`, `test`, `inspect`, `explain`, `lineage`,
  `manifest` — all exit 0.
- 3 seeds load; 5 models build; 20 tests pass (unique, relationships,
  accepted_values from `arguments:` syntax, not_null directives).
- **State:** seed CSV content hashes are recorded in `seed_loads`; a changed
  CSV re-plans the seed (and its downstream models, via the `csv:` source
  state) and an unchanged CSV is `SKIP`. Source-table appends still trigger
  rebuilds as before.

## Scaling benchmark (synthetic workspaces, DuckDB `:memory:`)

| Workspace | `check` before | `check` after | `plan` before | `plan` after |
|---|---|---|---|---|
| 100 models, 5 layers | 0.00s | 0.00s | 0.13s | 0.20s |
| 1,000 models, 5 layers | 0.37s | 0.07s | 0.93s | 0.25s |
| 5,000 models, 5 layers | 8.51s | 0.32s | 18.10s | 2.05s |
| 5,000 models, all independent | 15.02s | 0.19s | — | 0.95s |
| 5,000 models, single chain | 3.83s | 0.19s | — | 1.10s |

Peak RSS at 5,000 models: ~135 MB (`check`), ~168 MB (`plan`). `check` is now
effectively linear; `plan` is dominated by per-model `relation_exists`
catalogue probes, which is inherent to a state-aware planner.

## Bugs found and fixed

1. **`ref()` to seeds failed to resolve (DBT001).** Seed names are known
   resources; a ref now resolves to the relation dbt would materialise the
   CSV as (file stem in the target schema). Seeds are native runnable
   inputs — the CSV is copied to `seeds/` and loaded automatically — so
   both the seed and the referencing model classify CLEAN.
   `relationships` test targets pointing at seeds resolve the same way.
2. **dbt `arguments:` test syntax unsupported.** The modern form
   `accepted_values: {arguments: {values: [...]}}` (and `relationships:
   {arguments: {to, field}}`) produced false "lacks a column or values" /
   "target could not be resolved" findings. `arguments` is now merged into
   the test arg map for both model and source tests.
3. **`dbt.date_trunc` unnecessarily unsupported.** The dbt cross-database
   shim `dbt.date_trunc('day', 'col')` now lowers to native
   `date_trunc('day', col)` (the string-literal column argument becomes a
   bare identifier). Other package/project macros still classify REVIEW —
   Jinja is not executed, by design.
4. **DuckDB source state silently broken for unqualified sources.** A bare
   `from raw_events` resolves through DuckDB's search path to
   `main.raw_events`, but enrichment qualified it as `default.raw_events`;
   the resulting `DESCRIBE` error then discarded *all* source-state and
   schema enrichment. Appended source rows were invisible and every plan
   reported `SKIP` forever. Fixes: unqualified sources default to `main`
   under the DuckDB adapter, `collect_source_states` failure no longer
   discards schema enrichment, and `relation_columns` returns empty on a
   missing relation instead of erroring.
5. **Quadratic compile time.** `Resolver` linear-scanned every model up to
   four times per relation reference; duplicate detection and the
   type-analysis loop each re-scanned the model list per model. All are now
   hash/B-tree indexed — 5,000-model `check` went 8.5s → 0.3s.

## Regression coverage added

- `fixtures/dbt-seeds` — seed `ref()`, `arguments:` test syntax,
  `dbt.date_trunc`, and a `get_base_dates` call with no `profiles.yml`
  staying REVIEW.
- `fixtures/dbt-jaffle` — extended with `{% set %}`/`{% for %}` expansion
  (`payments_pivot`), `dbt_utils.generate_surrogate_key` +
  dispatching `cents_to_dollars` (`orders_enriched`),
  `dbt_date.get_base_dates` (`time_spine`), `dbt_utils.star`
  (`package_users`, `package_users_aliased`/`package_users_renamed` →
  REVIEW), grouped `not_constant`, and a `dbt_utils.expression_is_true`
  test.
- `fixtures/dbt-dynamic` — a dynamic `{% for %}` over `run_query` stays
  REVIEW and keeps `--verify` failing.
- `fixtures/native-seeds` — native `seeds/**/*.csv` discovery,
  `[seeds] schema`, content-hash versioning.
- `phlo-transform-dbt::seed_refs_arguments_syntax_and_dbt_builtins`.
- `phlo-transform-engine::seed_loads_before_models_and_skips_when_unchanged`,
  `seed_content_change_replans_the_load`,
  `failed_seed_load_blocks_dependent_models`, and
  `seed_tests_pull_the_seed_into_the_plan` — plan/apply/state over a
  fake adapter.
- `phlo-transform-core::csv_seeds_are_discovered_and_compiled`.
- `phlo-transform-cli::unqualified_source_appends_trigger_rebuilds_on_duckdb`
  — full lifecycle e2e asserting a source append reclassifies the model
  `BUILD`, not `SKIP`.

## Known limitations (documented)

- dbt semantic-layer resources (metrics, semantic models, exposures),
  snapshots, analyses, `unit_tests`/`groups`, and disabled models are
  UNSUPPORTED by design.
- Dynamic macros and Jinja constructs (`{% set %}` capture blocks, `{% for %}`
  over non-literals, `run_query`, `{% call %}`, macro bodies containing
  `{% %}` statements, e.g. `generate_schema_name`) are never executed;
  call sites are REVIEW and must be rewritten by hand.
- Seeds translate to native CSV inputs (DuckDB `load_csv`); adapters
  without a `load_csv` implementation report them UNSUPPORTED.
- DuckDB `source_state` fingerprints schema + row count: in-place updates
  that preserve the row count remain invisible (Iceberg snapshot IDs are the
  precise mechanism).
- `inspect`/`explain` without `--adapter` compile offline and can report a
  spurious `changed` status; pass `--adapter` for catalogue-enriched state.
- The Trino/Nessie path (WAP, promotion, diffs) was not exercised live in
  this validation — it has dedicated `--ignored` E2E tests and CI coverage.

## Verdict

**Ready for v0.1.0.** The documented install and getting-started path works
in a clean container with no undocumented steps; the dbt translator produces
honest, actionable classifications on realistic projects (all
jaffle_shop_duckdb models and 13/14 canvas-exemplar models convert CLEAN;
every remaining REVIEW/UNSUPPORTED is a real dynamic, backend-ambiguous, or
semantic-layer construct a human must decide on); the DuckDB lifecycle — seeds, plan, run, test,
state-aware re-runs — is verified end-to-end; and compile performance is
comfortable to at least 5,000 models. The version number already says
what it is: an early release with documented gaps, and the docs match the
behaviour observed here.
