# Parallel roadmap — dbt migration and translation

## Objective

Provide a one-way migration tool that analyses an existing dbt project and emits the simplest equivalent native Phlo Transform project.

This is a migration frontend, not a dbt compatibility runtime. Phlo Transform must not carry dbt's Jinja, macro, adapter-dispatch or configuration semantics into the core engine merely to make imported projects run.

The architectural goal is:

```text
                   native Phlo SQL
                         │
                         ▼
                    ┌─────────┐
                    │ frontend│
                    └────┬────┘
                         │
 dbt project ── dbt frontend ─┤
                         │
                         ▼
                Phlo semantic model
                         │
              ┌──────────┴──────────┐
              ▼                     ▼
           compile               emit native
                                  Phlo project
```

The compiler and execution engine operate only on the Phlo semantic model. dbt-specific parsing and migration logic remains isolated in the importer/translator.

## Delivery relationship

This roadmap is intentionally parallel to the numbered critical path.

- Phase 0 must establish a frontend-agnostic semantic boundary so a dbt importer can target the same model representation later.
- A useful translator should begin once Phase 1 has stabilised native model/config/materialisation semantics.
- Richer conversion of tests, types, contracts and lineage can improve after Phase 2.
- Translation must not block the core compiler, execution, state, Nessie or workflow roadmaps.

## Primary commands

Analysis only:

```bash
phlo transform translate --from dbt ./my-dbt-project --check
```

Write translated project:

```bash
phlo transform translate --from dbt ./my-dbt-project --output ./transforms --write
```

Equivalent shorthand may be supported when dbt is the only importer:

```bash
phlo transform translate ./my-dbt-project --check
```

The default must be non-destructive. Existing files are never overwritten without an explicit overwrite policy.

## Translation philosophy

Translate **semantics**, not syntax.

Do not mechanically turn dbt Jinja into equivalent Phlo Jinja when the underlying intent can be represented by a native Phlo concept.

Example dbt model configuration:

```yaml
models:
  - name: samples
    config:
      materialized: incremental
      unique_key: sample_id
    columns:
      - name: sample_id
        tests:
          - unique
          - not_null
```

Should preferentially become:

```sql
-- @incremental key=sample_id

select ...
```

The key declaration already conveys record identity and may imply uniqueness/non-null checks. Do not preserve duplicate configuration simply because dbt stored it separately.

The translator should therefore perform semantic compression wherever behaviour is unambiguous.

## Input discovery

Recognise standard dbt project inputs including:

- `dbt_project.yml`;
- configured model paths;
- SQL model files;
- schema/property YAML files;
- source declarations;
- singular SQL tests;
- seeds metadata where present;
- snapshots metadata where present;
- macros;
- package dependencies;
- target/profile references where useful for analysis.

The translator must honour dbt-configured source/model directories rather than assuming only `models/`.

## Initial conversion coverage

### SQL models

Convert standard SQL models to Phlo SQL models.

Preserve SQL that is already portable. Resolve dbt-specific constructs before emission where possible.

### `ref()`

Convert:

```sql
{{ ref('stg_samples') }}
```

into the resolved native Phlo model relation where resolution is deterministic:

```sql
staging.stg_samples
```

For two-argument/versioned/project references, resolve the actual dbt graph target before emitting a Phlo identifier.

The translator must not replace `ref()` using string search alone. Resolution must use the parsed dbt project graph/context.

### `source()`

Convert:

```sql
{{ source('lims', 'samples') }}
```

into the appropriate native external relation.

Where dbt's logical source name differs from its physical catalog/schema/table name, use the resolved physical relation and retain useful metadata separately.

### Materialisations

Translate common materialisations:

| dbt | Phlo Transform |
|---|---|
| `view` | `@view` or inherited default |
| `table` | `@table` |
| `incremental` | native `@incremental ...` strategy |
| `ephemeral` | inline/CTE conversion if semantically safe; otherwise review |

Do not emit directives when the target behaviour equals the inherited Phlo default.

### Incremental models

Recognise common dbt incremental patterns and convert their intent where possible.

Examples:

```yaml
materialized: incremental
unique_key: experiment_id
```

becomes:

```sql
-- @incremental key=experiment_id
```

A common `is_incremental()` timestamp predicate may be translated into a native time-window strategy where semantics are equivalent.

Complex custom incremental strategies require review rather than emulation.

### Tests

Translate common generic tests:

- `unique`;
- `not_null`;
- `relationships`;
- `accepted_values` where a native assertion exists;
- simple expression checks where safely convertible.

Prefer inferred/native contracts over separate generated test definitions.

