# Phase 8 — Compiler daemon and agent APIs

## Objective

Turn the compiler from a command-invoked process into a reusable long-running semantic service for CLI, editors, UI, CI and AI agents.

At the end of this phase, Phlo Transform should maintain an incrementally updated in-memory workspace model and expose fast, structured queries such as model inspection, lineage, impact, plan explanation and change analysis without reparsing the entire repository for every request.

## Design principles

1. The daemon wraps the same compiler libraries used by the CLI; it is not a second implementation.
2. Structured semantic APIs are the product; terminal text is a rendering.
3. Agents should query compiler truth, not grep SQL/config files when the compiler can answer directly.
4. Incremental invalidation should be dependency-aware and conservative.
5. Local development should work without a remote service.
6. Authentication/network complexity should not be introduced until the daemon needs to leave the local machine/process boundary.

## Daemon command

Initial direction:

```bash
phlo transform daemon
```

The daemon should load a workspace and maintain:

```text
file index
transform roots
model registry
parsed ASTs
resolved ASTs
typed ASTs
model DAG
column lineage
catalog schemas
model/version hashes
current plan inputs
diagnostics
```

## Incremental compilation

Watch relevant workspace files.

Example:

```text
workflows/assay/transforms/results.sql changed
        ↓
re-read file
        ↓
reparse model
        ↓
re-resolve relations
        ↓
re-type affected model
        ↓
invalidate affected downstream semantic state
        ↓
update graph/diagnostics
```

Do not invalidate unrelated models unnecessarily.

## Invalidation graph

Track dependency categories separately where useful:

```text
file -> model parse
model relation -> model DAG
source schema -> type resolution
column -> column lineage
model version -> downstream state
config scope -> affected model set
```

Changing a folder-level config should invalidate models in that scope without requiring a blind full-workspace rebuild if the affected set can be determined reliably.

## Performance goals

Design toward, not necessarily guarantee initially:

```text
~1,000 model cold load: <5 s
common single-file incremental update: <500 ms
simple inspect/lineage query after load: effectively interactive
```

Measure real fixtures before optimizing speculative bottlenecks.

## Local protocol

Start with the simplest stable local protocol.

Candidates:

```text
JSON-RPC over Unix socket
local HTTP + JSON
```

Avoid gRPC unless typed cross-language needs justify it.

The public protocol must be versioned.

## API surface

### Workspace status

```text
GET/status
```

Equivalent structured result should expose:

```text
workspace root
compiler version
semantic version
graph/model counts
diagnostic counts
last update time
current target/ref context
```

### List models

Equivalent of:

```bash
phlo transform list --json
```

Filters should mirror the small CLI selector model.

### Inspect model

Equivalent of:

```bash
phlo transform inspect assay.results --json
```

Return:

```text
logical ID
physical path
workflow/root ownership
effective config
materialisation
schema
dependencies
consumers
contracts/tests
current/desired state
change reasons
diagnostics
```

### Lineage

Equivalent of:

```bash
phlo transform lineage assay.results.result --json
```

Return typed nodes/edges and direction/depth controls.

### Impact

Equivalent of:

```bash
phlo transform impact assay.results.result --json
```

Return downstream columns/models/workflows/registered outputs.

### Check/diagnostics

Return current compiler diagnostics without requiring a full CLI invocation.

### Plan

Generate or query plan information using current compiler state.

Plan generation that requires warehouse/catalog/source refresh may remain asynchronous within the request, but the daemon must return a completed response rather than imply background completion later.

## Agent-oriented commands

The CLI remains useful for agents that operate through shell tools:

```bash
phlo transform inspect <model> --json
phlo transform lineage <model-or-column> --json
phlo transform impact <model-or-column> --json
phlo transform plan --json
phlo transform check --json
```

Keep these stable even after the daemon exists.

## Agent use cases

The semantic API should answer directly:

### "What does this model depend on?"

Return resolved upstream models/sources.

### "Where does this column come from?"

Return transitive column lineage.

### "What breaks if I remove this column?"

Return downstream impacted columns/models/workflows/tests/outputs.

### "Why will this model rebuild?"

Return state-hash component differences and planner reasons.

### "What will this PR change?"

Return changed model versions, schema impact, planned execution and, when available, data-diff summaries.

### "Can I rename this model?"

Return all known references/consumers and whether stable explicit ID preserves historical identity.

## Stable schemas

Do not expose internal Rust structs directly as API contracts.

Define explicit versioned DTOs for:

```text
ModelSummary
ModelDetail
GraphNode
GraphEdge
ColumnLineage
Diagnostic
PlanSummary
ChangeReason
StateSummary
```

Use additive evolution where practical.

## Diagnostics push

For editor/UI consumers, support subscription or watch semantics later so clients can receive updated diagnostics after file changes.

An initial polling endpoint is acceptable if it keeps implementation simpler.

## Editor integration

The daemon should make an editor extension/LSP feasible.

Useful capabilities:

