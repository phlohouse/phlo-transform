# 9. A transformation engine for humans and machines

A command-line interface is useful because humans can inspect it, script it and run it anywhere.

But a modern transformation system is also consumed by things that are not people typing commands:

- editors;
- CI systems;
- dashboards;
- automation services;
- agents.

If every integration shells out to the CLI and scrapes terminal text, the product ends up with two accidental interfaces:

```text
whatever the CLI happens to print today
whatever exit codes callers guessed were meaningful
```

Phlo therefore exposes a machine-facing daemon built on the same compiler and engine libraries as the CLI.

## The first principle: one truth, multiple interfaces

The CLI and daemon should not have independent business logic.

They should be two ways to access the same semantic model.

```text
               compiler + engine
                 /         \
                /           \
             CLI          daemon API
            human          machine
```

That means a plan produced over HTTP should mean the same thing as `phlo-transform plan --json`.

Promotion gates should be identical whether invoked by a person or an automated service.

Environment resolution should not diverge between surfaces.

This sounds obvious. In practice, interface-specific implementations are a common source of drift.

## Running the daemon

A local service can be started with:

```bash
phlo-transform daemon --root . --port 7070
```

It can also be launched with the same kinds of capabilities used by the CLI:

```text
adapter
state store
Nessie client
catalog URI
default environment
```

The daemon reports what it actually has through `/v1/status`.

That response includes:

- Phlo Transform version;
- compiler semantics version;
- workspace counts;
- configured capabilities;
- update information.

A caller can therefore distinguish:

> this service understands the workspace

from:

> this service is also capable of planning against a warehouse or mutating a Nessie candidate.

## Reads and mutations are different API shapes

A read should return an answer.

A mutation may run for minutes, be cancelled, retried or inspected later.

So Phlo separates them.

### Read endpoints

Read endpoints expose semantic/workspace state directly, for example:

```text
GET /v1/check
GET /v1/models
GET /v1/models/{id}
GET /v1/lineage
GET /v1/lineage/{target}
GET /v1/impact/{target}
GET /v1/graph
GET /v1/plan
GET /v1/state/runs
GET /v1/state/promotions
```

These are machine equivalents of existing compiler/state queries.

They do not mutate the workspace or warehouse merely because somebody asked a question.

That property matters for editors and agents. Inspection must be safe to perform repeatedly.

## Planning over HTTP still uses real environment semantics

Consider:

```text
GET /v1/plan?environment=ci/pr-42
```

A tempting implementation would simply attach the string `ci/pr-42` to the response while planning against whatever default targets the daemon already compiled.

That would be wrong.

The daemon resolves the environment through the same engine path as the CLI:

```text
environment/reference
      │
      ▼
recorded/explicit/generated catalog binding
      │
      ▼
retargeted compilation
      │
      ▼
plan
```

Read-only resolution does not create the branch or catalog, but it computes the physical targets a real run would use and fails if the configuration could never support them.

That gives plan/run parity instead of an API-specific approximation.

For a never-provisioned candidate, the preview is intentionally conservative: the target can look empty because the read does not create infrastructure. The CLI surfaces the same limitation and explains that a real `run --ref` may provision and adopt inherited outputs.

## Long-running work is an operation

Mutations are submitted through:

```text
POST /v1/operations
```

with a body such as:

```json
{
  "kind": "run",
  "idempotency_key": "ci-1234",
  "params": {
    "selectors": ["tag:marts"],
    "environment": "ci/pr-42",
    "retries": 2
  }
}
```

The daemon returns an operation handle rather than keeping the HTTP request open until the warehouse finishes.

The operation has a lifecycle:

```text
queued
  │
  ▼
running
  │
  ├──► succeeded
  ├──► failed
  └──► cancelled
```

Callers can poll the handle and, for runs, inspect live progress derived from the state store.

## Why idempotency matters

Imagine a CI system submits a run and loses the HTTP response because the network drops.

It does not know whether the request reached the daemon.

If it simply retries, it may launch the same mutating operation twice.

An **idempotency key** solves that coordination problem.

```text
request 1: key=ci-1234
network response lost
request 2: key=ci-1234
```

The second submission returns the existing operation instead of executing another copy.

The key is bound to a fingerprint of the operation kind and parameters. Reusing the same key for a different request returns a conflict rather than quietly replaying unrelated work.

This is particularly important for agents, which may retry actions automatically.

## Idempotency must survive a restart

In-memory idempotency is only useful until the daemon restarts.

Phlo journals operation transitions to:

```text
.phlo/transform/operations.jsonl
```

The initial queued/idempotency reservation is written before the operation is acknowledged.

If that journal write fails, the submission fails and nothing executes.

Why be that strict?