Singular SQL tests should normally become Phlo SQL tests that return violating rows.

### Sources

Source declarations should be reduced to metadata only where Phlo can infer the source relation from SQL/catalogue state.

Preserve useful metadata such as:

- description;
- owner where available;
- freshness;
- timestamp column;
- relevant column descriptions/contracts.

Do not emit declarations solely to prove that a source exists.

### Tags and metadata

Translate model tags and useful ownership/documentation metadata.

Avoid carrying dbt metadata that has no Phlo behaviour or useful documentation purpose.

### Folder/project configuration

Translate `dbt_project.yml` model hierarchy into the smallest equivalent Phlo workspace/root/folder configuration.

The translator should actively collapse redundant inherited dbt configuration.

Example:

If every staging model inherits `materialized: view` and `view` is already the Phlo default, emit no staging materialisation config.

## Translation classification

Every dbt resource must receive one of three statuses:

### CLEAN

The translator can prove an equivalent native Phlo representation.

### REVIEW

A useful translation can be generated, but semantic equivalence cannot be proven or some manual choice is required.

### UNSUPPORTED

The resource depends on behaviour Phlo intentionally does not implement or cannot safely translate.

The tool must never silently classify uncertain behaviour as CLEAN.

## Analysis report

`--check` should produce a migration report before writing files.

Example:

```text
dbt migration analysis

Resources
  models                 184
  sources                 23
  generic tests          612
  singular tests          14
  macros                   17

CLEAN
  models                 171
  sources                 23
  tests                  593

REVIEW
  models                  13
  tests                   19
  macros                   8

UNSUPPORTED
  macros                   9

Model conversion coverage: 92.9%

Review reasons
  7 custom incremental macro usage
  3 adapter.dispatch usage
  2 dynamic relation generation
  1 unsupported materialisation
```

JSON output must expose the same information structurally.

## Migration manifest

A translation should generate a machine-readable migration manifest, for example:

```text
.phlo/migration/dbt-translation.json
```

It should include:

- source dbt resource ID;
- original file/path;
- emitted Phlo model ID;
- emitted path;
- classification;
- transformations applied;
- configuration removed as redundant;
- warnings;
- unsupported constructs;
- manual actions required.

This allows migration review to be audited and rerun deterministically.

## Jinja and macros

This is the primary boundary of the feature.

### Automatically convertible examples

- `ref()`;
- `source()`;
- basic `var()` where a native variable is still necessary;
- common `is_incremental()` patterns that map directly to native incremental strategies;
- trivial conditional SQL where it can be statically evaluated from known project configuration.

### Review examples

- project macros that generate predictable SQL but have no direct native equivalent;
- moderate Jinja loops;
- environment-dependent SQL generation;
- packages used for convenience functions.

### Unsupported by default

- arbitrary macro execution needed at runtime;
- `adapter.dispatch()` behaviour with meaningful adapter-specific implementations;
- custom materialisations implemented as macros;
- Python-dependent Jinja behaviour;
- macros with side effects;
- dynamic dependency generation that cannot be resolved statically.

The translator may inspect/execute dbt parsing logic in a sandboxed migration context if later required, but generated Phlo projects must not depend on a dbt runtime.

## Packages

Package dependencies should be analysed by actual usage.

Do not blindly reproduce package dependencies.

For each used package macro:

1. recognise known patterns with native Phlo/SQL equivalents;
2. inline/translate simple SQL functionality where safe;
3. mark remaining usages REVIEW or UNSUPPORTED.

A future compatibility library may provide explicit equivalents for common package utilities, but this must not expand into general dbt macro compatibility.

## Seeds

Initial handling:

- recognise dbt seed resources;
- preserve CSV files where useful;
- emit a migration recommendation or native source/static-data representation;
- classify CLEAN only after Phlo defines a stable native seed/static-data concept.

Do not invent a permanent seed abstraction solely for dbt compatibility.

## Snapshots

Snapshots should initially be classified REVIEW/UNSUPPORTED unless their behaviour maps cleanly to a native Phlo historical/versioned-data concept.

Do not implement dbt snapshot semantics inside the translator simply to raise migration coverage.

## Profiles and targets

`profiles.yml` should not be translated directly into a dbt-style Phlo profile system.

Use it only to help identify:

- adapter type;
- database/catalog names;
- schema conventions;
- environment names.

Emit native Phlo target/environment configuration instead.

Credentials must never be copied into generated files unless an explicit secure migration mechanism is designed later.

## Multi-project / multi-folder migration

Migration should take advantage of Phlo's workspace model rather than recreating multiple isolated dbt projects.

