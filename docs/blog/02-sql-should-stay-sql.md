# 2. SQL should stay SQL

Here is a complete, working Phlo Transform model:

```sql
select
    experiment_id,
    sample_id,
    result
from assay.raw_results
```

No `{{ config() }}`. No `ref()`. No YAML file describing the model. No
schema declaration. This file is the model definition and the exact SQL
that executes. If `assay.raw_results` is another model in the workspace,
the compiler sees that and creates the dependency itself.

## Why `ref()` exists at all

`ref('stg_orders')` does two jobs in dbt. It declares a dependency edge,
and it resolves the physical relation name. Both are workarounds. A SQL
parser already knows which relations a query reads, so
`from staging.stg_orders` *is* a dependency edge, written in a standard,
greppable way that every other tool understands. Physical naming is what a
compiler is for.

The cost of `ref()` is more than aesthetics. Once dependencies go through a
template function, the file stops being SQL. No editor, linter, or query
engine can read it. Then every other feature piles onto the same templating
layer: macros, `var()`, `{{ this }}`, `is_incremental()`. The model becomes
a program you must execute to understand.

## What inference looks like in practice

Given this staging model:

```sql
-- transforms/staging/stg_orders.sql

select
    id as order_id,
    customer_id,
    amount,
    status,
    ordered_at
from raw.orders
```

Phlo reads `from raw.orders`, finds no model by that name in the workspace,
and records `raw.orders` as an external source. Downstream:

```sql
-- transforms/marts/customer_orders.sql

select
    c.customer_id,
    c.name,
    count(o.order_id) as order_count
from staging.stg_customers c
left join staging.stg_orders o
    on o.customer_id = c.customer_id
group by c.customer_id, c.name
```

The two `staging.*` names resolve to models, and the edges appear. Output
of `phlo-transform list`:

```text
Models (5)
  marts.customer_orders        [table] marts.customer_orders <- staging.stg_customers, staging.stg_orders
  marts.daily_revenue          [incremental] marts.daily_revenue <- staging.stg_orders
  marts.orders_incremental     [incremental] marts.orders_incremental <- staging.stg_orders
  staging.stg_customers        [view] staging.stg_customers <- raw.customers
  staging.stg_orders           [view] staging.stg_orders <- raw.orders

Sources (2)
  raw.customers
  raw.orders
```

Each row shows the logical name, the materialisation, the physical target,
and what the model reads.

Names are logical. `staging.stg_orders` means "the model `stg_orders` in
the `staging` namespace", and the folder under `transforms/` provides the
namespace. Move the file and the identity moves with it. Physical naming is
the adapter's concern.

## Inference has to be honest

Inference only works when the compiler refuses to guess. Two models that
could satisfy one relation name are an error: `check` fails and tells you.
Unparseable SQL is a diagnostic with a file and a position, not a fallback
to text matching. Ambiguity-as-error is what keeps plain SQL safe as the
dependency primitive.

## What you give up, and what you get back

No Jinja means no macro calls, no `env_var()` branching, no metaprogramming.
That constraint is real, and it is deliberate. A model that needs a
template engine to be read cannot be audited, diffed, or verified
statically. In exchange, every Phlo model parses, its graph is derivable,
and the compiler can answer questions like "what breaks if
`staging.stg_orders` changes?" without executing anything.

When a model does need directives, they live in comments. The file stays
valid to any SQL tool:

```sql
-- @incremental key=order_id
-- @tags mart

select ...
```

*Next: [One workspace, one graph](03-one-workspace-one-graph.md).*
