# Phlo Transform

**Status:** Proposed  
**Language:** Rust  
**Primary execution target:** Trino + Iceberg + Nessie  
**Secondary targets:** DuckDB, PostgreSQL  
**Primary host:** Phlo  
**CLI namespace:** `phlo transform`

## 1. Executive summary

Phlo Transform is a workspace-native SQL transformation compiler and execution engine.

It provides the useful core capabilities associated with tools such as dbt while deliberately avoiding much of their configuration surface, runtime templating complexity, project isolation, adapter abstraction complexity, and legacy compatibility requirements.

Phlo Transform is designed around five ideas:

1. **SQL should remain SQL.**
2. **The compiler should infer what it can.**
3. **The whole repository is one workspace.**
4. **Transforms are compiled into a typed dependency graph before execution.**
5. **Changes are planned, audited and promoted rather than blindly executed.**

A minimal model should therefore be valid as:

```sql
select
    experiment_id,
    sample_id,
    result
from assay.raw_results
```

If `assay.raw_results` is another model in the workspace, Phlo Transform automatically creates the dependency.

No `ref()` is required. No model YAML is required. No source declaration is required. No explicit DAG is required. No schema declaration is required unless the user wants a contract stricter than the inferred schema.

The same compiler representation powers:

- model execution;
- dependency resolution;
- model selection;
- type checking;
- schema validation;
- column lineage;
- impact analysis;
- incremental execution;
- state comparison;
- data diffing;
- workflow integration;
- AI/agent interfaces;
- documentation;
- auditability.

Phlo Transform is therefore not intended to be dbt rewritten in Rust. It is intended to be **a Rust-native SQL build system for modern data workspaces**.

---

## 2. Goals

Phlo Transform must:

- execute SQL transformation DAGs;
- discover transforms across multiple folders;
- support transforms local to workflows;
- construct one workspace-wide dependency graph;
- understand SQL structurally rather than primarily as text;
- infer dependencies directly from SQL;
- infer external sources where possible;
- minimise configuration;
- support table, view and incremental materialisations;
- support static and runtime data-quality rules;
- support schema contracts;
- support state-aware execution;
- detect changes before execution;
- support plan/apply semantics;
- integrate natively with Nessie branches;
- support Write-Audit-Publish workflows;
- provide data and schema diffs;
- expose model and column lineage;
- expose machine-readable APIs for agents and UIs;
- integrate transformation graphs into wider Phlo workflow graphs;
- be embeddable as a Rust library;
- have predictable, deterministic behaviour;
- produce reproducible execution artifacts.

## 3. Non-goals

Initial versions will not attempt to provide:

- complete dbt compatibility;
- arbitrary dbt macros;
- arbitrary Jinja execution;
- arbitrary Python execution inside model definitions;
- dbt package compatibility;
- every database adapter;
- semantic-layer functionality;
- BI metrics definitions;
- dashboard authoring;
- notebook functionality;
- orchestration of arbitrary non-transform workloads;
- hundreds of configuration precedence behaviours;
- adapter macro dispatch;
- user-defined materialisation code;
- Python models;
- dbt exposures;
- dbt snapshots;
- dbt seed compatibility;
- a complete dbt-style documentation website.

These should be introduced later only where there is a clear need.

---

## 4. Design principles

### 4.1 Convention over configuration

A valid transformation should usually require only a `.sql` file. Configuration exists to express exceptions, not basic operation.

### 4.2 One concept, one configuration mechanism

Avoid situations where the same setting can be specified through several unrelated configuration paths. Configuration precedence must remain intentionally small.

### 4.3 SQL-first

The majority of transformation files should contain valid SQL.

Preferred:

```sql
select *
from assay.results
```

rather than:

```sql
select *
from {{ ref("results") }}
```

### 4.4 Static analysis before execution

The compiler should understand the project before running it. Where feasible, errors should be caught at compile or plan time rather than at warehouse execution time.

### 4.5 Workspace-first

A repository is one transformation workspace. Transforms may live anywhere within defined transform roots. Workflows can own local transform trees without becoming isolated projects.

### 4.6 Explicit ambiguity

Phlo Transform must never resolve ambiguous references silently. Ambiguous names are compilation errors.

### 4.7 State is fundamental

Previous execution state is part of the execution model rather than an optional optimisation layer.

### 4.8 Plan before apply

Any potentially meaningful change should be inspectable before execution or promotion.

### 4.9 Immutable identity

Logical model identity must not depend entirely on filesystem location.

### 4.10 Structured interfaces

Anything the CLI can explain should also be available in structured form. Agents and UIs should never have to scrape human-readable terminal output.

### 4.11 Infer first

Every feature should first ask whether the compiler can infer the required information safely. Only require configuration when inference would be unreliable or ambiguous.

---

## 5. Core concepts

### Workspace

A repository or logical project containing transform roots and optionally workflows.

### Transform root

A directory containing transformation models, for example:

```text
transforms/shared/
workflows/assay_ingest/transforms/
workflows/manufacturing/transforms/
```

### Namespace

A logical grouping used for model identity and reference resolution, for example `assay_ingest`.

### Model

A SQL transformation producing a logical dataset.

### Source

A relation referenced by models but not produced within the current workspace.

### Model version

An immutable representation of a model at a particular code, configuration and dependency state.

### Materialisation

How the logical model is physically represented. Initially: `view`, `table`, and `incremental`.

### Contract

Assertions about model schema and structural behaviour.

### Test

A data-quality assertion evaluated against a model or source.

### Plan

The proposed transition from current state to desired state.

### Apply

Execution of an approved plan.

### Environment

A logical data state. For Iceberg/Nessie deployments this should normally map directly to a Nessie reference.

### Transform graph

Dependency graph between models and sources.

### Workspace graph

Higher-level graph containing workflows, tasks, sources, transformations and outputs.

---

## 6. Workspace layout

Phlo Transform must support multiple transform roots natively.

```text
phlo.toml

transforms/
├── shared/
│   ├── dimensions/
│   │   └── date.sql
│   └── reference/
│       └── sites.sql
│
workflows/
├── assay_ingest/
│   ├── workflow.toml
│   ├── tasks/
│   └── transforms/
│       ├── staging/
│       │   └── raw_results.sql
│       └── marts/
│           └── assay_results.sql
│
├── manufacturing/
│   ├── workflow.toml
│   └── transforms/
│       ├── batches.sql
│       └── deviations.sql
│
└── analytics/
    └── transforms/
        └── monthly_summary.sql
```

All of these belong to one transformation workspace.

## 7. Transform discovery

Default transform-root conventions:

```text
/transforms/**
/workflows/*/transforms/**
```

Additional paths may be configured:

```toml
[transforms]
include = [
    "domains/*/transforms/**"
]

exclude = [
    "**/target/**",
    "**/.phlo/**"
]
```

The default behaviour should be sufficient for ordinary projects.

## 8. Transform-root detection

A transform root may optionally contain `transform.toml`:

```toml
namespace = "assay_ingest"
schema = "assay"
materialized = "view"
```

Where configuration is absent, the namespace is derived by convention. For example:

```text
workflows/assay_ingest/transforms/
```

becomes namespace `assay_ingest`.

---

## 9. Model identity

Filesystem location and model identity must be related but distinct.