Where appropriate, several dbt projects may be translated into a single workspace containing roots such as:

```text
transforms/shared/
workflows/assay/transforms/
workflows/manufacturing/transforms/
```

Cross-project `ref()` relationships can become normal cross-root workspace dependencies.

This is a feature of migration, not merely compatibility.

## Output layout

The translator should be able to emit into a chosen transform root or preserve logical groupings.

Example:

```text
transforms/
├── staging/
├── intermediate/
└── marts/
```

or, with explicit mapping:

```text
workflows/
├── assay/
│   └── transforms/
└── analytics/
    └── transforms/
```

Do not infer workflow ownership from folder names unless a deterministic mapping is available or provided by the user.

## Idempotence

Running translation repeatedly against the same dbt project and translator version should produce equivalent output.

The migration manifest should record source hashes and translator version.

A future update mode may reconcile changes from a dbt project during a staged migration, but one-way translation is the initial goal.

## Diagnostics

Every non-clean resource must explain why.

Example:

```text
review[DBT014]: custom incremental macro cannot be mapped safely

model: finance.orders
file: models/finance/orders.sql
macro: incremental_where

Detected behaviour:
  adapter-specific predicate generation

Suggested Phlo representation:
  consider `@incremental window=updated_at`

No file was silently converted as equivalent.
```

Use stable diagnostic codes for programmatic consumption.

## CLI details

Proposed options:

```text
phlo transform translate
  --from dbt
  <input>
  --output <path>
  --check
  --write
  --json
  --include <selector>
  --exclude <selector>
  --fail-on review|unsupported
```

`--check` and `--write` should be mutually clear; analysis without `--write` is the safe default.

## Internal architecture

Keep importer-specific types outside the core compiler.

Suggested conceptual boundary:

```rust
trait ProjectFrontend {
    fn load(&self, source: &Path) -> Result<ImportedProject>;
}

struct ImportedProject {
    models: Vec<ImportedModel>,
    sources: Vec<ImportedSource>,
    tests: Vec<ImportedTest>,
    diagnostics: Vec<MigrationDiagnostic>,
}
```

A lowering step converts importer output into the same semantic model used by native Phlo discovery/parsing where possible.

Do not contaminate core model types with dbt-specific fields.

## Tests

### Fixture projects

Maintain representative dbt fixture projects covering:

- simple `ref()` chains;
- sources;
- table/view models;
- incremental + unique key;
- schema tests;
- singular tests;
- config inheritance;
- Jinja conditionals;
- macro usage;
- package macro usage;
- ephemeral models;
- ambiguous/unsupported custom behaviour.

### Golden output

Use snapshot/golden tests to verify generated native Phlo projects and migration reports.

### Semantic verification

Where possible, run equivalent dbt and translated Phlo models against the same DuckDB/Trino fixture data and compare outputs.

This should become the strongest CLEAN classification test: claimed automatic conversions should produce equivalent results on representative fixtures.

## Acceptance criteria — initial translator

The first useful release is complete when:

1. a standard dbt project can be analysed without modifying it;
2. dbt models, sources, refs, standard materialisations and common tests are discovered correctly;
3. simple `ref()` and `source()` usage is emitted as native Phlo relations;
4. redundant dbt configuration is omitted from output;
5. table/view models translate cleanly;
6. basic incremental + unique-key models translate to native incremental semantics;
7. common unique/not-null/relationship tests are translated or inferred rather than duplicated;
8. every resource is classified CLEAN, REVIEW or UNSUPPORTED;
9. unsupported macros/Jinja are never silently treated as equivalent;
10. `--check --json` exposes a complete structured migration report;
11. `--write` emits a valid Phlo workspace that passes `phlo transform check` for CLEAN fixtures;
12. translation is deterministic and covered by integration/golden tests.

## Later opportunities

Once the importer boundary exists, other frontends could be added without changing the core engine, for example:

- SQLMesh project import;
- plain SQL folder import;
- legacy warehouse script analysis;
- generated migrations from other declarative transformation systems.

These are opportunities, not roadmap commitments.

## Non-goals

- running dbt projects directly as a permanent compatibility mode;
- full Jinja compatibility;
- full dbt macro compatibility;
- reproducing dbt's adapter-dispatch system;
- guaranteeing automatic conversion of arbitrary custom dbt projects;
- preserving every dbt configuration file or directory structure;
- maximising reported migration percentage at the cost of native Phlo simplicity.

## Design rule

When translation encounters a dbt feature, ask:

> What behaviour is the project trying to express?

Translate that behaviour into the smallest native Phlo representation.

Do not ask:

> How do we reproduce this dbt feature inside Phlo?
