# 8. How the execution engine works

A plan says what should happen.

An execution engine has a different responsibility: make that plan happen while preserving dependency order, bounded concurrency, failure semantics and state.

Those concerns are easy to blur together. Phlo Transform keeps them separate deliberately.

```text
compiler
   │
   ▼
planner          decides what should happen
   │
   ▼
plan
   │
   ▼
runner           decides how to execute it safely
   │
   ▼
adapter          performs warehouse-specific operations
```

## The runner does not compile the workspace again

The compiler has already produced a semantic workspace.

The planner has already decided `BUILD`, `SKIP` or `CACHED` for each selected model.

The runner consumes that plan.

This separation matters because a reviewed plan should not become a different set of desired actions just because execution started later.

The runner can still reject unsafe conditions:

- the plan became stale;
- a cached output's identity drifted before adoption;
- cancellation was requested;
- a dependency failed.

But it does not silently reinterpret the model graph.

## Dependency order comes from the DAG

Suppose the plan contains:

```text
raw
├──► clean_a ──► mart
└──► clean_b ──► mart
```

`raw` must be satisfied first.

Then `clean_a` and `clean_b` are independent and can run concurrently.

`mart` becomes runnable only when both are satisfied.

The runner tracks the number of unsatisfied planned dependencies for each model. When that count reaches zero, the model is ready.

This is an **indegree scheduler**.

It gives us concurrency without violating graph order.

## Concurrency is bounded

Unlimited parallelism is rarely useful against a real warehouse.

Phlo Transform bounds concurrent model work with `--jobs`:

```bash
phlo-transform run --jobs 8
```

The scheduler can discover many ready models while a semaphore limits how many execute at once.

This distinction is useful:

```text
ready work       determined by graph
running work     limited by concurrency policy
```

The graph remains correct regardless of the chosen concurrency limit.

## Model execution has explicit states

A planned model moves through a small state machine:

```text
pending
   │
   ▼
ready
   │
   ▼
running
   │
   ├──► passed
   ├──► failed
   ├──► skipped
   ├──► cached
   ├──► blocked
   └──► cancelled
```

These outcomes are intentionally different.

### `passed`

The model executed successfully.

### `skipped`

No execution was needed because this environment already held the verified desired output.

### `cached`

No model SQL was needed because a verified existing output was adopted into this environment.

### `failed`

The model actually ran and encountered a terminal error.

### `blocked`

The model did not run because a required upstream dependency failed.

### `cancelled`

Execution was intentionally stopped before completion.

A blocked model should not be reported as if its own SQL failed. That distinction is essential when diagnosing large runs.

## Satisfied versus unsatisfied outcomes

For dependency scheduling, some states satisfy downstream requirements:

```text
passed
skipped
cached
```

Others do not:

```text
failed
blocked
cancelled
```

So if an upstream model fails, its dependents become blocked rather than executing against stale or incomplete data.

Independent branches can still continue unless fail-fast was requested.

## Two models must not race the same physical target

The logical graph can contain unusual configurations where separate planned nodes resolve to the same physical relation.

Even if graph dependencies permit concurrency, two simultaneous writers to the same table are unsafe.

The runner therefore has per-target locks for colliding physical targets.

This is a useful example of why logical scheduling and physical execution are separate layers:

```text
DAG says these nodes are independent
        │
        ▼
physical target analysis says writes collide
        │
        ▼
serialise those writes
```

## Seeds run before models that consume them

A seed is a local data file, typically CSV, loaded into a relation for models to read.

Seeds are part of execution state too.

A seed has:

- a content hash;
- a target relation;
- run status;
- attempts/failures;
- load timestamp.

If a seed load fails, models that depend on that source are blocked.

The Trino adapter now supports CSV seed loading directly: it parses RFC 4180 CSV, infers safe basic column types, creates the target and inserts rows in batches.

The local DuckDB path loads seeds without requiring external infrastructure.

## Tests are execution, but they are not model builds

After model work is satisfied, relevant tests execute against the resulting data.

A test can fail because:

- its query errors;
- its assertion query returns violating rows.

That fails the overall run, but it does not retroactively claim the model SQL itself failed.

This distinction keeps state honest:

```text
model execution: passed
quality assertion: failed
run: failed
```

Promotion can then refuse the run for quality reasons without corrupting model execution history.

## Failures have structure

A string such as:

```text
query failed
```

is not enough for retries or automation.

Phlo Transform records structured failure information including categories such as:

```text
adapter
sql
test
timeout
cancelled
dependency
state
internal
```

Adapter failures also carry adapter-specific error codes and a `retryable` flag.

Every attempt is recorded with timing, query id and failure detail.