Canonical internal URI:

```text
model://assay_ingest/staging/raw_results
```

User-facing notation:

```text
assay_ingest.staging.raw_results
```

Physical location:

```text
workflows/assay_ingest/transforms/staging/raw_results.sql
```

## 10. Stable model IDs

A model may optionally pin its logical identity:

```sql
-- @id assay_ingest.raw_results
```

This allows files to move without losing historical identity and is useful for run history, incremental state, lineage, cached results, renames, audit trails and UI references.

If no explicit ID is provided, the compiler derives one deterministically.

## 11. Namespaces

Given:

```text
workflows/assay_ingest/transforms/staging/raw_results.sql
```

The default identity may be:

```text
assay_ingest.staging.raw_results
```

Given:

```text
transforms/shared/dimensions/date.sql
```

The identity may be:

```text
shared.dimensions.date
```

## 12. Reference resolution

The preferred syntax is ordinary SQL:

```sql
select *
from assay_ingest.staging.raw_results
```

Relations are resolved deterministically:

1. exact fully-qualified workspace model;
2. current namespace;
3. current transform root;
4. globally unique workspace model;
5. declared external source;
6. catalogue relation;
7. compilation failure.

Any ambiguity produces an error.

```text
error: ambiguous relation `results`

Candidates:
  assay_ingest.results
  manufacturing.results

Use a qualified model name.
```

## 13. Optional `ref()` compatibility

For convenience and migration, restricted templating may support:

```sql
{{ ref("assay_ingest.results") }}
```

and:

```sql
{{ source("lims", "samples") }}
```

This is not the preferred native model. The compiler must not depend on `ref()` to construct its graph.

---

## 14. Sources

External relations should normally be inferred.

```sql
select *
from raw_lims.samples
```

If no workspace model produces `raw_lims.samples`, the compiler checks the target catalogue. If the relation exists it is registered as an external source.

```text
source://raw_lims/samples
```

Users may optionally enrich sources:

```toml
[source."raw_lims.samples"]
owner = "Analytical Development"
freshness = "24h"
```

Source declarations are not mandatory merely to use the source.

---

## 15. Configuration hierarchy

Configuration precedence is limited to:

```text
workspace defaults
        ↓
transform root
        ↓
folder
        ↓
model
        ↓
explicit CLI override
```

No additional hidden precedence layers.

## 16. Workspace configuration

Example:

```toml
[transform]
default_materialization = "view"
default_catalog = "phlo"
default_schema = "analytics"

[transform.execution]
max_concurrency = 8

[transform.state]
enabled = true

[transform.discovery]
include = [
    "transforms/**",
    "workflows/*/transforms/**"
]
```

## 17. Folder configuration

```text
transforms/
└── assay/
    ├── transform.toml
    ├── staging/
    └── marts/
```

```toml
materialized = "view"

[folder.marts]
materialized = "table"
```

Nested folder configuration may be supported. Effective configuration must always be inspectable:

```bash
phlo transform inspect assay.results --config
```

## 18. Model metadata syntax

Exceptional model behaviour can be expressed using lightweight header directives:

```sql
-- @table
-- @key experiment_id
-- @not-null sample_id,result
-- @owner analytical-development
-- @tags assay,gold

select ...
```

Equivalent long form:

```sql
-- @materialized table
```

Directives should remain intentionally small and parsed independently of SQL syntax.

## 19. Avoiding a DSL explosion

Model directives should not become a programming language. They should express declarative characteristics such as ID, materialisation, key, incremental strategy, tags, owner, nullability and contract behaviour. Anything requiring arbitrary computation belongs elsewhere.

---

## 20. Compiler architecture

Compilation pipeline:

```text
discover files
      ↓
parse metadata
      ↓
parse SQL AST
      ↓
resolve relations
      ↓
construct graph
      ↓
load catalogue metadata
      ↓
resolve types
      ↓
derive column lineage
      ↓
validate contracts
      ↓
calculate model versions
      ↓
calculate execution plan
```

No warehouse mutation occurs during compilation.

## 21. SQL parser

The compiler must use a real SQL parser. `sqlparser-rs` is the preferred starting point.

Initial dialect focus:

1. Trino;
2. DuckDB;
3. PostgreSQL.

An abstraction should exist for dialect-specific syntax but remain significantly simpler than dbt's adapter macro architecture.

## 22. Typed SQL representation

Raw parser output should be converted into Phlo's own semantic representation:

```text
Parsed SQL AST
     ↓
Resolved AST
     ↓
Typed AST
     ↓
Logical Model
```

For example:

```sql
select
    r.sample_id,
    r.result / s.volume as concentration
from assay.results r
join assay.samples s using (sample_id)
```

produces a semantic representation equivalent to:

```text
Projection
├── sample_id
│   └── assay.results.sample_id
│
└── concentration
    └── Divide
        ├── assay.results.result
        └── assay.samples.volume
```

## 23. Typed DAG

The graph must know more than model-to-model edges.

```text
assay.results.result
        ↓
assay.summary.mean_result
```

This supports model lineage, column lineage, impact analysis, schema-change detection, rename analysis and agent tooling.

## 24. Compile-time validation

Where sufficient metadata exists, compilation should detect:

- missing models;
- missing columns;
- ambiguous columns;
- invalid joins;
- incompatible types;
- invalid casts;
- circular dependencies;
- invalid model directives;
- incompatible materialisation configuration;
- contract violations;
- unresolved sources.

Dynamic SQL may require deferred runtime validation. The compiler must represent uncertainty rather than pretend certainty.

---

## 25. Dependency graph

Each graph node may represent a model, source, test or workflow transform group. Model dependencies are derived primarily from SQL relations.

```text
raw.results
     ↓
assay.clean_results
     ↓
assay.assay_results
     ↓
analytics.summary
```

Topological scheduling follows from this graph. Cycles are compilation errors.

---

## 26. Materialisations

Initial materialisations:

### View

```sql
-- @view
```

Candidate default for lightweight staging models.

### Table

```sql
-- @table
```

Fully materialised relation.

### Ephemeral

```sql
-- @ephemeral
```

The model is never materialised. References to it are inlined into
dependents as a derived-table subquery at compile time — including nested
ephemeral chains — and the model is excluded from the execution plan.
Ephemeral models must be single-statement selects.

### Incremental

```sql
-- @incremental key=experiment_id
```

Only changed data is applied.

## 27. Incremental semantics

Users specify intent rather than warehouse-specific implementation.

Supported strategies:

```text
append
key
partition
time-window
full
```

Examples:

```sql
-- @incremental append
```

```sql
-- @incremental key=experiment_id
```

```sql
-- @incremental partition=run_date
```

```sql
-- @incremental window=updated_at
```

The adapter determines appropriate execution SQL.

## 28. Incremental key behaviour

A declaration such as:

```sql
-- @key experiment_id
```

may contribute to schema contract, uniqueness checks, non-null checks, merge strategy, record identity and data diffing. The same information should not need to be declared repeatedly.

## 29. Incremental safety

The planner must identify dangerous state changes:

```text
assay.results

incremental key changed:
  old: sample_id
  new: experiment_id

Action required:
  FULL REBUILD
```

No silent incremental continuation should occur.

---

