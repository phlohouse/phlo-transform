# Phlo Transform implementation roadmap

This folder breaks [`SPEC.md`](../../SPEC.md) into implementation phases.

The roadmap is ordered by architectural dependency rather than by marketing release. Each phase should leave the repository in a usable and testable state and should not introduce abstractions that are only needed by later phases.

## Delivery order

| Phase | Outcome | Depends on |
|---|---|---|
| [0 — Compiler spike](00-compiler-spike.md) | Prove workspace discovery, SQL parsing, relation resolution and DAG construction | — |
| [1 — MVP build engine](01-mvp-build-engine.md) | Execute table/view DAGs through Trino with plans, tests and persisted run state | 0 |
| [2 — Typed compiler and lineage](02-typed-compiler-lineage.md) | Resolve columns/types and derive column lineage, contracts and impact | 1 |
| [3 — State-aware execution](03-state-aware-execution.md) | Content-address model versions and rebuild only what is required | 2 |
| [4 — Incremental models](04-incremental-models.md) | Add declarative append/key/partition/window incremental strategies | 3 |
| [5 — Nessie and WAP](05-nessie-wap.md) | Branch-native environments, Write-Audit-Publish, promotion and rollback | 4 |
| [6 — Data diff](06-data-diff.md) | Compare candidate and published data and use diffs as gates | 5 |
| [7 — Workflow integration](07-workflow-integration.md) | Merge transform DAGs into wider Phlo workflow/data lineage | 2, 5 |
| [8 — Daemon and agent APIs](08-daemon-agent-apis.md) | Incremental compiler service, editor/UI integration and agent-native queries | 2, 3 |

## Parallel roadmap

| Track | Outcome | Relationship to core roadmap |
|---|---|---|
| [dbt migration and translation](dbt-migration.md) | Analyse dbt projects and emit the simplest equivalent native Phlo Transform project | Design the frontend/semantic boundary in Phase 0; implementation can begin after Phase 1 and improve after Phase 2 |

The dbt translator is deliberately **not** part of the critical path. It is a one-way migration frontend, not a dbt compatibility runtime. The core engine must remain free of dbt-specific semantics.

## Implementation status

Audited against code and tests (not commit messages). Legend: **done**,
**partial** (works but with documented gaps), **missing**, **deviated**
(implemented differently from the plan).

| Phase | Status | Verified coverage / gaps |
|---|---|---|
| 0 compiler spike | done | discovery, IDs, `sqlparser-rs`, CTE-aware relations, resolver, DAG, diagnostics, CLI JSON; parse errors carry file but not spans |
| 1 MVP engine | partial | Trino adapter + plan/apply/run/test, scheduler, state, artifacts, cancellation. Gaps: partition-level scheduler tests absent; retries not automated |
| 2 typed compiler/lineage | partial | semantic IR, resolver, type inference, contracts, generated tests, lineage/impact; nested `array`/`map`/`row` types parsed; `lineage --upstream/--downstream` filter direction; `impact` accepts model or column and renders consumers. Gaps: offline column lineage needs catalogue schemas (model-level impact works offline); unsupported SQL is `Unknown` |
| 3 state-aware execution | partial | versions, skip/build/cached, stale plans. Source states are wired: `Adapter::source_state` reads Iceberg snapshots (schema-fingerprint fallback) and CLI plan/apply/run enrich compilation with them; cached reuse covered by test. Gaps: non-Iceberg sources use schema rather than data state; no cache *reuse* execution (classification only) |
| 4 incremental models | partial | `@incremental` strategies; append/merge; partition replaces touched partitions; time-window uses a committed watermark with typed predicate and applied overlap; schema classification forces full rebuilds; Trino `MERGE` verified live on Iceberg. Gaps: incremental partition replacement uses a column-list delete (no metadata pruning); no schema/watermark test against every adapter |
| 5 Nessie + WAP | partial | Nessie client boundary (REST v2, live-verified), `promote`/`rollback`/promotion artifact. `apply --ref <candidate> --from <base>` auto-creates the branch and a branch-scoped Trino catalog and writes candidate data; tests, an optional diff gate and a breaking-schema gate are enforced at promotion; `--cleanup` removes the candidate; live Nessie + Trino/Iceberg E2E covers isolation, diff, promotion and stale rejection. Gaps: no Iceberg snapshot in the promotion record, no automatic rebase of an advanced target; Nessie Iceberg catalog has no views |
| 6 data diff | partial | keyed diff with per-column counts and configured policies/tolerances; real `TABLESAMPLE` sampling; partition comparison via Iceberg `$partitions` metadata (row-count fallback); populated schema diffs; stale-diff invalidation at promotion; live Trino coverage for keyed/tolerance/partition/sampled. Gaps: no statistical distribution summaries; no example-value redaction |
| 7 workflow integration | partial | ownership, typed graph with `quality_gate`, cross-workflow policy, registered consumers. Missing: host workflow tasks/graph, transform-group invocation API, run correlation, gating API, e2e |
| 8 daemon + agent APIs | partial | versioned local HTTP API, coherent snapshot, watcher/reload, API tests. Missing: plan endpoint, targeted invalidation (full reload), benchmark, push diagnostics |
| dbt migration | missing | Frontend-agnostic `SemanticProject` boundary exists (Phase 0); no importer or `translate` command implemented |