A run therefore preserves not only the terminal result but how it got there.

## Retrying everything would be dangerous

Some failures may disappear if tried again:

- temporary external-service failure;
- insufficient warehouse resources;
- transient Nessie communication failure.

Others will not:

- SQL syntax error;
- unknown column;
- type mismatch;
- failed data test.

Phlo Transform retries only failures classified as retryable by the adapter policy.

```bash
phlo-transform run --retries 2
```

means extra attempts for eligible transient failures, with bounded backoff.

It does not mean “run broken SQL three times”.

## Why Trino/Nessie transient classification matters

Real scale testing exposed a useful example.

During large Iceberg materialisation, Trino's connector can surface temporary Nessie REST failures through error classes such as external/resource errors, and in one case inside a generic internal-error wrapper.

Those failures may genuinely succeed on retry.

v0.1 classifies the known transient shapes accordingly.

The point is not the exact error names. The design principle is:

> Retry policy should follow observed failure semantics, not a blanket rule that all failures are transient or all failures are permanent.

## Physical identity reads get their own resilience

Planning and cache adoption depend on reading Iceberg snapshot identities.

Originally, any failure reading a snapshot looked like:

```text
no identity available
```

That was safe in one sense—it forced a rebuild—but misleading in another. A temporary metadata failure should not masquerade as proof that the table has no identity.

The Trino adapter now retries non-missing-relation snapshot-read failures briefly before returning an unverified result.

If verification still cannot be established, Phlo Transform still fails closed to `BUILD`.

Resilience may reduce unnecessary work; it never weakens the proof required for reuse.

## Timeouts and cancellation are not ordinary failures

A model can be bounded with a per-attempt timeout.

When an attempt times out, Phlo Transform records a timeout failure and, where the adapter can identify in-flight queries, asks the warehouse to cancel them.

The Trino adapter tracks query ids and can cancel active statements through Trino's query API.

External cancellation and Ctrl-C use the same general cooperative path:

```text
stop scheduling new work
cancel/abort in-flight work where possible
mark remaining work honestly
persist run state
```

The engine does not report a query as cancelled merely because the local future was dropped if the adapter tells it the query actually completed.

## What does fail-fast mean?

Without fail-fast, one failed branch does not necessarily stop unrelated work.

```text
A ──► B(fails)

C ──► D
```

C and D can still finish.

With:

```bash
phlo-transform run --fail-fast
```

the first unrecoverable failure stops new scheduling and cancels unrelated in-flight/not-started work as appropriate.

Dependents of the failed model are still `blocked`; unrelated abandoned work is `cancelled`.

Again, the states explain *why* something did not complete.

## Interrupted and failed runs are different

A killed process may leave a run that never reached a terminal state.

That is different from a run that completed and was recorded as failed.

Phlo Transform provides separate operations for those cases.

### `--resume`

Resume continues an **interrupted** run under the same run id.

```bash
phlo-transform run --resume <run-id>
```

The engine reloads the persisted plan and run state, recompiles current workspace truth, and reuses only work that is still genuinely valid.

A previously passed model can be carried forward only when:

- its desired version still matches;
- its target still exists.

Stored `skip` or `cached` decisions are not blindly trusted if the world changed afterwards.

A finished failed run is not resumable because its history is already complete.

### `--retry-failed`

Retry-failed creates a **new** run linked to the previous run:

```bash
phlo-transform run --retry-failed <run-id>
```

It selects the failed/blocked/cancelled portion plus whatever upstream work is required now.

If a previously failed model has become safely cache-adoptable in the meantime, `CACHED` counts as executable retry work. The adoption runs and relevant tests remain in scope.

That behaviour matters because cache reuse is now a real execution action, not merely a planner annotation.

## Run state is written incrementally

The state store is updated as execution progresses.

Phlo Transform does not wait until the end and write one optimistic “run result”.

That means a killed process can leave an honest trace:

```text
run = running
model A = passed
model B = running
model C = pending
```

That trace is what makes meaningful resume possible.

It also provides an audit history for later inspection.

## Human and machine output share the same result

The runner produces a structured `RunResult` containing:

- run and plan ids;
- environment;
- continuation link;
- status counts;
- model/seed/test outcomes;
- attempts;
- failures;
- query ids;
- timings;
- warnings.

Human CLI output is derived from that value.

`--json` exposes the structured representation directly.

This pattern—one semantic result, multiple renderings—is important for the next post. Phlo Transform is not only a CLI. The same compiler and engine are exposed through a versioned local daemon API for automation and agents.

*Next: [A transformation engine for humans and machines](09-the-machine-interface.md).*