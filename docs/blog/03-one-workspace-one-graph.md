# 3. One workspace, one graph

Once every SQL file has been parsed and every relation reference resolved, the repository stops being a pile of queries.

It becomes a graph.

That graph is the central data structure behind most of Phlo Transform: build order, selection, lineage, impact analysis, state propagation, tests, contracts and promotion all depend on it.

## What is a graph?

A graph is just a set of things connected by relationships.

For transformations, the things are models and sources. The relationships are dependencies.

If `assay.results` reads `assay.raw`, we draw:

```text
assay.raw ─────► assay.results
```

If a report then reads `assay.results`:

```text
assay.raw ─────► assay.results ─────► reporting.monthly
```

The direction means “this output depends on that input”.

A transformation graph is normally a **directed acyclic graph**, or DAG:

- **directed** because dependency has a direction;
- **acyclic** because a model cannot ultimately depend on itself.

A cycle such as A → B → A is not an execution-order problem to solve later. It is a contradiction in the workspace, so compilation fails.

## Why the whole workspace should be one graph

Repositories are often organised for people rather than compilers:

```text
transforms/
workflows/assay/transforms/
workflows/reporting/transforms/
```

Those folders can represent ownership, domains or application boundaries. They do not need to become separate transformation universes.

Phlo Transform discovers multiple transform roots and compiles them into one logical workspace.

For example:

```text
workflows/assay/transforms/raw.sql
workflows/assay/transforms/results.sql
workflows/reporting/transforms/monthly.sql
```

can become:

```text
assay.raw
    │
    ▼
assay.results
    │
    ▼
reporting.monthly
```

`reporting.monthly` can simply contain:

```sql
select * from assay.results
```

No package mechanism is required to make the dependency visible because the compiler already sees both sides.

Ownership remains useful metadata. Fragmenting the graph is not required to express it.

## Why graph boundaries are expensive

Suppose `assay.results` changes.

A useful system should be able to answer:

- Which models are downstream?
- Which columns are affected?
- Which tests may need to run again?
- Which planned outputs are stale?
- Which candidate data should be compared with production?

If the workspace is split into disconnected project graphs, each question has to cross those boundaries using extra manifests, package metadata or orchestration glue.

One graph means the same compiler truth feeds every feature.

## Topological order: how the graph becomes an execution order

A graph says what depends on what. The runner still needs an order that respects those dependencies.

For:

```text
A ──► B ──► D
└──► C ──► D
```

A valid order might be:

```text
A, B, C, D
```

or:

```text
A, C, B, D
```

B and C can run concurrently after A finishes. D must wait for both.

That is a **topological order**: an ordering where every node appears after its dependencies.

Phlo Transform's runner uses the graph directly rather than turning the whole workspace into one long serial list. Independent branches can execute concurrently as soon as their upstream work is satisfied.

We will return to the scheduler later.

## A useful graph contains more than model names

A dependency graph that only knows “A depends on B” is valuable, but limited.

Phlo Transform's compiler builds a semantic model of the query too.

Consider:

```sql
select
    experiment_id,
    result * dilution_factor as corrected_result
from assay.raw_results
```

At model level we know:

```text
assay.raw_results ──► assay.corrected_results
```

At column level we can know that:

```text
assay.corrected_results.experiment_id
    ← assay.raw_results.experiment_id

assay.corrected_results.corrected_result
    ← assay.raw_results.result
    ← assay.raw_results.dilution_factor
```

That is **column lineage**.

## Lineage and impact are inverse questions

Lineage asks:

> Where did this thing come from?

Impact asks:

> What depends on this thing?

For a model:

```bash
phlo-transform lineage reporting.monthly
```

can walk upstream.

For a model or column:

```bash
phlo-transform impact assay.results
```

can walk downstream.

Because both are derived from the compiler graph, they are not separate metadata that can drift from execution.

Phlo Transform can also export the canonical graph as OpenLineage-compatible events for systems that consume that standard.

## Where types come from

SQL expressions have types: integer, decimal, varchar, timestamp, boolean and so on.

Some types are inferable entirely from SQL syntax. Others depend on the schema of an external relation.

For example:

```sql
select
    sample_id,
    concentration * dilution as corrected
from raw.assay_results
```

The compiler needs to know what `concentration` and `dilution` are to infer the output precisely.

Offline, external source columns may be unknown.

With an adapter configured, Phlo Transform can ask the real catalogue for relation columns and enrich the compilation.

This is an important boundary:

```text
compiler truth available from files
           +
optional observed warehouse schema
           =
richer semantic model
```

The compiler remains separate from the warehouse adapter; enrichment is an input, not a hidden execution side effect.

## What is a contract?

A contract is an explicit promise about a model's output schema.

For example, a downstream application may rely on:

```text
sample_id          VARCHAR   NOT NULL
result             DOUBLE
run_date           DATE
```

If a model silently removes `sample_id` or changes it to an incompatible type, the SQL might still execute successfully while consumers break.

A schema contract turns that implicit dependency into something the system can check.

Contracts become part of the model version and part of promotion auditing. A breaking contract change can block promotion unless it is explicitly allowed.

Declared renames can distinguish:

> this column disappeared

from:

> this column intentionally moved from `old_name` to `new_name`.

That is more useful than comparing opaque schema hashes.

## Keys are semantic information too

A key says which columns identify a row.

```sql
-- @key experiment_id,sample_id
select ...
```

For a composite key, the pair is unique; the individual columns do not need to be unique by themselves.

Phlo Transform turns the declaration into several consistent behaviours:

- not-null assertions for the key columns;
- one composite uniqueness assertion;
- row identity for keyed diffs;
- merge identity for keyed incremental models where applicable.

One declaration drives several features because they are all asking the same semantic question: *what identifies a row?*

## Tests are nodes around the graph, not comments in documentation

A generated or custom test is executable SQL with declared targets and sources.

Examples include:

- `not_null`;
- `unique`;
- accepted values;
- custom SQL assertions.

Tests run against actual materialised outputs. A model can execute successfully and still fail its tests, which causes the overall run to fail without pretending the SQL execution itself failed.

That distinction matters later for retries and promotion evidence.

## Multi-root does not mean “anything can depend on anything”

A single graph is not the same as having no boundaries.

Ownership and policy can still restrict which namespaces or workflows may consume others. The important architectural choice is that those rules operate *on one known graph* rather than hiding edges behind separate build systems.

This gives us a clean mental model:

```text
repository
   │
   ▼
compiler
   │
   ▼
one typed graph
   │
   ├── build order
   ├── lineage
   ├── impact
   ├── tests
   ├── contracts
   ├── version propagation
   └── promotion analysis
```

The next question is what happens over time. Once yesterday's graph has produced physical tables and today's graph has changed, how does Phlo Transform decide what to build, skip or safely reuse?

That is a state and planning problem.

*Next: [Plan before you apply](04-plan-before-apply.md).*