Because otherwise this sequence is possible:

```text
accept request
start mutation
crash before recording idempotency key
restart
same key arrives again
execute mutation twice
```

Durability is part of the idempotency guarantee.

Later operation-state journaling is more tolerant: losing a progress transition can degrade history, but it must not duplicate execution.

An operation that was in flight across a daemon restart is surfaced as interrupted rather than assumed successful.

## Only one warehouse-mutating operation runs at a time

The daemon serialises major mutating operations such as:

```text
run
resume
retry_failed
promote
```

A second conflicting mutation receives a conflict response.

This is deliberately conservative for v0.1.

The engine itself supports model-level concurrency inside a run. The daemon avoids coordinating multiple independent top-level mutations that might target overlapping state at the same time.

Read-only work does not need the same gate.

## Cancellation maps to the engine's cancellation model

An operation can be cancelled with:

```text
POST /v1/operations/{id}/cancel
```

The daemon does not invent a separate cancellation mechanism.

It passes the request into the engine's `CancelHandle`, which stops scheduling and cancels tracked warehouse work where the adapter supports it.

The operation then ends as `cancelled`, not as a generic internal error.

Again, machine semantics and CLI semantics stay aligned.

## Resume and retry preserve the original environment

A run recorded against `ci/pr-42` must not be resumed into `ci/pr-99` just because a client supplied different parameters later.

The stored run's environment is authoritative.

The daemon resolves/provisions the same environment before continuation and refuses incompatible requests.

This keeps the meaning of run history stable:

```text
run X was work against environment Y
```

not:

```text
run X means whatever environment the latest caller supplied
```

## Promotion over the API uses the same evidence model

The daemon's promote operation calls the same promotion evaluation used by the CLI.

That means it must see:

- a run bound to the candidate's current Nessie head;
- applicable branch-diff evidence;
- contract/schema gate results;
- lineage evidence where required;
- base/candidate hashes that have not moved;
- the exact evidence IDs consulted.

A machine caller cannot bypass the audit model by choosing a different interface.

This is one reason shared Postgres evidence matters: CI stage A can build/audit a candidate, while an approval service or operator on another machine promotes it through the daemon using the same persisted evidence.

## Authentication is intentionally small

For local use, the daemon binds to loopback by default.

It can also require a bearer token:

```bash
phlo-transform daemon --token <secret>
```

Protected requests then require:

```text
Authorization: Bearer <secret>
```

Liveness/status remains available so a supervisor can tell whether the process is up.

The v0.1 daemon is not trying to be an internet-facing identity platform. It provides a narrow local/service auth boundary and leaves larger deployment identity concerns outside the transform engine.

## Stable errors matter more to machines than prose

The API returns structured errors with stable codes such as:

```json
{
  "error": {
    "code": "API007",
    "message": "..."
  }
}
```

A human can read the message.

A machine can reason about the code without string matching.

The same philosophy appears throughout Phlo: human-readable explanations and machine-readable structure should come from the same underlying result.

## The daemon serves immutable compiled snapshots

Editors and agents may issue several reads while files are changing on disk.

They should not observe a compiler halfway through replacing its internal graph.

The daemon publishes an immutable compiled snapshot behind a read/write boundary. Reload builds a new snapshot and then swaps it in.

A file watcher can trigger conservative full recompilation when relevant SQL/config files change.

v0.1 deliberately prefers coherent full snapshots over clever partial invalidation. Targeted daemon recompilation is a later optimisation.

## What does an agent gain from this?

An agent working on a transformation repository can ask structured questions before editing:

```text
What models exist?
What does this model depend on?
What columns does it produce?
What is downstream of this column?
Would this change rebuild anything?
What is the current candidate plan?
Which run failed and why?
Which evidence authorised the last promotion?
```

It can then submit bounded actions using explicit operation schemas rather than constructing shell commands from prose.

That does not make the agent the source of truth.

The compiler, planner, state store and promotion gates remain the source of truth. The agent is simply another client.

That is the important inversion:

> Phlo is not made “agentic” by letting an LLM improvise transformation semantics. It becomes agent-friendly by exposing deterministic semantics through a structured interface.

## Where v0.1 deliberately stops

The daemon is useful but intentionally modest:

- file changes trigger full recompilation rather than targeted invalidation;
- Postgres state currently has no TLS in the client;
- daemon human ergonomics are less rich than the CLI because JSON is the primary surface;
- it is not a general workflow orchestrator for the entire lakehouse.

Those are boundaries, not hidden omissions.

The final post puts the whole system together from a blank directory to an audited Nessie promotion, and shows how the compiler, state, runner, evidence store and machine interface fit into one lifecycle.

*Next: [Putting Phlo Transform together](10-putting-it-all-together.md).*