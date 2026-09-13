# Machine-facing API (`phlo-transform daemon`)

`phlo-transform-daemon` wraps the same compiler/engine libraries as the CLI and
exposes a versioned local JSON API for editors, UI, CI and agents. It is the
stable machine surface of the project: reads never mutate anything, mutations
run as tracked operations, and every response reuses the same serialisable
report DTOs as the CLI's `--json` output.

## Running

```bash
phlo-transform daemon --root <workspace> --port 7070 \
    [--adapter duckdb|trino ...] [--state <path-or-url>] \
    [--nessie-endpoint <uri>] [--environment <label>]
```

The daemon binds `127.0.0.1` by default and needs no authentication for local
use. It loads an immutable compiled snapshot behind an `RwLock`, so readers
never observe partially applied graph/schema changes.

Whichever engine handles the CLI flags yield are served; a daemon with no
adapter still answers every offline read and returns `API007` on endpoints
that need the missing piece. `/v1/status` reports the `capabilities` it was
launched with (`adapter`, `state`, `nessie`).

## Read endpoints (never mutate)

| Endpoint | CLI equivalent | Notes |
|---|---|---|
| `GET /status`, `GET /v1/status` | — | workspace root, semantics version, counts, capabilities, last update |
| `GET /v1/check` | `check --json` | diagnostics / check report |
| `GET /v1/models` | `list --json` | models, sources and tests |
| `GET /v1/models/{id}` | `inspect <id> --json` | model detail |
| `GET /v1/lineage` | `lineage --format graph` | full lineage document |
| `GET /v1/lineage/{model-or-column}` | `lineage <target> --json` | model or column lineage |
| `GET /v1/impact/{model}.{column}` | `impact <col> --json` | downstream columns/models/tests/consumers |
| `GET /v1/graph` | `graph` artifact | typed graph artifact |
| `GET /v1/plan?select=&environment=&force=` | `plan --json` | needs adapter; `select` repeats or singles |
| `GET /v1/diff/lineage?base=<git-ref>` | `lineage --diff <ref>` | offline; extracts the base tree read-only |
| `GET /v1/diff/branch?from=&to=&full=` | `diff --from --to [--full]` | needs adapter; writes `branch_diff.json` like the CLI |
| `GET /v1/state/runs?environment=` | `state runs` | newest first |
| `GET /v1/state/runs/{id-or-prefix}` | `state show <run>` | run + stored plan + per-item records |
| `GET /v1/state/models/{id}?environment=` | `state model <id>` | recorded materialisation |
| `GET /v1/state/promotions` | `state promotions` | promotion audit log |

## Operations (mutations and long-running work)

`POST /v1/operations` submits work and returns a handle — it never blocks on
the work itself:

```json
{"kind": "run", "idempotency_key": "ci-1234", "params": {"selectors": ["tag:marts"]}}
```

Response: `{"operation": {...}, "replayed": false}`.

| Kind | CLI equivalent | Params | Needs |
|---|---|---|---|
| `run` | `run [selectors] --environment --force` | `selectors`, `environment`, `force`, `run_tests` | adapter |
| `test` | `test [selectors]` | `selectors` | adapter |
| `promote` | `promote <ref> --to <ref> [--check] [--require-diff] [--allow-breaking-schema] [--cleanup]` | `candidate`, `to`, `check`, `require_diff`, `allow_breaking_schema`, `cleanup`, `actor` | nessie |
| `reload` | — | — | — |

The submission body may also spread params at the top level
(`{"kind": "run", "selectors": [...]}`). Param schemas are strict —
`deny_unknown_fields` — so a misspelled key is `API012`, never ignored.

`params.environment` is the state/environment label (the CLI's
`--environment`); Nessie environment provisioning (`--ref`) stays a CLI step
because it is an interactive setup action, not a per-run parameter.

### Lifecycle

`queued → running → succeeded | failed | cancelled`

- `GET /v1/operations` — every known operation.
- `GET /v1/operations/{id}` — the record (`params`, `result`, `error`,
  timestamps). While a `run` op is running it also carries `progress` —
  the live per-model execution state read back from the state store
  (`run_id`, `planned`, `finished`, per-model records).