## 30. Tests and contracts

Testing is split conceptually into structural contracts and runtime assertions.

Structural contracts include column existence, expected type, nullability, key definitions and schema compatibility.

Runtime assertions include uniqueness, referential integrity, range checks and custom SQL conditions.

## 31. Contract syntax

```sql
-- @key experiment_id
-- @not-null sample_id,result
```

Optional stricter contract:

```toml
[model.assay_results.contract]
enforced = true

[model.assay_results.columns.experiment_id]
type = "VARCHAR"
nullable = false

[model.assay_results.columns.result]
type = "DOUBLE"
nullable = false
```

Column renames are declared alongside the contract so diffs can classify a
removed+added pair as a named rename — a breaking change for consumers
still selecting the old name — rather than an unexplained removal:

```toml
[model.assay_results.renames]
result = "legacy_result"
```

Explicit contract files remain optional.

## 32. Inferred tests

`@key experiment_id` should imply at least:

```text
NOT NULL experiment_id
UNIQUE experiment_id
```

Where a relationship is declared, referential integrity testing should be generated automatically.

## 33. Custom tests

Custom tests may be ordinary SQL returning violating rows:

```text
tests/
└── assay_results_positive.sql
```

```sql
select *
from assay.assay_results
where result < 0
```

Zero rows means pass. This avoids inventing a complex test DSL.

## 34. Source freshness

Optional source metadata:

```toml
[source."lims.samples"]
freshness = "6h"
timestamp_column = "loaded_at"
```

Planner output:

```text
lims.samples
  age: 7h 13m
  required: <6h
  status: STALE
```

Policy determines whether stale data warns, blocks or requires explicit override.

---

## 35. Content-addressed state

Each logical model produces a model-version fingerprint conceptually based on:

```text
hash(
    canonical SQL AST
    + effective configuration
    + compiler version
    + dependency versions
    + source version metadata
    + relevant target semantics
)
```

Example:

```text
assay.assay_results@b74a02e
```

## 36. Logical versus physical state

The engine distinguishes desired model version from currently materialised model version. Execution reconciles the two.

Phlo Transform is therefore closer conceptually to a build system than a SQL script runner.

## 37. State-aware execution

If desired and materialised versions match, skip execution. If model code changes, build. If an upstream dependency changes materially, build. If only irrelevant metadata changes, skip where this can be determined safely.

## 38. Source state

For Iceberg sources, state may include:

- snapshot ID;
- schema ID;
- branch;
- partition metadata.

This makes source change detection substantially more precise than timestamp-only heuristics.

## 39. Build cache

Successful immutable model versions may be cached. Where a compatible desired model version already exists, it may be reused or promoted instead of recalculated.

