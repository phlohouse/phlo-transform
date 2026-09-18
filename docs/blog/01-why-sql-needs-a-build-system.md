# 1. Why does SQL need a build system?

Start with the smallest possible data transformation: one SQL query.

```sql
select
    occurred_at::date as day,
    kind,
    count(*) as events
from landing.events
group by 1, 2
```

If you run that once in a console, look at the result, and throw it away, you do not need Phlo Transform. You do not need dbt either. You barely need a file.

The interesting problems begin when the result stops being temporary.

## A query becomes a model when other things rely on it

Suppose we save the result as `marts.daily_events` because a dashboard needs it every morning.

Now we have two things:

- a **definition**: the SQL above;
- a **materialisation**: the table or view produced from that definition.

That distinction is fundamental. The SQL is what we *want*. The relation in the warehouse is what currently *exists*.

A transformation tool has to keep those two things aligned.

Then another query reads `marts.daily_events`:

```sql
select
    date_trunc('week', day) as week,
    sum(events) as events
from marts.daily_events
group by 1
```

We now have a dependency:

```text
landing.events
      │
      ▼
marts.daily_events
      │
      ▼
marts.weekly_events
```

Nothing created that graph for us. It already existed the moment one query read another result.

This is the first useful idea in Phlo Transform:

> **The dependency graph is a property of the SQL, not of the tool.**

The tool's job is to discover it accurately.

## What is a relation?

A SQL engine works with named things such as tables and views. In this series, we will use **relation** as the general term for one of those queryable objects.

For example:

```text
landing.events
marts.daily_events
analytics.customer_value
```

A relation may be an external input that Phlo Transform does not build, or it may be the physical output of a model that Phlo Transform does build.

That gives us two more useful terms:

- a **source** is an input relation that exists outside the transformation graph;
- a **model** is SQL owned by the workspace that produces a relation.

If model B reads model A, A is upstream of B and B is downstream of A.

## Why scheduling alone is not enough

A simple scheduler could run model A and then model B every hour. That works until the system changes.

Imagine someone reformats `daily_events.sql` without changing what it computes. Should the table rebuild?

Now imagine they change:

```sql
occurred_at::date
```

to:

```sql
date_trunc('day', occurred_at at time zone 'UTC')
```

That may be a real semantic change. `daily_events` is now stale, and `weekly_events` may be stale too.

Or perhaps the SQL is unchanged but the upstream source moved to a new Iceberg snapshot. The model definition did not change, but its input did.

Or perhaps the table was modified outside the transformation tool after the last successful run. The tool's state says one thing while the warehouse holds another.

A scheduler cannot answer these questions. It knows *when* to run commands, not *why* an output is valid.

## This is a build-system problem

Software build systems already solve the same class of problem.

A compiler does not rebuild an entire program merely because a file's whitespace changed. It understands source files, dependencies, outputs and invalidation.

A SQL transformation system needs the same concepts:

1. **Source language** — the SQL files people write.
2. **Compiler** — parses those files and understands what they mean.
3. **Dependency graph** — which model reads which model or source.
4. **Build state** — what was previously materialised and from which inputs.
5. **Plan** — what needs to happen now, and why.
6. **Executor** — performs the required physical work.
7. **Evidence** — proves what was actually built and tested.

The warehouse is not the build system. It is the execution target.

## What does “materialise” mean?

To **materialise** a model means to turn its SQL definition into a durable warehouse object.

The simplest choices are:

- **view** — store the query definition; compute rows when queried;
- **table** — run the query and store its result;
- **incremental table** — update only the part that needs changing.

For a table model, a build might conceptually execute:

```sql
create or replace table marts.daily_events as
select ...
```

For an incremental model it might perform an `INSERT`, `MERGE`, partition replacement or time-window update instead.

The important point is that the model describes *what the data means*. The engine should own the mechanics of producing it.

## Desired state and current state

Phlo Transform's planning model becomes much easier to understand if you separate two worlds.

### Desired state

The compiler calculates what each model should look like now:

- its canonical SQL semantics;
- its configuration;
- its contract;
- the exact versions of upstream models;
- observed source states;
- the compiler semantics version.

These inputs produce a content-addressed model version.

### Current state

The state store records what Phlo Transform previously materialised:

- model version;
- environment;
- physical target;
- adapter;
- producing run;
- materialisation timestamp;
- and, where the adapter can prove it, a strong physical output identity such as an Iceberg snapshot id.

A plan is the comparison between desired and current state.

That comparison produces a small vocabulary:

```text
BUILD   the required output is not currently proven to exist
SKIP    this environment already has the required verified output
CACHED  the required verified output already exists and can be adopted here
```

We will unpack all three later. For now, notice the important property: these are decisions backed by evidence, not merely commands.

## The basic Phlo Transform loop

A local workspace can start with no external infrastructure:

```bash
phlo-transform init
phlo-transform check
phlo-transform plan --adapter duckdb
phlo-transform run --adapter duckdb
```

`check` answers:

> Can the workspace be compiled into a coherent model?

`plan` answers:

> Given the compiled workspace and the current state, what would happen?

`run` performs the plan and then runs the relevant tests.

The same compiler and engine can target Trino, Iceberg and Nessie for the lakehouse path. The execution substrate changes; the model of the workspace does not.

## Why not “just run the SQL”?

Because once transformations are shared, stored and changed over time, the hard questions are no longer SQL syntax questions:

- What depends on this model?
- Which outputs became stale?
- Did the model change, or only its formatting?
- Did an input change?
- Does the physical table still match what we recorded?
- Can an existing output be reused safely?
- Which tests passed against the exact candidate we want to publish?
- Did production move after we audited it?
- What evidence authorised a promotion six months later?

Those are build-system questions.

Phlo Transform starts from that premise and tries to remove everything that does not follow from it.

The next step is the source language itself. If SQL already contains relation names and expressions, how much additional syntax does a compiler actually need?

*Next: [SQL should stay SQL](02-sql-should-stay-sql.md).*