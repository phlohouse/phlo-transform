# Convergence review

A repository-wide hardening pass over `phlo-transform` after the roadmap
implementation: one audit of whether the compiler, state, execution,
lakehouse environments, promotion workflow, CLI and machine API all agree
about the same underlying model — and the fixes where they did not.

## The model, in one paragraph

A *logical environment* (`--environment`/`--ref`, aliases that must agree)
labels execution and scopes state. On a Nessie deployment it maps to a
candidate branch plus a branch-scoped physical catalog; without Nessie it is
an honest state-scoping label and nothing more. `--from` names the base ref
a candidate is cut from. `--catalog` pins the physical target; the generated
candidate catalog name is `phlo_<sanitised-ref>_<8-hex-sha256>`.
`--trino-catalog` is unrelated — it is the session catalog the connection
opens. `main` and the unlabeled default share the deployment catalog and
fold each other's state records. Promotion is authorised only by evidence
bound to the exact candidate and target commits being merged.

## What diverged, and what changed

### Environments: fabricated evidence closed

The CLI used to record `--environment ci/foo` on a run while executing
against the *default* catalog — a labelled run bound to a branch head it
never wrote, which later promotion would have treated as candidate evidence.
Now CLI and daemon share `EnvironmentContext::resolve`:

- `run`/`apply` resolve in `Ensure` mode — provision the branch and catalog,
  then compile retargeted.
- `plan`/`test` resolve in `ReadOnly` mode — the same catalog precedence
  (explicit override → recorded binding → generated name) and the same
  retargeted compile, but nothing is created and no evidence is written.
  A `plan(environment=X)` names exactly the targets `run(environment=X)`
  executes.
- `ReadOnly` also applies `Ensure`'s provisioning requirement: when
  resolution lands on the generated catalog and the adapter cannot
  provision catalogs (`supports_catalog_provisioning`), the preview fails
  closed too — it never names a target a run would refuse. An explicit or
  recorded user-managed pin is the escape hatch.
- Nessie present but isolation unprovisionable (no adapter or no
  catalog-facing URI) fails closed with `NotConfigured`/`API007` — the run
  never proceeds on the default target while claiming a candidate.
- Nessie absent entirely: the environment is a state label and the CLI
  prints a note saying so, rather than implying isolation that does not
  exist.
- `--environment` and `--ref` given with different values is an error.
- `resume`/`retry_failed` infer the environment from the stored run — no
  more needing to repeat `--ref` — and still refuse a flag that disagrees.

### Orchestration: one implementation, two surfaces

Promotion, candidate cleanup, run-reference binding, run lookup and test
execution were implemented twice — CLI and daemon had parallel copies that
had already drifted. They now live once in `phlo-transform-engine`:

- `evaluate_promotion` — gathers candidate/target refs, the env-scoped run
  and its model/seed/test records, the provisioning artifact, the hash-bound
  branch diff, live contract breaks, merge check and lineage standing, then
  evaluates the gates. `promotion.request()` pins the target hash at merge
  so a racing commit is rejected by Nessie.
- `persist_promotion`, `cleanup_candidate`, `bind_run_reference`,
  `find_unique_run` (distinguishing no-match from ambiguous), and
  `execute_tests` are shared by both surfaces.

Net effect: ~700 lines of duplicated orchestration removed; the CLI's
`promote` and the daemon's promote operation authorise a merge under
literally identical rules.

### Catalog ownership

Provisioning records how each catalog was established
(`created`/`unverified`/`unmanaged`) plus whether Phlo provably owns it.
A pre-existing catalog is accepted only on a recorded binding — the
generated name is a public convention anyone can mint, so it proves
nothing on its own, and a `ref create` intent record cannot vouch for a
catalog that appeared later. Nor can a legacy `unverified` record: the
`catalog_owned_by_phlo` flag was introduced with the vetting rule, so a
record lacking it may have been adopted on the name alone and is not
binding evidence. A catalog another candidate claims is refused; an
`unmanaged` generated name fails closed; a refused `unverified` catalog
rolls back a just-created branch so a failed provisioning attempt leaves
nothing behind. `cleanup_candidate` drops only
catalogs Phlo owns — including legacy `created` records predating the
ownership flag — and reports every failure rather than leaving silent
leftovers. Environment artifacts are removed only after the branch and
catalog are both gone.

### Two Nessie addresses

