# 4. Plan before you apply

A compiler tells us what the workspace means now. A warehouse contains whatever was built in the past.

A transformation engine needs to reconcile those two worlds without guessing.

That is the purpose of **state** and **planning**.

## Start with the simplest question

Suppose this model exists:

```sql
select
    sample_id,
    result
from assay.raw_results
```

Yesterday, Phlo Transform built it successfully.

Today, should it run again?

A timestamp is not enough to answer that. Neither is “the file changed” or “the table exists”.

The real question is:

> **Does the physical output we have now still prove that it represents the model we want now?**

Phlo Transform answers that question before execution.

## What is state?

State is a durable record of what Phlo Transform has observed and produced.

The default local store is SQLite:

```text
.phlo/transform/state.db
```

A shared Postgres backend is also available for workflows where different CI stages or machines need to see the same run and audit history.

State contains things such as:

- runs and their status;
- per-model execution results;
- stored plans;
- model versions that were materialised;
- the physical target used;
- adapter identity;
- time-window watermarks;
- seed versions;
- promotions;
- portable audit evidence.

For adapters that can prove physical identity, a materialised record also carries an **output identity**.

For Iceberg, that is the snapshot id.

## A model version is a statement about meaning

Phlo Transform computes a content-addressed version for every compiled model.

The version incorporates the pieces that can change what the model computes:

```text
canonical SQL semantics
configuration
contract
dependency versions
source states
content slot/materialisation
compiler semantics version
```

A few details are worth unpacking.

### Canonical SQL semantics

The version is derived from parsed/canonical semantics rather than source bytes, so harmless formatting does not create a new data version.

### Dependency versions

If an upstream model changes, a downstream model's desired version changes even if the downstream SQL file itself is untouched.

That is how invalidation propagates through the graph.

### Source states

External inputs can move without SQL changing.

An Iceberg source can be represented by a snapshot identity. DuckDB uses a more limited schema/row-count fingerprint. If an observed source state moves, models that consume it become stale.

### Compiler semantics version

Sometimes the compiler itself changes how the same source program is interpreted. Phlo Transform carries an explicit compiler-semantics version so an upgrade can trigger a one-time honest rebuild rather than silently treating old and new semantics as equivalent.

### Why the catalogue is not in the model version

This became important for v0.1.

Consider the same logical model materialised through two Nessie-bound catalogs:

```text
phlo_main.analytics.assay__results
phlo_ci_42.analytics.assay__results
```

The catalog says **where an environment sees the model**. It does not change what the model computes.

So the version hashes the content slot — effectively `schema.table|materialisation` — rather than the environment's catalog binding.

Physical target movement is still detected separately. Removing the catalog from semantic identity does not mean Phlo Transform ignores where data lives.

## Desired state versus recorded state

For each model, the planner has two sides.

### Desired

From the current compilation:

```text
version = abc123
physical target = phlo_ci_42.analytics.assay__results
```

### Recorded

From state:

```text
version = abc123
physical target = phlo_main.analytics.assay__results
adapter = trino
output identity = snapshot:918273645
```

It also asks the adapter what exists *right now*.

That last step is critical. State is evidence from the past. The warehouse is allowed to have changed since then.

## The three normal actions

Phlo Transform plans every model as one of three meaningful actions.

### `BUILD`

Phlo Transform cannot prove the required output already exists safely.

Reasons include:

- target relation missing;
- SQL semantics changed;
- config changed;
- contract changed;
- upstream dependency changed;
- source state changed;
- adapter changed;
- output identity drifted;
- physical target moved and the old output is not verifiably present there;
- `--force` was requested.

### `SKIP`

This environment's own record already vouches for the desired version at this target, and any strong physical identity still matches live.

Nothing needs to execute.

### `CACHED`

The desired output was recorded elsewhere, and this environment can prove it already sees the same physical output.

This is not “we found the same hash in a database, so let's trust it”. It is **verified adoption**.

That distinction deserves its own section.

## What does cache reuse mean for data?

In a traditional build cache, a compiler may copy a previously built artifact into the current build.

A lakehouse can sometimes do something better: the target environment already sees the same immutable physical object.

Nessie makes this common.

Imagine `main` contains an Iceberg table at snapshot `S1`:

```text
main
└── assay.results → snapshot S1
```

Create a candidate branch from `main`:

```text
main                candidate
└── results S1      └── results S1
```

Before the candidate changes anything, both references can see the inherited snapshot.

If the compiled candidate wants the same model version, Phlo Transform can ask:

1. Is there a materialised record for this exact model version elsewhere?
2. Was it produced by the same adapter semantics?
3. Does it describe the same content slot?
4. Does it carry a strong output identity?
5. Does the candidate's own target report that same identity live?