- diagnostics on save/type;
- go to upstream model;
- find downstream references;
- hover model/column schema;
- show inferred type;
- show lineage;
- flag ambiguous/unknown relation names.

A complete LSP implementation can be a subphase after the daemon/API is stable.

## SQL language-server boundary

Do not build a general SQL IDE from scratch if existing LSP components can be reused. Phlo-specific value is workspace semantic resolution, lineage and state.

Expose these through a thin LSP bridge if appropriate.

## UI integration

Phlo UI may query the daemon/service for:

- workspace graph;
- model details;
- plan review;
- schema changes;
- lineage;
- current diagnostics;
- run/state history.

The daemon returns semantic data only; UI layout/rendering belongs elsewhere.

## Catalog refresh

Catalog metadata can become stale independently of files.

Support explicit/incremental refresh:

```text
refresh relation on demand
TTL refresh for external schemas
refresh on plan when correctness requires it
```

Do not constantly hammer Trino/catalog services while idle.

## Offline behaviour

The daemon should still provide file-based workspace parsing, DAG and cached semantic information when the warehouse is unavailable.

Mark catalogue-dependent information stale/unknown explicitly.

## Concurrency

Multiple read queries should not block each other unnecessarily.

Compiler updates should publish coherent workspace snapshots so clients never observe half-applied graph changes.

Potential model:

```text
mutable compiler worker
      ↓
immutable semantic snapshot
      ↓
many concurrent readers
```

## Security

For a local socket/service:

- bind locally by default;
- do not expose credentials in API responses;
- do not expose compiled secrets/connection tokens;
- redact sensitive config fields.

Remote/multi-user deployment security is a separate design problem and should not be prematurely introduced.

## Agent mutation boundary

The daemon/API should primarily provide semantic reads and planning.

Agents that edit SQL should still make normal filesystem/Git changes, allowing the daemon to observe and recompile them.

Do not add an opaque "agent edits model" mutation API unless a concrete use case requires it.

## Machine-readable errors

Every API error should include:

```text
stable code
category
message
source span where applicable
related model/source IDs
suggested remediation where deterministic
```

## Testing

### Incremental compiler tests

Verify changes to:

- one SQL file;
- upstream model;
- source schema;
- root config;
- folder config;
- model ID;

invalidate exactly the required semantic state.

### Snapshot consistency

Issue concurrent reads during updates and verify no client receives internally inconsistent graph/schema state.

### Protocol tests

Version and contract-test all public DTOs/endpoints.

### Performance fixture

Generate a representative ~1,000 model workspace and benchmark:

- cold load;
- single-leaf edit;
- upstream edit with many downstreams;
- inspect query;
- column lineage query.

Track regressions in CI where practical.

### Agent workflow tests

Prove a client can:

1. inspect a model;
2. determine impact of a column change;
3. edit the file externally;
4. observe updated diagnostics/impact;
5. request a new plan;

without restarting the daemon.

## Acceptance criteria

Phase 8 is complete when:

1. `phlo transform daemon` maintains a live semantic workspace;
2. common single-file edits trigger targeted rather than full recompilation where safe;
3. model inspection, lineage, impact, diagnostics and plan information are exposed through a versioned structured API;
4. CLI JSON output remains compatible with the same semantic model;
5. clients never observe partially updated workspace state;
6. offline/cached operation clearly marks warehouse-dependent data stale rather than failing all semantic queries;
7. the API exposes no credentials/secrets;
8. a realistic large-workspace benchmark is tracked;
9. an agent/editor-style integration test can edit files and receive updated semantic results without daemon restart.

## Possible follow-on work

- LSP implementation;
- remote shared compiler service;
- browser/UI graph exploration;
- semantic rename/refactor operations;
- richer agent tools for safe automated migration;
- proactive PR impact summaries;
- compiler-backed autocomplete.

## Implementation notes

Phase 8 is **partially implemented** (audited against code and tests). See
[`docs/daemon.md`](../daemon.md).

Implemented:

- `phlo-transform-daemon`: `WorkspaceService` holding an immutable compiled
  snapshot, versioned `/v1` routes for status/check/models/inspect/lineage/
  impact/graph/plan plus diff and state reads, tracked `POST /v1/operations`
  mutations (`run`/`test`/`promote`/`resume`/`retry_failed`/`reload`) with
  idempotency keys and cooperative cancellation, and a polling file watcher
  that reloads on change.
- CLI `daemon` command binds `127.0.0.1`.
- API and CLI share the same report DTOs; errors carry stable codes.
- Tests cover the HTTP surface, explicit reload and watcher-driven reload
  without restart.

Missing / deviated:

- Reload is a **coherent full recompile**, not dependency-aware targeted
  invalidation (acceptance criterion 2 is deviated).
- No concurrency/snapshot-consistency test under simultaneous reads and writes.
- No 1,000-model benchmark (acceptance criterion 8 unmet).
- No push/subscription diagnostics (polling reload only), no LSP bridge and no
  remote security model.

