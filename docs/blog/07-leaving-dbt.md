# 7. Leaving dbt without losing your work

The first six posts describe Phlo as if a workspace starts from scratch.

Real teams rarely start from scratch.

They already have models, tests, sources, configuration and years of accumulated decisions in another transformation system. For many teams, that system is dbt.

A replacement that requires every model to be rewritten manually is not a practical migration path.

Phlo therefore includes a one-way dbt translator:

```bash
phlo-transform translate --from dbt
```

The important word is **translator**, not compatibility layer.

## What is a dbt model actually made of?

A dbt project contains SQL-like model files plus metadata and templating semantics.

A typical model might contain:

```sql
{{ config(
    materialized='incremental',
    unique_key='order_id',
    incremental_strategy='merge'
) }}

select
    order_id,
    customer_id,
    amount,
    updated_at
from {{ ref('stg_orders') }}

{% if is_incremental() %}
where updated_at > (select max(updated_at) from {{ this }})
{% endif %}
```

There are several different ideas mixed together here:

- SQL defining the output;
- `ref()` expressing a model dependency and relation lookup;
- `config()` declaring materialisation behaviour;
- `is_incremental()` adding runtime control flow;
- `this` referring to the current target;
- Jinja acting as a general template/programming language.

A migration tool has to decide which of those ideas correspond to native concepts in the destination system.

Simply renaming template functions would preserve the syntax without simplifying the model.

## Migration should preserve intent

Phlo's translator tries to answer:

> What did this resource *mean*?

rather than:

> How can I preserve every character of the implementation mechanism?

The example above may become something like:

```sql
-- translated from dbt model `orders_incremental`
-- @incremental key=order_id

select
    order_id,
    customer_id,
    amount,
    updated_at
from staging.stg_orders
```

Several dbt mechanisms disappear because Phlo has native representations for the intent.

### `ref()` becomes a normal logical relation

```sql
{{ ref('stg_orders') }}
```

becomes:

```sql
staging.stg_orders
```

The compiler infers the dependency from SQL after translation.

### `source()` becomes a source relation

A declared dbt source is lowered to the relation name Phlo's compiler and adapter can resolve.

### `config()` becomes native model configuration

Materialisation, tags, keys and other supported configuration become directives or generated Phlo config rather than runtime templates.

### Incremental control flow becomes a strategy

If the translator can prove that a dbt incremental pattern means “merge by this key” or “advance by this time window”, it lowers the pattern into Phlo's native incremental strategy.

The Jinja branch is no longer needed because the engine owns incremental execution.

## Semantic compression is a feature

A translated file may be shorter than its input.

That is expected.

If five lines of template machinery were only expressing one concept already present in the destination engine, preserving all five lines would preserve accidental complexity rather than user intent.

Migration should preserve behaviour that matters, not implementation scaffolding that no longer does.

## But translation must be honest

General Jinja is a programming language.

A macro can execute arbitrary logic to generate SQL. Static translation cannot safely pretend to understand every possible program.

So Phlo classifies resources explicitly.

### `CLEAN`

The resource was translated to native Phlo semantics without unresolved behaviour.

### `REVIEW`

Phlo could emit a useful result, but a human needs to inspect or complete part of it.

Examples include macro call sites or patterns whose intended meaning cannot be proven statically.

### `UNSUPPORTED`

The resource has no current native translation and is not emitted as a working model.

Examples can include dbt-specific resource categories outside Phlo's model scope, such as snapshots or semantic-layer resources.

The key rule is:

> **Uncertain translation must become visible uncertainty, not plausible-looking wrong SQL.**

## Residual Jinja should fail loudly

For some `REVIEW` cases, preserving the unresolved call site is more useful than deleting it.

That means the generated file may still contain something such as a macro invocation that plain SQL cannot parse.

`--verify` compile-checks the generated Phlo workspace and exits non-zero when that residual content is not valid native SQL.

The developer receives a precise place to fix rather than a successful migration with changed semantics.