- `POST /v1/operations/{id}/cancel` — cooperative cancellation through the
  engine's `CancelHandle`; observed between model builds and test
  executions. Cancelled ops end `cancelled`, not `failed`.

`result` on success is the same DTO the CLI's `--json` prints: `run.json`
(RunResult) for `run`, `{tests: [...], failed}` for `test`, the gate report +
promotion record for `promote`. A gate rejection is `succeeded` with
`ok: false` — the operation delivered a verdict. Engine/infrastructure
failures end `failed` with `error.code`/`error.message`.

`promote` applies the same evidence rules as the CLI (see `docs/wap.md`):
the recorded run must be bound to the candidate's current Nessie head, the
branch-diff artifact must name both current heads, the `base` gate needs
immutable branch-cut provenance or a hash-bound audit, and the merge request
pins both refs so a concurrent advance is rejected. The `run` op binds the
run to the environment's current Nessie head when the environment names a
reference; an unbound run cannot later promote.

### Idempotency

Resubmitting a body with the same `idempotency_key` (or `Idempotency-Key`
header; the header wins) returns the existing handle with `replayed: true`
instead of executing twice. Keys are scoped per kind and live for the daemon
process — they deduplicate retries, they are not durable records.

### Serialization

At most one warehouse-mutating operation (`run`, `promote`) executes at a
time; a second submission gets `409 API008`. `test`/`reload` are not gated
(read-only queries / in-memory snapshot swap). `POST /v1/reload` is a
synchronous convenience for `{kind: "reload"}`.

## Errors

Every failure is `{"error": {"code": "APIxxx", "message": "..."}}` with a
stable code:

| Code | HTTP | Meaning |
|---|---|---|
| `API001` | 400 | invalid model id |
| `API002` | 404 | no such model |
| `API003` | 400 | invalid lineage target |
| `API004` | 404 | no column lineage |
| `API005` | 400 | invalid column reference |
| `API006` | 400 | invalid selector expression |
| `API007` | 503 | capability not configured (adapter/state/nessie) |
| `API008` | 409 | another mutating operation is running |
| `API009` | 404 | no such operation |
| `API010` | 400 | git-side failure (unknown ref, extraction error) |
| `API011` | 500 | engine/state failure |
| `API012` | 400 | malformed request body, params, or query |
| `API013` | 404 | missing record (ref, run, materialisation) |
| `API014` | 400 | ambiguous run-id prefix |

Inside an operation record, `error.code` reuses these codes; engine failures
surface as `API011`, selector errors as `API006`, and cancellation as
`cancelled`.

## Incremental updates

`spawn_watcher` polls relevant `.sql`/`.toml` files (excluding `.git`,
`target`, `.phlo`, `node_modules`) and reloads the snapshot when any
modification time changes. Reload is conservative (a full recompile), which
keeps published snapshots coherent. `POST /v1/reload` and the `reload`
operation do the same on demand, so agents edit files on disk and see
updated semantics without restarting the daemon.

## Offline behaviour

The daemon compiles offline (no warehouse required). Catalogue-dependent
information is reported as unknown via model limitations rather than failing
queries.

## No secrets

The API exposes semantic data only; credentials and connection configuration
are never part of responses or error messages. State URLs are redacted by
the CLI before they reach the daemon.

## Tests

- Existing API tests: status, models, inspect, lineage, impact, graph,
  check, reload, watcher.
- `run` operation lifecycle on real DuckDB + sqlite state: submit → poll →
  `succeeded`, `result.status == passed`, state endpoints serve the recorded
  run/model, idempotent resubmission replays the handle.
- `test` op, `GET /v1/plan` (selector narrowing + `API006`), `GET /v1/lineage`
  document, `GET /v1/diff/lineage` self-diff and `API010` on a bogus ref.
- Rejection matrix: `API007`/`API008`/`API009`/`API012`, the mutating-op
  conflict gate, cancel on a queued op, `POST /v1/reload`.

## Deferred

- Nessie environment provisioning (`--ref` semantics) and `ref`/`diff`
  model-level mutation endpoints — the API currently mirrors the
  non-interactive command surface;
- durable operation history (ops are in-memory; restart clears them);
- push/subscription diagnostics and live event streaming (progress is
  currently polled from the state store);
- dependency-aware (targeted) invalidation and performance benchmarks;
- LSP bridge;
- remote/multi-user security.
