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

| Classification | Before fixes | After fixes |
|---|---|---|
| CLEAN models | 5 (35.7%) | 9 (64.3%) |
| REVIEW models | 9 | 5 |
| UNSUPPORTED | 5 macros, 25 exposures (19 metrics + 6 semantic models) | unchanged |

Remaining REVIEWs are all correct: project macro `cents_to_dollars` (3 call
sites), package macro `dbt_utils.generate_surrogate_key`, and
`dbt_date.get_base_dates(...)` in `metricflow_time_spine`. Jinja is
deliberately never executed; these files are emitted broken on purpose so
`check` fails loudly. `translate --verify` exits 1 on this project, as
designed.

Note: the analysis report counts all 6 declared sources; the generated
workspace reports 3 — only sources actually referenced by models are
inferred into the Phlo graph. Intended, but worth knowing when comparing
counts.

### jaffle_shop_duckdb (5 models, 3 seeds)

| | Before fixes | After fixes |
|---|---|---|
| CLEAN models | 1 | 1 |
| REVIEW models | 4 | 4 |

The count is unchanged but the *reasons* are now correct. Before,
`ref('raw_customers')`-style seed references failed with DBT001 "no known
target" and modern `arguments:` test syntax produced false DBT014s. Now seed
refs resolve to the physical relation with an explicit DBT015 note ("load the
CSV into relation `raw_orders` before running"), and the `arguments:` tests
generate real test files. The remaining REVIEWs are honest: `orders.sql`
contains a genuine Jinja `{% for %}` pivot that needs a human rewrite, and
the three seeds need hosting.

## End-to-end execution (jaffle_shop_duckdb → DuckDB)

After translating (`--verify` exits 1 only because of the intentional
`orders.sql` pivot) and loading the three seed CSVs as `raw_*` tables:

- `doctor`, `check`, `plan`, `run`, `test`, `inspect`, `explain`, `lineage`,
  `manifest` — all exit 0.
- 15 tests pass (unique, relationships, accepted_values from `arguments:`
  syntax, not_null directives).
- **State:** after inserting a row into `raw_orders`/`raw_payments`, `plan`
  correctly shows `BUILD staging.stg_orders / stg_payments` ("a source state
  changed") and `BUILD jaffle_shop.customers / orders` ("an upstream model
  version changed"), while `stg_customers` stays `SKIP`. The new row
  propagates. A subsequent no-change run is a full `SKIP`.

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
   CSV as (file stem in the target schema), flagged REVIEW with an
   actionable DBT015 message. `relationships` test targets pointing at seeds
   resolve the same way.
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
  `dbt.date_trunc`.
- `phlo-transform-dbt::seed_refs_arguments_syntax_and_dbt_builtins`.
- `phlo-transform-cli::unqualified_source_appends_trigger_rebuilds_on_duckdb`
  — full lifecycle e2e asserting a source append reclassifies the model
  `BUILD`, not `SKIP`.

## Known limitations (unchanged, documented)

- dbt semantic-layer resources (metrics, semantic models, exposures),
  snapshots, analyses, and disabled models are UNSUPPORTED by design.
- Project and package macros (e.g. `cents_to_dollars`,
  `dbt_utils.generate_surrogate_key`, `dbt_date.*`) are never executed;
  call sites are REVIEW and must be rewritten by hand.
- Seeds have no native representation; `ref()`s to them resolve, but the
  CSV must be loaded into the target relation manually.
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
honest, actionable classifications on a realistic project (64% of
canvas-exemplar's models convert CLEAN; every remaining REVIEW is a real
Jinja construct a human must decide on); the DuckDB lifecycle — plan, run,
test, state-aware re-runs — is verified end-to-end; and compile performance
is comfortable to at least 5,000 models. The version number already says
what it is: an early release with documented gaps, and the docs match the
behaviour observed here.