## The migration report is part of the output

A migration is not only a directory of files.

Phlo records what happened to each source resource:

```text
resource
classification
transformations applied
issues/reasons
source hash
```

The manifest under:

```text
.phlo/migration/dbt-translation.json
```

makes translation inspectable and rerunnable.

That matters for large migrations where “we converted it months ago” is not sufficient provenance.

## A practical workflow

First inspect without writing output:

```bash
phlo-transform -r my-dbt-project translate --from dbt --check
```

Then generate into a separate directory:

```bash
phlo-transform -r my-dbt-project \
    translate --from dbt \
    --out generated/ \
    --verify
```

The generated workspace can then go through the normal Phlo lifecycle:

```bash
phlo-transform -r generated check
phlo-transform -r generated plan --adapter duckdb
phlo-transform -r generated run --adapter duckdb
phlo-transform -r generated test --adapter duckdb
```

The translator ends at native Phlo files. The runtime does not carry a hidden dbt compatibility mode afterwards.

## Seeds, tests and configuration matter too

Migration is not only about model SQL.

The translator also handles supported surrounding project semantics such as:

- sources;
- seeds and seed references;
- schema tests;
- accepted-values tests;
- tags;
- folder/project configuration;
- variables that can be resolved statically;
- materialisations;
- unique keys;
- supported incremental patterns.

For example, dbt's newer `arguments:` syntax for generic tests is translated rather than requiring old YAML shapes.

Known dbt functions with clear SQL equivalents can be lowered directly where supported.

Again, the criterion is semantic certainty rather than percentage-of-syntax bragging rights.

## Does translated output actually execute?

That is the important test.

The repository carries migration fixtures that go beyond checking generated text.

`dbt-shop`, for example, includes:

- sources;
- staging views;
- a table mart;
- keyed incremental behaviour;
- time-window incremental behaviour;
- folder configuration;
- tags;
- schema tests;
- variables.

It translates `CLEAN` and runs end to end on DuckDB with no manual edits.

The validation exercises multiple runs: initial materialisation, changed source data, incremental update, and a final no-change run that plans `SKIP`.

The release review also exercised external projects rather than only bespoke fixtures. A DuckDB jaffle-shop-style project translated its models cleanly, and a larger exemplar project translated almost all models untouched, with the remaining model becoming clean once the required profile context was declared. Unsupported semantic-layer resources remained explicitly unsupported rather than contaminating model translation.

Those results are more useful than a claim that “most Jinja is supported”.

## Why not make Phlo understand all dbt macros?

Because that would turn the migration layer into the architecture of the new runtime.

If Phlo had to execute arbitrary dbt Jinja forever before compiling a model, then the core properties described earlier in this series would disappear:

- dependencies would no longer be statically knowable;
- SQL would no longer be the source program;
- compilation would depend on template execution;
- plan reasoning would become less deterministic;
- the migration would never really finish.

The translator is intentionally one-way.

```text
dbt project
    │
    ▼
semantic translation
    │
    ├── CLEAN
    ├── REVIEW
    └── UNSUPPORTED
    │
    ▼
native Phlo workspace
```

Once the workspace is native, it participates in the same compiler, state, planning, WAP and evidence model as anything written for Phlo directly.

## Migration is where first-principles design gets tested

It is easy to claim that a simpler model is elegant when there is no existing complexity to absorb.

Migration forces the design to prove that the simpler concepts are expressive enough to carry real intent:

```text
ref()                  → relation resolution
materialized config    → native materialisation
unique_key             → row identity
incremental template   → incremental strategy
schema tests           → compiled assertions
project hierarchy      → workspace/root/folder config
```

Where the mapping is real, translation removes machinery.

Where it is not real, Phlo says so.

The next post moves back under the hood and explains the execution engine itself: how a plan becomes concurrent warehouse work, how failures propagate, and why resume, retry and cancellation need their own state model.

*Next: [How the execution engine works](08-how-execution-works.md).*