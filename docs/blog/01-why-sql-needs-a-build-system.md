# 1. Why does SQL need a build system?

Start with one query. You have a table of raw events and you want a daily
count:

```sql
select
    occurred_at::date as day,
    kind,
    count(*) as n
from landing.events
group by 1, 2
```

Run it once in a console, paste the result into a dashboard, done. No build
system needed. So where does the complexity come from?

## The moment it stops being one query

The query becomes a system when three things happen:

1. **The result is stored.** Now it is a table, `marts.daily`, that other
   queries read, and someone has to refresh it on a schedule.
2. **Other queries depend on it.** A weekly rollup reads `marts.daily`. A
   churn model reads the rollup. You have a dependency graph whether you
   wrote it down or not.
3. **The query changes.** Someone edits the definition. Which downstream
   tables are stale now? Does a cosmetic edit justify a full rebuild?

None of these problems are about SQL. A compiler and a build system solve
the same problems for code: dependencies, incremental rebuilds, and knowing
what a change invalidates. A transformation layer is a build system whose
source language happens to be SQL.

## The accidental complexity tax

Existing tools answer these questions, but with machinery on top:

- You declare dependencies with a special function, `ref('model')`, instead
  of letting the tool read the `from` clause you already wrote.
- Models carry Jinja templates, so the file you edit is not the SQL that
  runs. The file is a program that produces SQL, and you cannot know what it
  does without executing it.
- Incremental behaviour lives in user-editable materialisation macros:
  hundreds of lines of templated DDL per adapter.
- Correctness gates mostly do not exist. `dbt build` mutates production
  directly, and you find out what it did by watching it.

Each item is a workaround for missing structure. A tool that understands
SQL can infer the dependency. A tool that owns materialisation does not
need a macro for incremental. A tool that records what it built can tell
you what a change invalidates before it touches anything.

## What first principles look like

Strip it back. A transformation workspace needs:

- **SQL files.** The source of truth, executable as written.
- **A compiler** that parses the files into a typed dependency graph.
- **State** that records what was built, so rebuilds stay minimal and
  explainable.
- **A plan step before apply.** Every mutation follows an inspectable
  decision.
- **Tests and audits inside the run**, not as a separate workflow.
- **Promotion gated on evidence**, not on hope.

That is Phlo Transform. The rest of this series walks through each piece
and shows the workflow running end to end, including a real dbt project
translated and executed on DuckDB.

*Next: [SQL should stay SQL](02-sql-should-stay-sql.md).*