Cache reuse must account for environment and source-state compatibility. A version-hash match in another environment authorises reuse only when the record also names the same physical target relation, was produced by the same adapter, and carries a strong output identity (the adapter's post-write `output_identity` — e.g. the Iceberg snapshot id) that still matches what the relation reports — the hash says *what* should exist, not *where* it was written, *which engine* wrote it, or *that it is still there*. `output_identity` must prove physical contents: a schema fingerprint or row count describes shape, not bytes, so adapters that cannot prove identity (non-Iceberg relations, DuckDB) report none and never authorise reuse. Same-version records that fail any check produce a `cache_miss` build reason; a current-environment record produced by a different adapter or predating adapter tracking produces `adapter_change`, and one whose recorded output identity no longer matches the physical relation produces `output_drift`.

Materialisation records are ordered writes, not last-writer-wins: `model_versions` and `seed_loads` only apply records that are not older than what is stored, and watermarks order by run generation, so two runs completing out of order cannot regress shared state.

---

## 40. `plan`

Planning is a core command:

```bash
phlo transform plan
```

Every decided model carries at least one structured reason; the human
output prints each reason's detail under the model:

```text
Plan:  4f6c…
Adapter: trino
Environment: feature/new-assay
Selection: assay.results+ — excluding assay.legacy_raw

Models (4) — 3 build, 1 skip, 0 cached
  SKIP   assay.reference             [view]  iceberg.assay.reference
           SQL, config, contract and inputs unchanged
  BUILD  assay.clean_results         [table] iceberg.assay.clean_results
           SQL semantics changed
           source raw.lims changed (snap:81f2… → snap:9c40…)
  BUILD  assay.assay_results         [table] iceberg.assay.assay_results
           upstream version changed: assay.clean_results
           upstream assay.clean_results will rebuild in this plan
           strategy: key
  BUILD  analytics.monthly_summary   [table] iceberg.analytics.monthly_summary
           selected by `assay.results+`
           upstream assay.assay_results will rebuild in this plan

Tests (18)
  …
```

Reason kinds are stable machine-readable codes (`sql_semantic_change`,
`config_change`, `contract_change`, `dependency_change`, `upstream_rebuild`,
`source_change`, `target_change`, `compiler_semantics_change`,
`incremental_change`, `schema_change`, `missing_relation`, `unknown_state`,
`cache_reuse`, `cache_miss`, `adapter_change`, `output_drift`, `forced`,
`unchanged`, `selected_dependency`, `selection_expansion`,
`state_unavailable`, `git_change`, `resumed_run`). `--json` exposes them with an
optional `subject` (the dependency or source the reason is about), plus the
resolved `selection` (terms, matched, expanded, required, exclude) and each
model's `membership` (`selected` / `expanded` / `dependency`).

## 41. `apply`

```bash
phlo transform apply
```

Ideally, `apply` executes a previously calculated plan:

```bash
phlo transform apply <plan-id>
```

This guarantees execution matches the inspected plan unless source state invalidates it.

## 42. Stale plans

A plan becomes stale when relevant conditions change, including source snapshot, branch state, model files, dependencies or catalogue schema. Apply should reject stale plans unless explicitly replanned.

## 43. Selective planning

All planning commands share one selector engine (see §82). Examples:

```bash
phlo transform plan assay.results        # one model + its dependencies
phlo transform plan assay.results+       # …plus transitive dependents
phlo transform plan +assay.results       # explicit upstream expansion
phlo transform plan 'assay.*'            # namespace glob
phlo transform plan source:lims+         # models reading lims + dependents
phlo transform plan --tag qc             # intersect the selection by tag
phlo transform plan --changed            # desired version ≠ recorded state
phlo transform plan --select assay.* --exclude assay.legacy_raw
phlo transform plan --downstream assay.results
```

Positional selector terms and `--select` are equivalent. `--exclude` is
absolute: an excluded model is never pulled back in by `+` expansion or by
dependency closure — a selected model that depends on it plans against the
existing materialisation and the plan records a warning. If the excluded
model was never materialised the plan is rejected outright (excluding an
ephemeral dependency is always fine — its SQL is inlined, so no relation is
needed).

---

## 44. Nessie-native environments

For the primary Phlo environment, Nessie references should represent transformation environments directly.

Examples:

```text
main
feature/assay-normalisation
ci/pr-184
release/2026-09
```

Avoid building a separate environment abstraction when Nessie already provides one.

## 45. Branch behaviour

A Git branch may map automatically or explicitly to a Nessie reference. Explicit form:

```bash
phlo transform --ref feature/new-assay plan
```

References are managed explicitly — `ref list`, `ref show`, `ref create --from <base>` and `ref delete` — and no command creates or deletes a branch as a side effect, except explicit candidate provisioning on `plan`/`apply`/`run --ref` and `--cleanup` on `promote`. `ref delete main` is refused: `main` is the default base, not a scratch branch. Provisioning is recorded per candidate in `environment_<sanitised ref>_<hash>.json` (plus the single-slot `environment.json`), and deleted with the branch. Each artifact carries `created_from` — the reference and commit the candidate was provably created from, recorded at `ref create` or first provisioning and preserved across re-provisioning. A pre-existing branch Phlo did not create has unrecorded provenance; promotion refuses it rather than redefine its base as the current target head.

## 46. Write-Audit-Publish

Production-oriented execution follows:

```text
WRITE
  ↓
AUDIT
  ↓
PUBLISH
```

Typical sequence:

```text
create/update branch state
      ↓
execute models
      ↓
run contracts
      ↓
run tests
      ↓
run data diffs
      ↓
apply policy
      ↓
promote Nessie reference
```

Transforms should not write directly to canonical production state by default.

## 47. Promotion

Example:

```bash
phlo transform promote feature/new-assay --to main
phlo transform promote --from feature/new-assay --to main
```

Promotion is authorised by named gates, reported identically in human and JSON output: `run` (latest candidate run passed and validated the exact commit being promoted — a passed run is bound to the candidate head **after** its writes land, and a candidate that advanced since its run, or a run recorded before commit binding, fails), `tests` (no failed tests), `blocked` (no blocked/cancelled model, seed or test work), `schema` (a fresh audited diff inspected this pair at these commits and found no unwaived breaking changes — physical breaks come from the audited diff; contract breaks are computed live from the workspace's desired contracts against the target's recorded contracts, so a post-`diff` contract edit cannot slip past — and absent or stale evidence fails closed, never reading "no evidence" as "no changes"), `data_diff` (when `--require-diff` is set: a passing `--full` audited diff bound to this candidate→target pair at the commits being promoted — the artifact records both refs' resolved heads and is rejected when they no longer match — still fresh per recorded versions, and never a self-comparison; a single-model `diff.json` is never promotion evidence), `base` (the target still equals the commit the evidence was established against — the hash-bound artifact's recorded base, else the candidate's `created_from` provenance; unknown provenance fails rather than redefining the base as the current head, and the merge asserts the evaluated target hash) and `conflicts` (the merge check is clean). A candidate that advanced between gate evaluation and merge is refused rather than promoted unaudited. `--check` evaluates gates without merging; a passing promotion merges and persists a `PromotionRecord` (refs, hashes, plan/run ids, gate results, timestamp) in the state store.

Preconditions may include successful plan, successful execution, required tests passing, no blocking schema changes, no stale state and optional approval.

## 48. Rollback

Because published table state is versioned, rollback is a first-class operation:

```bash
phlo transform rollback assay.results --to <version>
```

or through environment-level Nessie rollback semantics.

---

## 49. Schema evolution

Schema changes must be understood structurally.

```text
assay.results

Schema changes

+ dilution_factor DOUBLE
~ result FLOAT -> DOUBLE
- legacy_result VARCHAR
```

Classification:

```text
SAFE
  + nullable column

REVIEW
  numeric widening

BREAKING
  removed column
```

## 50. Schema policy

Workspace policy may define behaviour:

```toml
[schema]
allow_nullable_additions = true
allow_numeric_widening = true
breaking_changes = "error"
```

## 51. Downstream schema impact

Removing `assay.results.legacy_result` may produce:

```text
Affected downstream models:

analytics.legacy_report
  direct reference at line 18

qc.monthly_extract
  SELECT * dependency
```

This must be available during planning before data is modified.

---

## 52. Data diff

Native command:

```bash
phlo transform diff assay.results
phlo transform diff --from feature/new-assay --to main
```

With no model argument, `diff` compares two Nessie references: every dataset known to the workspace or recorded in state is classified `added`/`removed`/`changed`/`unchanged`/`absent`, schema and nullability changes are listed per model, row counts come from the catalogs, and `--full` runs keyed value diffs on changed models. `main`'s records include the default (unlabeled) environment, so state from a run with no `--ref` still counts. When Nessie is configured the report records both refs' resolved commit hashes, binding the evidence to exact branch heads. The report is the audit artifact promotion consumes.

Example:

```text
assay.results

Rows
  main       128,231
  candidate  128,237
  delta           +6

Records
  added          12
  removed         6
  modified       31

Changed values
  concentration  23
  status          8
  sample_id       0
```

## 53. Data-diff strategies

Possible comparison strategies:

```text
key-based
partition-based
aggregate
sampled
full
```

Large-table defaults should avoid expensive full comparison.

```bash
phlo transform diff assay.results --full
```

## 54. Diff policies

A workflow may require:

```toml
[model.assay_results.diff]
max_removed_rows = 0
max_changed_fraction = 0.05
```

This may become a promotion quality gate. Row thresholds require keyed coverage to measure; on a keyless model (or under `--partition`) they fail rather than pass on unmeasured zeros.

---

## 55. Model lineage

Compilation produces one canonical `LineageGraph` — model, dataset, column
and test nodes joined by `input`, `output`, `derives`, `contains` and `tests`
edges. Model outputs, sources and seeds are all `dataset://` nodes, so lineage
is expressed between datasets rather than being tied to transform models;
future ingestion systems contribute nodes and edges to the same graph.

Every input connects identically: `dataset ──input──▶ model` covers sources,
seeds and upstream model outputs alike (`model → model` and `dataset →
dataset` edges exist only as one-hop rollups). Tests are consumers — `dataset
──tests──▶ test` — so impact traversal reaches them naturally.

Every report and export reads this one structure — `lineage`, `impact`,
artifacts and the OpenLineage exporter never re-derive their own:

```text
Compilation ─▶ LineageGraph ─▶ OpenLineageExporter
```

Column `derives` edges carry optional metadata: `directness`
(`direct`/`indirect`), `transformation` (`identity`, `transformation`,
`aggregation`, `join`, `filter`, `group_by`, `sort`, `window`, `conditional`),
`confidence` (`exact` for AST-proven lineage, `unknown` when analysis is
incomplete; `inferred`/`declared`/`runtime` reserved for other producers) and
the responsible SQL expression. Join keys, filters, grouping and sort keys are
recorded as indirect inputs rather than dropped.

The graph is indexed and deterministic; `document()` serialises it and
`document_for`/`subgraph` scope it to a selection. See
[`docs/lineage.md`](docs/lineage.md).

```bash
phlo transform lineage assay.assay_results
```

```text
lims.results
    ↓
assay.raw_results
    ↓
assay.clean_results
    ↓
assay.assay_results
    ↓
analytics.monthly_summary
```

`lineage --format graph` prints the canonical document;
`lineage --format openlineage` exports the OpenLineage design-time document —
a bare JSON array of spec-valid `JobEvent`s and `DatasetEvent`s, each with
`eventTime`/`producer`/`schemaURL`, usable directly as a batch-endpoint
payload. Both accept a model target or selector terms for scoping.

`lineage --diff <git-ref>` is the semantic complement to the data-side
`branch_diff`: the workspace subtree at `merge-base(ref, HEAD)` is
materialised read-only (`ls-tree`/`cat-file`, no worktree or checkout
mutation), compiled with the same options, and the two graphs are compared
— nodes and edges added, removed or field-changed, plus the consumers each
removed node orphans and the downstream each removed or moved edge affects.
Edges compare as multisets: parallel edges between the same nodes keep
their metadata distinct. `lineage --diff <base> <candidate>` compares two
exact refs instead. The report persists to `lineage_diff.json` with the
resolved base commit, the candidate's git head and worktree state, and the
Nessie branch-pair binding when `--ref`/`--from` resolves — `promote`
audits that provenance and reports the delta as `current`, `advisory`, or
`stale` with the reason rather than treating a moved report as evidence.

## 56. Column lineage

```bash
phlo transform lineage assay.assay_results.result
```

```text
lims.raw_results.signal
        ↓
assay.raw_results.signal
        ↓
assay.clean_results.corrected_signal
        ↓
assay.assay_results.result
```

The report separates direct inputs from indirect ones (join keys, filters,
grouping/sort keys) and prints the column's confidence when it is not
`exact`.

## 57. Impact analysis

```bash
phlo transform impact assay.results.result
phlo transform impact external.samples.volume
```

Returns downstream columns, models, workflows, tests and registered published
consumers. Source and seed columns are valid targets — the walk traverses the
canonical graph, so a raw input column reports every downstream column and
model it feeds.

---

## 58. Workflow integration

Transforms may belong to a workflow by physical ownership:

```text
workflows/
└── assay_ingest/
    ├── workflow.toml
    ├── tasks/
    └── transforms/
```

The workflow engine should not need to enumerate every transform model. A transform namespace may expand internally into its transformation DAG.

## 59. Nested DAGs

High-level workflow:

```text
extract
   ↓
validate
   ↓
transform
   ↓
publish
```

Transform expands to:

```text
raw_results
     ↓
clean_results
     ↓
assay_results
```

Combined graph:

```text
extract
   ↓
validate
   ↓
raw_results
   ↓
clean_results
   ↓
assay_results
   ↓
publish
```

This creates one coherent workflow/data-lineage representation.

## 60. Cross-workflow dependencies

A model may reference another workflow's model:

```sql
select *
from manufacturing.batches
```

inside `workflows/analytics/transforms/` creates:

```text
manufacturing.batches
        ↓
analytics.monthly_summary
```

This must not require publishing packages between separate transformation projects.

## 61. Ownership boundaries

Cross-workflow references may optionally trigger policy:

```toml
[dependencies]
cross_workflow = "warn"
```

A future visibility model may distinguish workflow-private, workspace-public and external-public models.

---

## 62. Execution engine

Execution is graph-based and requires:

- topological scheduling;
- bounded concurrency;
- dependency-aware failure handling;
- retries for transient failures;
- cancellation;
- structured events;
- deterministic status;
- per-model timing;
- target-specific transactions where applicable.

Tokio is the expected async runtime.

## 63. Execution states

A model execution may be:

```text
pending
ready
running
passed
failed
skipped
blocked
cancelled
cached
```

`blocked` indicates an upstream dependency prevented execution — the model's own SQL never ran, and it is not reported as a model failure. `skipped` and `cached` satisfy dependents; `failed`, `blocked` and `cancelled` do not.

## 64. Partial failure

Independent branches may continue when safe.

```text
A → B → C
D → E
```

If B fails:

```text
A PASS
B FAIL
C BLOCKED

D PASS
E PASS
```

With `--fail-fast`, the first unrecoverable failure instead stops new scheduling: dependents of the failure are `blocked`, unrelated not-started work is `cancelled`, and in-flight tasks are aborted. Each attempt runs through a tracked adapter view that reports its in-flight query ids, so abort and per-attempt `--model-timeout` cancel the underlying warehouse query where the adapter supports it (Trino: `DELETE /v1/query/{id}`); adapters that cannot report in-flight queries degrade to dropping the attempt's future.

## 65. Retry behaviour

Retry applies only to recognised transient execution failures by default. Compilation errors and deterministic test failures must not be retried automatically.

Every failure carries a stable machine-readable category (`adapter`, `sql`, `test`, `timeout`, `cancelled`, `dependency`, `state`, `internal`) plus the adapter error code, attempt number, timestamp and a `retryable` flag. `--retries N` allows extra attempts with bounded exponential backoff; only adapter failures the adapter marked retryable are retried — SQL-semantic errors are never retried, and test/timeout/dependency/cancellation failures are scheduler or assertion outcomes, not transient faults. Every attempt is recorded.

## 65a. Interrupted runs

Execution progress is persisted incrementally: the run row and stored plan are written before scheduling, each node record lands as it transitions, and every failed attempt is persisted before its retry backoff begins. A state-store write failure is fatal to the run — progress that cannot be recorded cannot be trusted for resume. A killed process therefore leaves an accurate partial record rather than a misleadingly successful one.

Two continuations rebuild from that state:

- `run --resume <run-id>` — continue an *interrupted* run (still `running`, or `cancelled`) under the same run id. Verified `passed` work is reused (a model's earlier `passed` counts only when its desired version still matches and its target relation still exists; a seed's only when its content hash matches and its target exists); every other node is re-planned through the normal planner against current state — stored `skip`/`cached` decisions, `full_rebuild` flags and watermarks are never trusted once the workspace moved, so an incremental strategy/key change or a schema change classified as full-rebuild-required rebuilds fully, and time-window models read the current watermark. A finished run is refused: `failed` redirects to `--retry-failed`.
- `run --retry-failed <run-id>` — start a new run (linked by `continued_from`) over the failed/blocked/cancelled models of a finished run, the dependencies they still need, and any tests that failed; tests over rebuilt models re-verify too.

Only `passed` materialisations update materialised-version state; a model version is never recorded as successful before execution genuinely completes.

---

## 66. Adapters

Adapters expose a compact Rust trait conceptually similar to:

```rust
trait Adapter {
    async fn introspect_relation(...);
    async fn execute(...);
    async fn create_table(...);
    async fn create_view(...);
    async fn apply_incremental(...);
    async fn drop_relation(...);
    async fn get_source_state(...);
}
```

Avoid executable macro dispatch.

## 67. Adapter priority

- v1: Trino;
- v1.x: DuckDB;
- v2: PostgreSQL.

Additional adapters are justified only by concrete use cases.

## 68. Trino adapter

Primary responsibilities:

- SQL execution;
- metadata introspection;
- Iceberg relation management;
- CTAS;
- view creation;
- incremental write strategy;
- query status;
- cancellation;
- errors;
- statistics where available.

## 69. Iceberg awareness

The primary adapter should understand Iceberg concepts explicitly, including snapshot ID, schema ID, partition specification, table properties and commit history. These feed planning and state comparison.

## 70. Nessie adapter layer

Nessie operations should be separate from SQL execution.

Responsibilities:

- branch creation;
- branch lookup;
- hash resolution;
- merge;
- conflict detection;
- branch deletion;
- rollback;
- environment comparison.

---

## 71. Build artifacts

Every run should emit structured artifacts:

```text
.phlo/
└── transform/
    ├── manifest.json
    ├── graph.json
    ├── plan.json
    ├── run.json
    ├── lineage.json
    ├── openlineage.json
    └── state/
```

These files are interfaces, not incidental logs.

## 72. Manifest

`manifest.json` includes workspace, transform roots, models, sources, IDs, paths, effective configuration, materialisations, dependencies, columns, contracts, tests and model hashes.

## 73. Graph artifact

`graph.json` includes nodes, edges, edge types, column edges, workflow relationships and external-source relationships.

`lineage.json` holds the canonical lineage document — every model, dataset, column and test node with its edges and column-level metadata — and `openlineage.json` the same graph exported as an OpenLineage design-time document (a JSON array of spec-valid `JobEvent`s and `DatasetEvent`s) for tools such as OpenMetadata and DataHub.

## 74. Plan artifact

`plan.json` includes plan ID, current environment state, desired state, models selected, the resolved selection (terms, excludes, matched/expanded/required provenance), structured per-model reasons with stable kind codes, proposed physical operations, schema impacts, tests, data-diff requirements and promotion constraints.

## 75. Run artifact

`run.json` includes run ID, plan ID, `continued_from`, environment, the candidate reference hash the run validated (`reference_hash`, bound post-run), timestamps, model/seed/test results, compiled SQL hashes, query IDs, per-status counts, model versions, source versions, timings, per-attempt records and structured failures (category, adapter code, retryable flag).

---

## 76. CLI

Primary interface:

```text
phlo transform
```

## 77. CLI commands

Core commands:

```bash
phlo transform check
phlo transform plan
phlo transform apply
phlo transform run
phlo transform test
phlo transform inspect
phlo transform explain
phlo transform lineage
phlo transform impact
phlo transform diff
phlo transform list
phlo transform promote
phlo transform rollback
phlo transform clean
phlo transform state runs|show|model|promotions
```

The state backend is selected by `--state`/`PHLO_STATE_URL`: a `postgres://`/`postgresql://` URL selects the shared PostgreSQL store; anything else is a SQLite file path (default `.phlo/transform/state.db`).

## 78. `check`

Compile and statically validate without planning execution:

```bash
phlo transform check
```

Output includes SQL parse errors, unresolved relations, ambiguous references, circular dependencies, type mismatches and contract errors.

It should be fast enough for frequent local use.

## 79. `run`

Convenience command, broadly `plan + apply`, intended for development environments. Production use should favour explicit plan/apply.

Execution flags apply to `run`/`apply` alike:

```bash
phlo transform run --jobs 4              # bounded concurrency
phlo transform run --retries 2           # transient-failure retries
phlo transform run --fail-fast           # stop scheduling on first failure
phlo transform run --model-timeout 30m   # per-attempt timeout
phlo transform run --resume <run-id>     # continue an interrupted run
phlo transform run --retry-failed <run-id>  # new run over the failed portion
```

`--resume`/`--retry-failed` take a full run id or a unique prefix; selection flags do not apply to them — the prior run defines the work.

## 80. `inspect`

```bash
phlo transform inspect assay.results
```

Example:

```text
Model: assay.results
ID: model://assay/results
Path: workflows/assay/transforms/results.sql

Materialisation: incremental
Key: experiment_id

Depends on:
  lims.results
  shared.samples

Used by:
  analytics.monthly_summary

State:
  desired: b74a02e
  current: a621ee3
  status: changed
```

### `explain`

`explain` is the per-model companion to `plan`: identity, dependencies,
recorded state, and the current build decision with the same structured
reasons `plan` reports (the state diff needs no adapter; when one is
configured the relation-existence check makes the decision identical to
`plan`):

```bash
phlo transform explain assay.results
```

```text
Model:         assay.results
…
Version:       b74a02e…
Recorded:      a621ee3… in dev (materialised 2026-02-10T08:41:07Z)
Decision:      build
Reasons:
  upstream version changed: assay.clean_results
  upstream assay.clean_results will rebuild in this plan
```

## 81. Machine output

Every relevant command should support `--json`.

No semantic information should exist only in formatted CLI output.

## 82. Selectors

One selector language is shared by `plan`, `apply`, `run`, `test`,
`lineage`, `impact` and `list`. Per term:

```text
term    := "+"? body "+"?
body    := "tag:" value            model carries the tag
        |  "namespace:" value      model's namespace (first name segment)
        |  "source:" value         model reads a matching source
        |  "changed"               desired version differs from recorded
                                   state — or, under `--since`, from a
                                   Git ref
        |  "all" | "*"             every model
        |  pattern                 name, `model://` URI, `prefix.*` glob,
                                   or a unique name suffix
```

A leading `+` adds every transitive dependency of the matched models; a
trailing `+` adds every transitive dependent; `+model+` does both. Include
terms union; `--tag`/`--workflow` intersect; `--exclude` terms subtract last
and are absolute (excluded models are never re-added by expansion or
dependency closure). Selection is deterministic — topological order where
the graph allows — and errors are explicit: empty, invalid, ambiguous and
unmatched selectors all fail with named candidates or suggestions rather
than silently widening.

CLI mapping:

```bash
phlo transform plan assay.results 'assay.*'   # positional terms
--select tag:qc                               # same grammar via flag
--exclude assay.legacy_raw                    # subtract (applied last)
--tag qc --workflow assay_ingest              # intersect filters
--changed                                     # shorthand for `changed`
--since main                                  # `changed` resolves via Git
--upstream / --downstream                     # expand the filtered base
--force                                       # rebuild regardless of state
```

The `changed` term has two providers answering different questions:

- **State-derived** (default): the desired model version differs from the
  version recorded as materialised for the target environment. Because a
  moved upstream version changes dependents' versions, this set naturally
  includes downstream models. Ephemeral models are never reported changed —
  they are never materialised; their edits surface through dependents'
  dependency versions.
- **Git-derived** (`--since <ref>`): semantic project inputs changed
  relative to a Git ref — the *directly changed* set only. Downstream
  impact comes from selector expansion (`changed+`), not from the diff.

```bash
phlo transform plan --since main            # shorthand for --select changed --since main
phlo transform plan --since main --select changed+
phlo transform run --since origin/main
phlo transform impact --since main          # blast radius of the diff
```

`--since` compares `merge-base(<ref>, HEAD)` against the working tree —
the fork point, so a moving `main` doesn't inflate the diff — and counts
staged, unstaged and untracked files. It requires a `changed` term
somewhere in the selector set (implied when no include terms are given);
`--since <ref> <model>` without `changed` is an error rather than a
silently ignored flag.

The Git provider maps each changed workspace path once — model files
(semantic comparison: canonical SQL plus directives, so comment- and
formatting-only edits don't count), `seeds/*.csv` (consumers of the seed's
source relation; unused seeds are still reported), `tests/*.sql` (the
test's target models, so `test --since` covers them), `transform.toml`
(every model under its directory) and `phlo.toml` (narrowed to changed
sections; `[transform]`/`[dependencies]`/defaults widen to all models).
Added, modified, renamed, deleted and untracked files all count; a deleted
or renamed-away model marks its dependents, since they now read a source
where a model used to be. When a path cannot be proven narrow, the
provider widens rather than guesses.

Direct changes carry `git_change` selection provenance into the plan —
distinct from rebuild reasons, which stay state/version-based:

```text
BUILD assay.raw       selected because transforms/assay/raw.sql modified since main
                      SQL semantics changed
BUILD assay.results   selected by `changed+`
                      upstream assay.raw will rebuild
```

Plan JSON exposes `plan.git`: the requested ref, resolved merge-base and
HEAD, every changed workspace path with its status, the directly changed
models with causes, changed seeds and consumers, changed tests, deleted
model identities, and paths outside the workspace.

---

## 83. Agent interface

Phlo Transform must be explicitly designed for machine interaction.

Examples:

```bash
phlo transform inspect assay.results --json
phlo transform lineage assay.results --json
phlo transform impact assay.results.result --json
phlo transform plan --json
```

Agents should query compiler truth directly.

## 84. Agent use cases

Agents should be able to answer:

- what does this model depend on?
- where does this column originate?
- what breaks if this column is removed?
- which models changed?
- why will this model rebuild?
- what will this PR change in production?
- which models failed previously?
- what is the inferred schema?
- which workflows consume this dataset?
- what are the current quality gates?

without manually parsing source files.

---

## 85. Transform daemon

A later phase should introduce:

```bash
phlo transform daemon
```

The daemon maintains an incremental in-memory workspace representation containing parsed ASTs, typed ASTs, dependency graph, catalogue schemas, lineage, hashes, filesystem state and compiled plans.

## 86. Daemon consumers

Potential consumers include CLI, Phlo UI, editor extensions, workflow engine, AI agents, CI tooling and developer tools.

Communication may use local HTTP, Unix sockets, JSON-RPC or gRPC. Initial implementation should favour the simplest stable interface.

## 87. Incremental compilation

Filesystem changes should invalidate only affected compiler state:

```text
assay/results.sql changed
        ↓
reparse results
        ↓
re-resolve its dependencies
        ↓
invalidate downstream typing/lineage
```

Do not recompile the entire workspace unnecessarily.

## 88. Developer experience

Target local loop:

```text
edit SQL
   ↓
instant static feedback
   ↓
phlo transform plan
   ↓
inspect changes
   ↓
apply
```

The compiler should be fast enough that `check` can reasonably run on save in moderate projects.

---

## 89. Errors

Errors must be typed, deterministic, location-aware, actionable and machine-readable.

```text
error[T012]: unknown column `reslt`

 --> workflows/assay/transforms/results.sql:14:9

14 |     reslt
         ^^^^^

Did you mean:
  result

Source:
  assay.raw_results
```

## 90. Error categories

Suggested categories:

```text
PROJECT
CONFIG
PARSE
RESOLUTION
TYPE
GRAPH
CONTRACT
PLAN
ADAPTER
EXECUTION
TEST
DIFF
PROMOTION
STATE
```

Every error should have a stable error code.

## 91. Observability

Structured events should cover compilation, planning, model queueing, model execution, query IDs, test execution, retries, state comparison, promotion and errors.

Human logs and structured logs should be produced from the same event stream.

## 92. Auditability

Run artifacts should provide enough information to reproduce and explain an execution, including code/model version, compiler version, effective configuration, dependency versions, source versions, plan, executed SQL hash, execution result, tests, environment and promotion result.

This positions Phlo Transform well for controlled scientific and regulated environments without making the core engine specific to one regulatory regime.

## 93. Determinism

Given the same workspace, configuration, compiler version, source state and target semantics, planning must produce the same desired model versions and graph.

Nondeterministic configuration evaluation should be prohibited.

---

## 94. Templating

Templating support should be intentionally restricted. Possible supported constructs:

```text
ref()
source()
var()
simple if
simple for
small built-in macro library
```

The compiler must avoid arbitrary Python execution.

## 95. Native SQL preferred over templates

Preferred:

```sql
select *
from assay.results
```

Supported compatibility:

```sql
select *
from {{ ref("assay.results") }}
```

Any feature achievable with ordinary SQL should prefer ordinary SQL.

## 96. Variables

Variables should be used sparingly:

```toml
[vars]
site = "oxford"
```

```sql
where site = '{{ var("site") }}'
```

Environment-specific relation names should ideally be handled by catalogue/environment resolution rather than variable interpolation.

## 97. Macros

Initial macro support should be a fixed built-in function set. Do not initially support arbitrary project-defined executable macros.

Future custom macros, if added, should ideally operate over SQL AST structures rather than arbitrary text.

---

## 98. SQL formatting and canonicalisation

The compiler should canonicalise SQL ASTs for hashing. Irrelevant formatting should not trigger rebuilds.

These should ideally hash equivalently:

```sql
select * from assay.results
```

and:

```sql
SELECT
    *
FROM assay.results
```

Comments unrelated to directives should similarly not necessarily invalidate materialisation state.

## 99. Semantic change detection

Longer term, state calculation may distinguish:

```text
text change
AST change
semantic change
physical result-affecting change
```

Initial implementation may use canonical AST hashing.

---

## 100. Documentation

Much documentation can be generated automatically.

For each model:

- description;
- path;
- owner;
- materialisation;
- inferred schema;
- dependencies;
- consumers;
- lineage;
- tests;
- run history.

Descriptions may optionally be supplied through comments or config. No separate documentation site is required initially.

## 101. UI integration

Phlo UI can consume compiler artifacts or APIs to render workspace graphs, model details and plan review.

Model detail should include SQL, schema, lineage, history, tests, state and diffs.

Plan review should include models changing, schema impact, data impact, tests and promotion status.

---

## 102. Rust project structure

Suggested workspace:

```text
crates/
├── phlo-transform-core/
├── phlo-transform-project/
├── phlo-transform-parser/
├── phlo-transform-sql/
├── phlo-transform-graph/
├── phlo-transform-types/
├── phlo-transform-state/
├── phlo-transform-plan/
├── phlo-transform-executor/
├── phlo-transform-test/
├── phlo-transform-lineage/
├── phlo-transform-diff/
├── phlo-transform-adapter/
├── phlo-transform-trino/
├── phlo-transform-duckdb/
├── phlo-transform-nessie/
├── phlo-transform-artifacts/
├── phlo-transform-cli/
└── phlo-transform-daemon/
```

Do not over-fragment crates during early development. Initial implementation may consolidate several of these until interfaces stabilise.

## 103. Core Rust model

Conceptually:

```rust
struct Workspace {
    roots: Vec<TransformRoot>,
    models: ModelRegistry,
    sources: SourceRegistry,
    graph: TransformGraph,
}
```

```rust
struct Model {
    id: ModelId,
    namespace: Namespace,
    path: PathBuf,
    sql: ParsedSql,
    resolved_sql: ResolvedSql,
    config: ModelConfig,
    schema: Option<ModelSchema>,
    dependencies: Vec<RelationId>,
    columns: Vec<Column>,
}
```

```rust
struct ModelVersion {
    model: ModelId,
    version: ContentHash,
    sql_hash: ContentHash,
    config_hash: ContentHash,
    dependencies: Vec<ModelVersionRef>,
    sources: Vec<SourceVersionRef>,
}
```

## 104. Graph representation

Use a DAG-capable Rust graph structure such as `petgraph`.

Potential edge types:

```rust
enum DependencyKind {
    Model,
    Source,
    Column,
    Workflow,
    Test,
}
```

Model-level and column-level graphs may be stored separately where operationally simpler.

## 105. Template engine

If Jinja compatibility is retained, a constrained engine such as MiniJinja is suitable. Expose only allowed functions and constructs; do not expose arbitrary host-language functions by default.

## 106. State storage

Initial local state may use SQLite or DuckDB.

Requirements:

- model versions;
- run history;
- plan history;
- source states;
- execution metadata.

Phlo deployments may later use PostgreSQL. The storage interface should be abstracted.

## 107. State versus artifacts

Artifacts are immutable execution outputs. State storage is queryable operational history. Do not rely solely on JSON files for long-term operational state.

---

## 108. CI integration

Typical CI:

```text
checkout
   ↓
phlo transform check
   ↓
phlo transform plan --base main
   ↓
create CI Nessie branch
   ↓
apply
   ↓
tests
   ↓
diff
   ↓
report
```

CI artifacts should contain the complete plan and results.

## 109. Pull-request reporting

Phlo could generate:

```text
Transform changes

Models
  4 changed
  2 downstream rebuilt

Schema
  +1 column
  0 breaking changes

Data
  +12 rows
  -0 rows
  31 records modified

Tests
  27 passed
  0 failed
```

---

## 110. Performance targets

For roughly 1,000 models, design toward:

```text
cold check: <5 seconds
incremental check: <500 ms for common edits
```

These are design targets rather than hard MVP requirements.

Scheduling overhead should be negligible relative to warehouse queries. The semantic graph should remain comfortably resident in memory for normal enterprise workspaces.

## 111. Security

The compiler itself should not require unrestricted database permissions.

Separate credentials where practical for read/introspection, development writes, branch writes and production promotion.

Sensitive credential values must never be embedded in model artifacts or logs.

## 112. Concurrency

```toml
[transform.execution]
max_concurrency = 8
```

The scheduler should respect graph readiness, adapter limits, warehouse limits and optional resource pools.

Future:

```toml
[resources]
trino_heavy = 2
trino_normal = 8
```

## 113. Resource hints

Later, models may specify:

```sql
-- @resource heavy
```

This can influence scheduling without exposing warehouse-specific knobs throughout model definitions. Not required for MVP.

---

## 114. Public API

Phlo Transform should exist as:

1. Rust library;
2. CLI;
3. structured artifact interface;
4. later daemon/service API.

The CLI should remain a thin consumer of core libraries.

## 115. Library API

Potential high-level Rust interface:

```rust
let workspace = Workspace::load(path)?;
let compilation = workspace.check().await?;
let plan = compilation.plan(target).await?;
let result = plan.apply().await?;
```

The Phlo workflow engine should use this API directly rather than shelling out where practical.

## 116. Extension philosophy

Extension points should exist only where there is a known abstraction boundary.

Good extension points:

- adapters;
- state backends;
- catalogue providers;
- policy engines;
- event sinks.

Avoid arbitrary executable plugin systems initially.

---

## 117. Delivery phases

Implementation details and acceptance criteria are broken out under [`docs/roadmap/`](docs/roadmap/README.md).

The high-level phases are:

0. Compiler spike;
1. MVP build engine;
2. typed compiler and lineage;
3. state-aware execution;
4. incremental models;
5. Nessie and WAP;
6. data diff;
7. workflow integration;
8. daemon and agent APIs.

---

## 118. MVP acceptance criteria

The first genuinely usable release must support:

```text
workflows/
├── assay/
│   └── transforms/
│       ├── raw.sql
│       └── results.sql
│
└── reporting/
    └── transforms/
        └── monthly.sql
```

`results.sql`:

```sql
select *
from assay.raw
```

`monthly.sql`:

```sql
select *
from assay.results
```

Without additional dependency configuration, the engine must derive:

```text
assay.raw
    ↓
assay.results
    ↓
reporting.monthly
```

It must then:

1. compile the workspace;
2. detect syntax errors;
3. detect unresolved relations;
4. construct the DAG;
5. create a plan;
6. execute in dependency order;
7. run tests;
8. persist run state;
9. skip unchanged models on subsequent runs;
10. expose structured model metadata.

## 119. Full product acceptance criteria

A mature release should allow a developer to run:

```bash
phlo transform plan
```

and understand what changed, why, what will execute, what data may change, what schemas will change, which downstream models are affected, what tests will run and whether promotion is safe.

Then:

```bash
phlo transform apply
```

should execute only required work, run appropriate tests, persist reproducible execution state, generate complete artifacts and leave production unaffected until promotion.

Then:

```bash
phlo transform promote
```

should atomically publish approved state where supported.

---

## 120. Explicit simplifications versus dbt

| dbt-style concept | Phlo Transform |
|---|---|
| project-per-model-tree | workspace with many roots |
| mandatory `ref()` dependency model | SQL relation resolution |
| mandatory source declarations | source inference |
| YAML-heavy metadata | inference + optional config |
| Jinja-heavy compilation | mostly real SQL |
| arbitrary macros | constrained functions |
| adapter macros | Rust adapter traits |
| multiple config precedence paths | fixed hierarchy |
| runtime-first validation | compile-first validation |
| state as optimisation | state as core execution primitive |
| environment abstraction | Nessie references where available |
| manual WAP patterns | native WAP |
| separate data/workflow lineage | unified graph |
| textual SQL understanding | typed semantic model |

## 121. Key differentiators

Phlo Transform succeeds by being better specifically at:

- **workspace-native transformations** — `workflows/X/transforms` works naturally;
- **SQL-native dependencies** — no `ref()` required in ordinary cases;
- **minimal configuration** — infer anything that can be inferred safely;
- **typed compiler** — understand models and columns structurally;
- **column-level lineage** — native compiler output;
- **build-system semantics** — desired state versus current state;
- **plan/apply** — changes are inspectable before execution;
- **Nessie-native environments** — versioned data and transformation state align;
- **WAP** — safe publication is the normal production execution model;
- **native data diff** — understand actual output changes, not only SQL changes;
- **unified workflow and data lineage** — transformations belong naturally to wider workflow execution;
- **agent-native interfaces** — AI systems query structured compiler state instead of reverse-engineering source trees.

---

## 122. Architectural north star

```text
                    SQL
                     │
               ┌─────▼─────┐
               │ Compiler  │
               └─────┬─────┘
                     │
         ┌───────────┼───────────┐
         ▼           ▼           ▼
      Typed DAG    Lineage      State
         │           │           │
         └───────────┼───────────┘
                     ▼
                   Plan
                     │
              ┌──────┴──────┐
              ▼             ▼
            Build          Diff
              │
              ▼
            Audit
              │
              ▼
           Promote
```

The compiler is the core product. Execution, lineage, testing, planning, state, agents and workflow integration all derive from the same semantic representation.

## 123. Final design rule

Every new feature should be tested against this question:

> **Can the compiler infer this reliably instead of requiring the user to configure it?**

If yes, infer it.

If no:

> **Can it be expressed declaratively with one small, predictable mechanism?**

If yes, configure it once.

Only after both answers are no should a more complex abstraction be introduced.

That principle should keep Phlo Transform substantially smaller than dbt while allowing it to become more capable in the areas that matter to Phlo.