`--nessie-endpoint`/`PHLO_NESSIE_ENDPOINT` is the REST endpoint this process
calls. `--nessie-catalog-uri`/`PHLO_NESSIE_CATALOG_URI` (new) is the address
written into provisioned catalogs — what Trino's Iceberg connector dials.
They default to the same value; the split exists because Trino in a
container cannot reach `http://127.0.0.1:<host-mapped-port>`. The daemon's
`ServiceConfig.nessie_uri` already modelled this; the CLI now passes the
catalog URI to provisioning instead of conflating the two.

### Planner performance: measured, then fixed

A committed scaling measurement (`plan_scaling_adapter_metadata`, ignored by
default) established the baseline with 5ms simulated metadata latency:

| models | scenario | before | after |
|---:|---|---:|---:|
| 100 | cold | 0.7s | 16ms |
| 100 | warm | 1.4s | 63ms |
| 1,000 | cold | 7.6s | 56ms |
| 1,000 | warm | 14.8s | 515ms |
| 5,000 | cold | 45s | 305ms |
| 5,000 | warm | 82s | 2.6s |

Three changes produced this, in order of leverage:

1. **Batched metadata.** `Adapter::relations_exist` and
   `relation_columns_many` batch relation probes through
   `information_schema` (Trino groups by catalog/schema; the trait defaults
   keep other adapters serial). One call replaces N `SELECT 1 … LIMIT 0`
   round trips. A missing relation in `relation_columns_many` reports `Err`
   rather than an empty schema — an empty schema would misclassify every
   column as added.
2. **Bounded parallelism.** `output_identity` (a per-table `$snapshots`
   read that cannot batch) is prefetched with `buffered(16)` before the
   decision loop. `decide` is now synchronous over `ModelEvidence` — all
   I/O happens in one prefetch pass.
3. **Killed the O(N²) CPU work.** `Selection::get` scanned `members`
   linearly with a `logical_name()` allocation per comparison, called per
   model — ~7.7s of pure CPU at 5k models. The planner builds a
   `BTreeMap<&str, &SelectedModel>` once. The per-model "required by" BFS
   became one multi-source BFS (`requiring_models`).

State reads were bulked at the same time: `materialized_in`, `seeds_in` and
the new `StateStore::materialized_by_hashes` (real implementations for
SQLite and Postgres) replace per-model queries, and the default↔`main`
record fold lives in shared `materialized_scope`/`seeds_scope` helpers so
the planner, branch diff and CLI/daemon all scope identically.

### Golden path, proven end-to-end

`crates/phlo-transform-cli/tests/golden_path.rs` drives the real binary
through the full lifecycle against testcontainers Trino + Nessie: workspace
init → candidate provisioning → isolated run → branch diff → gate
evaluation → promotion → branch and catalog cleanup. It runs in CI
alongside the existing container tests. Stale-evidence rejection — a moved
candidate or target head, an unbound run — is covered by the engine's audit
tests rather than this path. The golden path's first real run is what
surfaced the Nessie endpoint/catalog-URI conflation.

### Product-surface reconciliation

- `doctor` gained a Nessie check: endpoint reachability plus the
  catalog-URI the warehouse would be provisioned with, when it differs.
- Flag help now describes one model consistently across every subcommand.
- Docs audited and reconciled: `engine.md` no longer defers implemented
  phases (its Deferred section listed contracts, incremental, WAP, diff and
  the daemon — all shipped); `README.md`/`roadmap/README.md` Phase 8 no
  longer claims a missing plan endpoint; `wap.md`/`SPEC.md` describe
  read-only `plan`/`test` resolution, the `--environment`/`--ref` alias
  rule and `--nessie-catalog-uri`; `architecture.md` acknowledges the
  post-Phase-0 crates.

## Evidence and provenance model

Promotion evidence chains four sources: the live Nessie refs, the state
store's env-scoped run/model/seed/test records, the store's immutable
audit-evidence records (environment bindings, branch diffs, lineage diffs —
exported to `environment*.json`, `branch_diff.json`, `lineage_diff.json`),
and live-computed contract breaks. Everything is hash-bound: the diff
evidence records both refs' resolved heads, the run binds to the
candidate's post-run head, and `promote` re-checks both hashes at merge
time. Evidence in a shared state backend is portable across CI stages; a
store read failure fails closed rather than substituting local files that
could disagree with what another stage recorded. Unknown provenance fails
closed — a branch Phlo did not create has no recorded base, and "no
evidence" never reads as "no changes".

