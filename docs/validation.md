# Validation — dbt migration end-to-end

This documents a usability audit of the migration workflow, run against three
fixture projects:

| Fixture | Shape |
|---|---|
| `fixtures/dbt-clean` | Minimal project: one source, staging view, table mart, `not_null` test. Translates 100% CLEAN. |
| `fixtures/dbt-shop` | Realistic DuckDB-compatible project: sources, staging views, a table mart, a keyed (`merge`) incremental, a watermark (`append` + `is_incremental()`) incremental, folder-level `+materialized`/`+tags`, schema tests, `var()`. Translates 100% CLEAN. |
| `fixtures/dbt-jaffle` | Jaffle-style project exercising the REVIEW/UNSUPPORTED paths: package macros (`dbt_utils.star`), a project macro, an `ephemeral` model, an `is_incremental()` else-branch, a seed, a snapshot, a singular test, `packages.yml`. |

## Workflow exercised

```text
phlo-transform -r <dbt-project> translate --from dbt --check
phlo-transform -r <dbt-project> translate --from dbt --out <dir> --verify
# seed source tables in a DuckDB file
phlo-transform -r <dir> doctor / check / list / plan / run / test
phlo-transform -r <dir> explain / inspect / lineage / manifest
# mutate sources, re-run, confirm incremental behaviour and no-op third run
```

## What worked

- `translate --check` classification is accurate and honest: macro call sites
  and the ephemeral model are REVIEW, the snapshot is UNSUPPORTED, everything
  else is CLEAN. Generated SQL is minimal — `{{ config }}` blocks become
  directives, `ref`/`source`/`var` resolve to names/literals, and no
  `phlo.toml` is emitted when the project needs no defaults.
- `--verify` compiles the generated workspace and reports real diagnostics.
- Residual Jinja in REVIEW models fails loudly at `check` time with a parse
  error pointing at the generated file, as designed.
- `dbt-shop` translated, compiled, planned, ran and tested on DuckDB with no
  manual edits. The keyed incremental merged an updated row; the
  time-window incremental appended only rows past its watermark; a third run
  with no upstream change was a full SKIP.
- `doctor` reports workspace/compile/adapter/state/nessie health clearly and exits
  non-zero on problems.
- Generated tests (`unique`, `not_null`, `accepted_values`, key-folded
  assertions) all executed and passed.

## Problems found and fixed

1. **`translate --verify` exited 0 on a failing generated workspace.** The
   command printed `check failed` with diagnostics but always returned
   success, so CI could not detect REVIEW models that do not compile. It now
   exits non-zero when the verify report is not ok. Files are still written —
   REVIEW output is intentionally inspectable.
2. **`inspect` always reported `status: changed`.** The desired version in the
   report is the *short* hash while the recorded materialised version was
   compared by *full* hash, so even a just-built model showed `changed`. The
   comparison now uses the short hash on both sides.
3. **DuckDB `source_state` only fingerprinted the schema.** Appended source
   rows were invisible to the planner, so incremental models — the main reason
   to run incrementally — were always SKIP on DuckDB. The fingerprint now
   includes `count(*)`, which is metadata-cheap on DuckDB and catches the
   dominant append case.
4. **DuckDB timestamp/time values were rendered as Rust debug strings.**
   `max(<timestamp>)` returned e.g. `Microsecond 1704272400000000`, which broke
   the time-window watermark predicate (`CAST('Microsecond ...' AS timestamp)`
   is invalid SQL) and corrupted any test/diff output containing timestamps.
   `ValueRef::Timestamp`/`Time64` now render as `YYYY-MM-DD HH:MM:SS.ffffff`
   / `HH:MM:SS.ffffff` literals.

## Coverage added

- `fixtures/dbt-shop` — the third fixture above.
- `translate_verify_failure_exits_nonzero` — regression test for fix 1.
- `translated_dbt_project_runs_on_duckdb` — full lifecycle e2e: translate,
  `--verify`, seed DuckDB, `run`, mutate sources, `run` again (asserting
  merge/window row counts), no-change `plan` is SKIP, `inspect` reports
  `unchanged`.

## Remaining limitations

- REVIEW models with residual Jinja fail `check` with a generic parse error;
  the diagnostic does not mention that the `{{ ... }}` is untranslated Jinja.
- DuckDB `source_state` (schema + row count) still cannot see in-place updates
  that preserve row count; Iceberg snapshot ids remain the only precise source
  state.
- `inspect`/`explain` computed offline (no `--adapter`) cannot observe source
  state, so a model reading external sources derives a different desired
  version than the enriched one `run` uses; pass `--adapter` for accurate
  state. (The catalog no longer contributes — it is an environment binding,
  not a version input.)
- `dbt_utils.star`-style macro call sites are preserved verbatim and flagged
  REVIEW — they require manual rewrite, as documented.
