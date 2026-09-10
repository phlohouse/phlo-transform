# Phase 8 compiler daemon and agent APIs

`phlo-transform-daemon` wraps the same compiler/engine libraries as the CLI and
exposes a versioned local JSON API for editors, UI, CI and agents.

## Running

```bash
phlo-transform daemon --root <workspace> --port 7070
```

The daemon binds `127.0.0.1` by default and needs no authentication for local
use. It loads an immutable compiled snapshot behind an `RwLock`, so readers
never observe partially applied graph/schema changes.

## API (v1)

| Endpoint | Description |
|---|---|
| `GET /status` | workspace root, compiler semantics version, counts, last update |
| `GET /v1/check` | diagnostics / check report |
| `GET /v1/models` | models, sources and tests |
| `GET /v1/models/{id}` | model detail (config, schema, deps, workflow, state) |
| `GET /v1/lineage/{model-or-column}` | model or column lineage |
| `GET /v1/impact/{model}.{column}` | downstream columns/models/tests/consumers |
| `GET /v1/graph` | typed graph artifact |

Responses reuse the same serialisable report DTOs as the CLI `--json` output,
so CLI and API stay consistent. Errors carry a stable `error.code` and message.

## Incremental updates

`spawn_watcher` polls relevant `.sql`/`.toml` files (excluding `.git`,
`target`, `.phlo`, `node_modules`) and reloads the snapshot when any
modification time changes. Reload is currently conservative (a full
recompile of the workspace), which keeps published snapshots coherent; the
the design leaves room for dependency-aware invalidation. `reload()` is also
available directly and via a new file edit observed by the watcher, so agents
edit files on disk and see updated semantics without restarting the daemon.

## Offline behaviour

The daemon compiles offline (no warehouse required). Catalogue-dependent
information is reported as unknown via model limitations rather than failing
queries.

## No secrets

The API exposes semantic data only; credentials and connection configuration
are never part of responses.

## Tests

- HTTP API test covering status, models, inspect, lineage, impact, graph and
  check;
- reload test (edit a file, reload, see the new model);
- watcher test (edit a file, observe updated state without restart).

## Deferred

- dependency-aware (targeted) invalidation and performance benchmarks;
- push/subscription diagnostics;
- LSP bridge;
- remote/multi-user security.