## Crate boundaries

The layout holds: `cli → {trino, duckdb, daemon} → engine → {core,
openlineage, nessie} → core → sql`. `cli` and `daemon` are composition
roots; the adapters implement the engine's trait and nothing reaches past
it; `dbt` lowers into `SemanticProject` without touching execution; `nessie`
is a leaf client. No splits were needed — the fix was moving orchestration
*into* the engine, not drawing new crate lines.

## Remaining debt

- **`output_identity` is inherently per-relation.** Warm plans at 5k models
  still spend ~2.6s on 5000 metadata reads at 16-way concurrency — correct,
  but a ceiling.
- **`cached` is classification only** — cross-environment reuse is planned
  and evidenced, not executed as a data copy.
- **Daemon**: reload is a full recompile (no targeted invalidation);
  progress is polled, not pushed.
- **`warm` cost asymmetry**: `relation_exists` batches but
  `source_state`/`output_identity` do not — a non-Iceberg catalog offering
  bulk metadata could narrow this further.

## Maturity

Levels: **Mature** — hardened *and* operationally complete; nothing here
is there yet. **Strong** — failure modes handled deliberately and covered
by tests; fails closed where safety demands. **Production-credible** —
works end to end with documented gaps. **Needs hardening** — the happy
path works; failure modes unexplored. **Experimental** — a stub or
sketch, present but not exercised.

| area | level | evidence / remaining gap |
|---|---|---|
| compiler + diagnostics | Strong | typed IR, deterministic DAG, stable IDs, contracts; unsupported SQL degrades to `Unknown` honestly rather than guessing |
| dbt translator | Production-credible | 100% of jaffle_shop/canvas-exemplar convert CLEAN; dynamic Jinja/macros, exposures, metrics are REVIEW/UNSUPPORTED by design |
| lineage + impact | Production-credible | canonical graph, OpenLineage export, `--diff` across refs; offline column lineage needs catalog schemas; unsupported SQL → `Unknown` |
| planner + state/cache | Strong | batched warehouse+state evidence, synchronous `decide`, cache reuse requires same-relation + same-adapter + strong output identity; `cached` still classifies — reuse is not executed |
| execution runner | Strong | bounded concurrency, `--retries` backoff on retryable adapter failures, timeouts, cancellation, `--resume`/`--retry-failed`; DuckDB has no remote cancel |
| incremental models | Production-credible | append/merge/partition/window strategies, watermarks, schema-change rebuilds; Trino `MERGE` verified live; partition replace is a column-list delete, and watermark coverage is not per-adapter |
| state store | Strong | env-scoped records, `main`↔default fold, SQLite/Postgres parity tested; watermarks deliberately do not fold |
| environments + provisioning | Strong | one resolve path, fail-closed, recorded bindings, ownership-aware cleanup, branch rollback on rejected catalogs |
| WAP + promotion | Strong | hash-bound evidence, provenance-required base, merge-time hash recheck, gates fail closed, live golden-path E2E; evidence is state-store backed and portable across machines via Postgres |
| data diff | Production-credible | keyed/tolerance/partition/sampled diff live-tested; no distribution summaries, no example-value redaction |
| daemon API | Production-credible | versioned ops API, idempotency, cancellation, coherent snapshots; reload is a full recompile and progress is polled |
| CLI | Strong | every surface shares engine orchestration; JSON output everywhere; `--environment`/`--ref` agreement enforced |
| workflow integration | Needs hardening | ownership + `quality_gate` exist; host workflow tasks, run correlation and gating APIs are Phase 7 debt |

The model now holds together: one environment resolution path, one
promotion audit, one ownership story, one state scope — and every surface
(CLI, daemon, plan preview, continuation runs, diff, promote, cleanup)
reads from it identically. The fail-closed posture is tested rather than
asserted: unprovisionable environments error, unverifiable catalogs are
refused, stale evidence blocks, and the golden path proves the whole loop
on real infrastructure.

## Top priorities from here

1. Cache-reuse *execution* — `cached` currently classifies; actually
   reusing the materialised output closes the loop.
2. Targeted daemon invalidation — reload by changed paths rather than full
   recompile.
3. Bulk source-state/output-identity reads where the catalog allows.
4. `plan` output — surface the resolved physical catalog once at the top
   (it is currently only per-model in each target row).