M0 and M1 are met. M2–M4 are met only at the documented partial level above.

## Milestones

### M0 — Architecture proven

Phases 0 complete.

We can discover a multi-root workspace and derive a valid graph from ordinary SQL relations without `ref()`.

The compiler also has a clean semantic boundary that can be targeted by non-native project frontends such as the dbt translator without adding dbt concepts to the core model.

### M1 — Useful transform runner

Phases 0–1 complete.

A developer can run `check`, `plan`, `apply`, `run`, `test`, `inspect` and `list` against Trino using table/view models.

At this point, initial dbt translation work may begin in parallel because the native project/config/materialisation surface is sufficiently stable to emit against.

### M2 — Compiler differentiator

Phases 2–3 complete.

The engine understands columns and types, can explain lineage and impact, and uses content-addressed state to avoid unnecessary work.

The dbt translator can now improve conversion of contracts, tests, types and more complex model metadata against the typed semantic model.

### M3 — Production-capable lakehouse engine

Phases 4–6 complete.

Incremental models, Nessie branches, WAP, promotion, rollback and data diffs are available.

### M4 — Phlo-native platform component

Phases 7–8 complete.

Transforms participate in the wider Phlo workflow graph and the compiler becomes a reusable service for UI, CI, editors and agents.

## Cross-cutting rules

These rules apply to every phase:

1. **Do not add configuration if the compiler can infer the same information safely.**
2. **Do not make `ref()` the dependency primitive.** Ordinary SQL relation names remain the native path.
3. **Do not introduce arbitrary runtime macros or Python execution.**
4. **Every human-facing semantic result must have a structured representation.**
5. **Compilation must remain side-effect free.** Warehouse mutation only occurs in execution/apply paths.
6. **Ambiguity is an error.** Never silently pick a model/source candidate.
7. **Preserve stable logical model identity independently of physical paths.**
8. **Prefer semantic/AST hashes to raw text hashes.** Formatting-only changes should not rebuild data once canonicalisation exists.
9. **Keep Trino first.** Add adapters only after the adapter contract is proven by a real second implementation.
10. **Keep crates coarse until boundaries are demonstrated by implementation.**
11. **Keep project frontends separate from the semantic core.** Native Phlo discovery and future importers such as dbt must lower into the same compiler representation without adding source-system-specific fields to core types.
12. **Migration tools translate intent, not syntax.** A dbt feature should become the smallest equivalent native Phlo construct rather than a recreated dbt abstraction.

## Suggested repository shape during early development

Avoid creating every crate listed in the long-term spec immediately. Start with something closer to:

```text
crates/
├── phlo-transform-core/
├── phlo-transform-sql/
├── phlo-transform-trino/
└── phlo-transform-cli/
```

Split additional crates only when ownership and dependency direction are clear.

An importer crate such as `phlo-transform-dbt` should be added only when implementation begins; do not create empty abstraction crates during the compiler spike.

## Definition of done for a phase

A roadmap phase is complete only when:

- its acceptance criteria pass in CI;
- public behaviour is covered by integration tests;
- relevant CLI commands support JSON output;
- errors have stable typed categories/codes where appropriate;
- documentation/examples match actual behaviour;
- no later-phase placeholder architecture is required for the completed feature to work.
