# The Phlo Transform series

A blog series explaining what Phlo Transform is, starting from first
principles: not "a dbt alternative" as a marketing line, but *why* a SQL
build system exists at all, which problems are fundamental and which are
accidental, and what Phlo does differently.

| # | Post | Question it answers |
|---|---|---|
| 1 | [Why does SQL need a build system?](01-why-sql-needs-a-build-system.md) | What actually goes wrong when transformations are "just queries" |
| 2 | [SQL should stay SQL](02-sql-should-stay-sql.md) | Why `ref()` and Jinja are accidental complexity, and how dependency inference works |
| 3 | [One workspace, one graph](03-one-workspace-one-graph.md) | How the whole repository compiles into a single typed DAG |
| 4 | [Plan before you apply](04-plan-before-apply.md) | Content-addressed state and why "what changed" is a compiler question |
| 5 | [Incremental models without macros](05-incremental-without-macros.md) | Declaring intent instead of writing materialisation code |
| 6 | [Safe by default: branches, audits and diffs](06-safe-by-default.md) | Write-Audit-Publish on Nessie branches and data diffs as gates |
| 7 | [Leaving dbt without losing your work](07-leaving-dbt.md) | Semantic translation, honest classification, and running the result on DuckDB |

The series assumes you can read SQL. Everything else is built up from
scratch. Posts are written against the real CLI. Every command shown runs
today, and where behaviour is partial, the post says so.
