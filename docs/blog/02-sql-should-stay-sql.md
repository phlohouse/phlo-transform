# 2. SQL should stay SQL

A build system needs a source language. For Phlo Transform, that language is SQL.

Here is a complete model:

```sql
select
    experiment_id,
    sample_id,
    result
from assay.raw_results
```

There is no `ref()` call declaring the dependency. There is no template layer producing another SQL string first. The file is SQL, and the SQL is the model definition.

That sounds almost trivial, but it determines a large part of the architecture.

## What does a compiler actually do here?

A compiler is often associated with turning a language such as Rust or C into machine code. More generally, a compiler takes one representation of a program, understands its structure, validates it, and produces a representation suitable for later stages.

Phlo does the same thing with a transformation workspace.

A simplified pipeline is:

```text
files
  │
  ▼
discovery
  │
  ▼
SQL parser
  │
  ▼
semantic models
  │
  ├── relation resolution
  ├── type/schema analysis
  ├── dependency graph
  ├── lineage
  ├── contracts/tests
  └── physical target resolution
  │
  ▼
compiled workspace
```

Execution comes later. The compiler should be able to answer as much as possible before a warehouse is mutated.

## Step 1: discover models

A workspace contains one or more transform roots. SQL files under those roots become candidate models.

For example:

```text
transforms/
├── staging/
│   ├── customers.sql
│   └── orders.sql
└── marts/
    └── customer_orders.sql
```

The path gives each file a stable logical identity:

```text
staging.customers
staging.orders
marts.customer_orders
```

That logical identity is not necessarily the physical table name. It is the name used inside the workspace graph.

This distinction matters because logical identity should remain meaningful even when an environment binds models to a different physical catalog.

## Step 2: parse SQL, do not scan text

Consider:

```sql
with orders as (
    select * from raw.orders
)
select * from orders
```

A text search for `orders` cannot tell whether the final `from orders` means a workspace model, an external table or the CTE defined two lines earlier.

A SQL parser can.

Phlo parses SQL into an abstract syntax tree. Relation extraction walks that tree with SQL scope rules, including CTE shadowing, aliases and nested queries.

That is why dependency inference can be a compiler feature rather than a naming convention.

## Step 3: resolve relation names

Suppose we have:

```sql
-- transforms/marts/customer_orders.sql
select
    c.customer_id,
    count(o.order_id) as order_count
from staging.customers c
left join staging.orders o
    on o.customer_id = c.customer_id
group by c.customer_id
```

The compiler sees two relation references:

```text
staging.customers
staging.orders
```

It asks the workspace registry what each name means.

If exactly one workspace model matches, the reference resolves to that model and creates a dependency edge.

If no model matches, it is an external source.

If more than one model could satisfy the name, compilation fails with an ambiguity diagnostic.

The important rule is:

> **Inference is only safe if ambiguity is an error.**

Phlo does not “pick the most likely model” and hope.

## Why no `ref()`?

In dbt, this:

```sql
from {{ ref('stg_orders') }}
```

both declares a dependency and asks the runtime to produce a physical relation name.

Those are legitimate requirements, but they do not require a template function.

The SQL already says which relation is read:

```sql
from staging.orders
```

The compiler can infer the dependency from that syntax, and it can separately rewrite the logical name to the correct physical target after resolution.

Keeping those responsibilities separate has a useful consequence: the model remains valid SQL throughout the process.

## Logical names and physical targets

A model might be written as:

```sql
select * from staging.orders
```

while the physical relation used by a particular environment is:

```text
phlo_ci_pr_42_ab12cd.analytics.staging__orders
```

The user should not have to write that physical name into the model.

The compiler knows:

1. `staging.orders` means a workspace model;
2. that model has a resolved physical target for this compilation;
3. the emitted SQL should reference that target.

So model SQL is rewritten after semantic resolution.

This is not string substitution. It is an AST-aware rewrite that follows the same scope rules as dependency extraction.

## External sources use the same physical model

Not every relation belongs to the workspace.

```sql
select * from raw.instrument_results
```

If `raw.instrument_results` is not a model, it becomes a source.

When a default catalog is configured, Phlo qualifies unqualified or two-part external sources through that catalog. This matters under Nessie: a candidate environment uses a branch-bound catalog, and reads must resolve through the same environment as writes.

Without that rule, a candidate could write its own branch while accidentally reading a source through the session's base catalog.

A source already carrying an explicit catalog stays explicit.

The compiler and execution engine share the same source-to-relation mapping, so source probing, seed loading and emitted SQL agree on what physical relation a source means.

## Directives add intent without replacing SQL

Some information is not present in a `SELECT` statement.

For example, SQL cannot tell us whether a model should be a table or view, or whether an incremental table should merge on `sample_id`.

Phlo represents that information as configuration or lightweight comment directives:

```sql
-- @table
-- @key sample_id
-- @tags assay,validated

select ...
```

or:

```sql
-- @incremental key=sample_id
select ...
```

Comments preserve an important property: the file is still parseable SQL to editors and other tooling.

The directives describe intent. They are not a second programming language.

## Canonical semantics, not source bytes

Once SQL is parsed, the compiler can reason about semantic structure rather than file text.

That is the basis of content-addressed state later in the system. Reformatting this:

```sql
select a,b from x
```

into this:

```sql
select
    a,
    b
from x
```

should not invalidate data.

A build system should care that the computation changed, not that the file's whitespace changed.

## Compilation should fail early and specifically

Because Phlo owns the compiler stage, invalid workspaces fail before execution.

Typical errors include:

- SQL parse failures;
- ambiguous model references;
- duplicate logical identities;
- physical target collisions;
- invalid directives;
- impossible contract declarations;
- dependency cycles.

Diagnostics carry source locations where possible. A broken workspace does not become a half-executed run.

This is one of the main reasons to keep SQL statically understandable: the tool can reject contradictions before they become warehouse mutations.

## What is deliberately not supported?

Plain SQL means giving up unrestricted compile-time metaprogramming inside models.

Phlo does not try to execute arbitrary Jinja to discover what SQL might emerge.

That is a constraint, but it buys strong properties:

- dependencies are derivable without executing templates;
- models can be parsed and inspected directly;
- lineage can be calculated statically;
- plans are based on known semantics;
- migration tooling can clearly classify what is and is not translatable.

The design principle is not “configuration is bad”. It is narrower:

> **Do not add configuration for facts the compiler can safely infer from the program itself.**

Once the compiler has resolved every model and source, the repository stops being a collection of SQL files. It becomes one semantic graph.

That graph is the subject of the next post.

*Next: [One workspace, one graph](03-one-workspace-one-graph.md).*