# 5. Incremental models without macros

Incremental models are where transformation tools earn or lose their keep.
A full rebuild is always correct and always expensive. An incremental run
is cheap and correct only when the machinery underneath knows exactly what
"new data" means for that model.

In dbt, the machinery is yours. You write `{{ config(materialized =
'incremental') }}`, then guard the expensive part by hand with
`{% if is_incremental() %} ... where ts > (select max(ts) from {{ this }})
{% endif %}`, and the adapter's materialisation macro decides what DDL to
emit. The intent, "merge by key" or "append rows past a watermark", is
buried in templated control flow.

## Declaring intent instead

Phlo makes incremental a declaration on the model. Four strategies cover
the common shapes:

```sql
-- @incremental append
-- @incremental key=experiment_id
-- @incremental partition=run_date
-- @incremental window=updated_at
```

Or the same in `phlo.toml`:

```toml
[model.events.incremental]
strategy = "time-window"
column = "updated_at"
overlap = "2h"
```

The strategy becomes part of the model's config, which makes it part of the
model's version. Change the key and the plan notices:

```text
  BUILD  marts.orders_incremental [incremental] marts.orders_incremental
           reason: config changed
           strategy: key
           full rebuild required
```

## What the engine does with the declaration

- **Bootstrap or full rebuild:** `create table ... as`, or the equivalent
  replace.
- **append:** `insert into <target> <select>`.
- **key:** a merge on the key columns, emulated as delete + insert where the
  engine has no `MERGE` (DuckDB). Verified against Trino and Iceberg,
  including idempotent re-application.
- **partition:** delete the partitions that the source touches, then insert.
  Other partitions stay untouched.
- **time-window:** append rows where the window column exceeds the last
  committed watermark, optionally backed off by `overlap`, then advance the
  watermark to `max(column)` on success only.

The engine picks the physical operation. Adapters implement `append`,
`merge`, `replace_partitions`, and `create_or_replace_table`. No
materialisation SQL lives in compiler structures, and none lives in your
project.

## Watermarks commit on success

A subtle point that matters in production: the watermark only advances when
the build succeeds. A failed run records no materialisation and no new
watermark, so the next plan still sees the old state and retries the same
window. Failed runs do not corrupt incremental progress.

## What you lose with the macro gone

The macro's disappearance is the feature. A watermark filter written in
Jinja can drift, can reference the wrong column, and can hide an `else`
branch that changes semantics between full and incremental runs. A declared
`window=ordered_at` has one meaning, the engine implements it once per
adapter, and the plan prints it before anything executes.

The honest limit: unusual incremental shapes that do not map to a strategy
degrade to a full rebuild. That is always correct, sometimes slower, and
the plan says `full rebuild required` so the trade-off is visible rather
than silent.

*Next: [Safe by default: branches, audits and diffs](06-safe-by-default.md).*