If all of those are true, the candidate does not need to run model SQL.

It can adopt the output.

## Adoption is an execution action

`CACHED` is not just a planner label.

The plan carries provenance for the reusable output:

```text
source environment
source physical target
output identity
producing run id
materialised-at timestamp
```

Immediately before adoption, the runner asks for the target's live output identity **again**.

Why twice?

Because time exists between planning and execution.

A table could change after the plan was created.

If the identity no longer matches, the runner changes course and performs a full build with a `cache_miss` reason. It does not adopt stale evidence.

If the identity still matches, Phlo Transform writes an environment-local materialisation record pointing at the verified output.

It preserves the original producing run id and materialisation timestamp. Reuse must not pretend that this run physically created data it did not create.

For time-window incrementals, the matching watermark is adopted too because identical content has the same processing frontier.

After adoption, the candidate has its own state record, so the next no-op plan is a normal `SKIP` rather than another cross-environment cache lookup.

## Physical identity is stronger than “the table exists”

Consider this failure mode:

```text
state says assay.results = version abc123
warehouse table exists
someone rewrites the table outside Phlo Transform
```

If the planner checked only the relation name, it could incorrectly skip.

For Iceberg, Phlo Transform records the snapshot id after a successful build and checks it again on later plans.

If state says:

```text
snapshot:100
```

and live storage says:

```text
snapshot:104
```

then the record no longer proves the table contains Phlo Transform's recorded output.

The model rebuilds.

This is why warm planning is not free. At large scale, strong drift checks require real metadata reads. In v0.1, a 5,000-model warm Trino/Iceberg plan is dominated by per-table snapshot lookups. Phlo Transform deliberately pays that cost rather than turning “probably unchanged” into `SKIP`.

## What does a plan look like?

A first run might produce:

```text
BUILD  staging.customers
       target relation does not exist

BUILD  staging.orders
       target relation does not exist

BUILD  marts.customer_orders
       target relation does not exist
```

After a successful run:

```text
SKIP   staging.customers
       SQL, config, contract and inputs unchanged

SKIP   staging.orders
       SQL, config, contract and inputs unchanged

SKIP   marts.customer_orders
       SQL, config, contract and inputs unchanged
```

On a fresh Nessie candidate inheriting verified base outputs:

```text
CACHED staging.customers
       candidate target holds the desired snapshot recorded in main; adopting it
```

The JSON plan carries the same structured reasons for automation.

## Plan first means apply does not re-decide intent

This separation is subtle but important.

The planner decides *what should happen*.

The runner decides *how to execute that plan safely*.

The runner can reject a stale plan or downgrade an unsafe cache adoption to a build, but it does not silently invent a different desired model graph.

A plan can therefore be reviewed by a human or machine before mutation.

`run` is the convenient combined workflow:

```text
compile → plan → apply → tests
```

but the boundaries still exist internally.

## Stale plans are rejected

Suppose a plan says:

```text
model: assay.results
target: catalog_a.analytics.assay__results
version: abc123
```

Then the workspace is recompiled against `catalog_b` before apply.

Because catalog identity is intentionally outside the semantic version hash, `abc123` may still match.

That does **not** make the old plan valid.

Plan staleness checks the full physical target too. A plan inspected against one target cannot silently execute against another.

The same principle applies when the model's desired version changes after planning.

## `--since` reduces the problem before planning it

Sometimes we already know which source files changed relative to Git.

```bash
phlo-transform plan --since main
```

Phlo Transform maps the Git change set back into model identity, expands the necessary dependency closure, and plans only the affected subset.

This matters at scale because expensive physical verification is only needed for the selected work.

The graph and state system cooperate:

```text
Git change
   │
   ▼
changed model set
   │
   ▼
dependency/impact closure
   │
   ▼
state-aware plan
```

## `inspect` is the same accounting at one-model scale

For one model, the useful view is:

```text
Desired version:  abc123
Current version:  abc123
Status:           unchanged
Target:           ...
Adapter:          ...
Output identity:  ...
```

The point is not the formatting. It is that `inspect`, `explain`, `plan` and `run` all derive from the same compiler and state model rather than each inventing their own definition of “changed”.

## Why this matters beyond performance

Minimal rebuilds are useful, but the deeper benefit is accountability.

For every model, Phlo Transform can explain:

- what it wants;
- what it previously built;
- what it can currently verify;
- which input moved;
- why the chosen action follows.

That foundation is what makes incrementals, branch isolation and promotion evidence tractable.

The next post looks at incremental materialisation specifically: how to update only part of a table without putting control-flow programs inside model SQL.

*Next: [Incremental models without macros](05-incremental-without-macros.md).*