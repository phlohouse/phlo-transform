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
    [--nessie-endpoint <uri>] [--nessie-catalog-uri <uri>] \
    [--environment <label>]
```

The daemon binds `127.0.0.1` by default and needs no authentication for local
use. `phlo-transform daemon --token <secret>` requires `Authorization:
Bearer <secret>` on every endpoint except `/status` (liveness stays open);
missing or wrong credentials answer `401 API015`.

It loads an immutable compiled snapshot behind an `RwLock`, so readers
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
| `GET /v1/impact/{target}` | `impact <target> --json` | model target: downstream models + their tests; `model.column`/`dataset.column`: column-level impact |
| `GET /v1/impact?select=` | `impact --select <terms>` | selection blast radius: dependents outside the set + covering tests |
| `GET /v1/graph` | `graph` artifact | typed graph artifact |
| `GET /v1/plan?select=&environment=&base=&force=` | `plan --json` | needs adapter; `select` repeats or singles; an `environment` resolves through the same physical-target resolution a `run` uses (read-only — it provisions nothing) |
| `GET /v1/diff/lineage?base=<git-ref>[&candidate=<git-ref>]` | `lineage --diff <base> [candidate]` | the shared engine diff: one ref is merge-base→worktree, two refs is exact→exact; writes `lineage_diff.json` like the CLI so a later `promote` can audit it |
| `GET /v1/diff/branch?from=&to=&full=` | `diff --from --to [--full]` | needs adapter; writes `branch_diff.json` like the CLI |
| `GET /v1/state/runs?environment=` | `state runs` | newest first |
| `GET /v1/state/runs/{id-or-prefix}` | `state show <run>` | run + stored plan + per-item records |
| `GET /v1/state/runs/{id-or-prefix}/failed` | `state show <run> --failed` | failed/blocked/cancelled items — what `resume`/`retry_failed` would pick up |
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
| `run` | `run [selectors] --ref --from --force` | `selectors`, `environment`, `base`, `force`, `run_tests` | adapter |
| `resume` | `run --resume <run>` | `run` | adapter + state |
| `retry_failed` | `run --retry-failed <run>` | `run` | adapter + state |
| `test` | `test [selectors] --ref --from` | `selectors`, `environment`, `base` | adapter |
| `promote` | `promote <ref> --to <ref> [--check] [--require-diff] [--allow-breaking-schema] [--cleanup]` | `candidate`, `to`, `check`, `require_diff`, `allow_breaking_schema`, `cleanup`, `actor` | nessie |
| `reload` | — | — | — |

The submission body may also spread params at the top level
(`{"kind": "run", "selectors": [...]}`). Param schemas are strict —
`deny_unknown_fields` — so a misspelled key is `API012`, never ignored.

### Environments are real branches

`params.environment` is the candidate Nessie reference (the CLI's `--ref`),
and `params.base` the ref a new candidate is cut from (the CLI's `--from`,
default `main`). A `run` or `test` op without `environment` — and the
`plan`/state reads without `environment=` — inherits the daemon's
launch-time `--environment` label, so a daemon scoped to a candidate
environment runs and reads inside it consistently. When the daemon was
launched with `--nessie-endpoint`, a
`run`/`resume`/`retry_failed` against an environment other than the base
ref runs the full provisioning step the CLI runs: the Nessie branch is
created (or reused), a branch-scoped Iceberg catalog is provisioned, and
the workspace is recompiled retargeted at that catalog before planning —
the run physically writes to the environment it claims, and a passed run
binds to the environment's **post-run** Nessie head. The provisioning
evidence is persisted (`environment_<ref>.json`) so `promote` can audit
cut-from provenance.

The provisioned catalog is named `phlo_<sanitised-ref>_<hash>` — the
readable ref plus 8 hex of the ref's SHA-256 — so refs that fold to the
same readable name (`ci/pr-1`, `ci_pr_1`, `ci-pr-1`) can never share one
physical catalog. An existing catalog is never adopted on name alone:
Trino cannot read a catalog's configured Nessie ref back over SQL, so the
engine records how each catalog was established (`created` /
`unverified` / `unmanaged`) and accepts a pre-existing catalog only when
the binding is provable — the generated self-named convention, or a
recorded `environment_<ref>.json` binding. An unverifiable foreign
catalog, or one another candidate already claims, fails the operation
rather than write into it.

When the daemon has no Nessie client at all the environment can only ever
be a state-record label — local mode is truthful and the run proceeds.
But once Nessie is configured a named environment claims branch semantics:
a non-base environment it cannot provision (no adapter, or no
catalog-facing `nessie_uri`) fails with `API007` rather than execute on
the default target and bind its evidence to a head it did not produce.
Running against the base ref itself (`environment == base`) is not a
candidate and uses the compiled default target.

`GET /v1/plan?environment=` resolves through the *same* environment
resolution — in read-only mode: the physical catalog is computed (override
→ recorded binding → generated name) and the workspace recompiled against
it, but no branch is created and no evidence is written. A previewed
`plan(environment=X)` therefore names exactly the targets a later
`run(environment=X)` executes — `plan.models[].target` equals
`run.models[].target` for the same environment and base (`base` defaults
to `main`).

`resume`/`retry_failed` continue the run whose id or unique prefix
`params.run` names. The stored run's environment is authoritative — the
continuation provisions and retargets whatever the original run targeted,
and refuses to execute into a different one. `resume` keeps the original
run id and only works on interrupted/cancelled runs; a finished failed run
must go through `retry_failed`, which starts a new run linked by
`continued_from` and rebuilds only the failed/blocked/cancelled or
failed-test portion. Both need the persisted plan — runs recorded before
plan persistence cannot be continued.

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
pins both refs so a concurrent advance is rejected. A passed `run`,
`resume`, or `retry_failed` op binds the run to the environment's
**post-run** Nessie head — the commit its writes produced; an unbound run
cannot later promote.

### Idempotency

Resubmitting a body with the same `idempotency_key` (or `Idempotency-Key`
header; the header wins) returns the existing handle with `replayed: true`
instead of executing twice. Keys are **global to the endpoint** — kind
lives inside the request fingerprint, not the lookup — and bound to the
request: the reservation stores a SHA-256 fingerprint of the canonical
`{kind, params}` body, so the same key resubmitted with *different* params
or a *different kind* answers `409 API016` — a key can never quietly
replay a request it did not cover. Operations recorded before request
binding (no stored fingerprint) still replay by key for backward
compatibility.

### Durability

Every operation transition is appended to
`.phlo/transform/operations.jsonl` and folded back in at startup: operation
history, results and idempotency keys survive a daemon restart, so a retried
submission still replays its original handle. The initial queued record —
the idempotency reservation — is journaled **before** the operation is
acknowledged; if that append fails the submission is rejected (`API011`)
and nothing executes, so a restart can never accept the same key twice.
Later transitions remain best-effort: a lost one degrades history (the op
restores as `interrupted`) but cannot duplicate execution. An operation
that was in flight when the daemon stopped is restored as `failed` with
`error.code: "interrupted"` — its cancel handle cannot be reconstructed and
its effects cannot be assumed; resubmit it under a new key.

### Serialization

At most one warehouse-mutating operation (`run`, `resume`, `retry_failed`,
`promote`) executes at a time; a second submission gets `409 API008`.
`test`/`reload` are not gated (read-only queries / in-memory snapshot
swap). `POST /v1/reload` is a synchronous convenience for
`{kind: "reload"}`.

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
| `API015` | 401 | missing or invalid bearer token |
| `API016` | 409 | idempotency key reused for a different request |

Inside an operation record, `error.code` reuses these codes; engine failures
surface as `API011`, selector errors as `API006`, cancellation as
`cancelled`, and a restart with the op in flight as `interrupted`.

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
- `promote` end-to-end over HTTP: real DuckDB + sqlite state + recorded
  provisioning evidence + a deep branch-diff artifact + in-memory Nessie —
  `--check` gates without merging, the real promote merges and persists the
  promotion record, an unaudited candidate fails the `run` gate with
  `ok: false`.
- Durability: ops and idempotency keys survive a restart via the journal;
  an in-flight op at shutdown restores as `interrupted`; a keyed retry
  replays its original handle; an unwritable journal rejects the
  reservation before anything executes.
- Request-bound idempotency: same key + same request replays, same key +
  different params answers `409 API016`, and the binding survives a
  restart.
- Continuation: `GET /v1/state/runs/{id}/failed` lists the failed/blocked
  portion; `resume` keeps an interrupted run's id and reuses its passed
  models; `retry_failed` starts a new run over the failed portion and links
  it with `continued_from`; both refuse runs whose stored environment they
  cannot target.
- Impact surface: `GET /v1/impact/{model}` (downstream + tests),
  `GET /v1/impact/{model.column}` (column impact), `GET /v1/impact?select=`
  (selection blast radius).
- Environment isolation e2e (ignored; CI): a `run` op against
  `environment: ci/pr-1` over real Nessie + Trino provisions the branch and
  its hash-suffixed `phlo_ci_pr_1_<hash>` catalog, writes the candidate
  value there, leaves `main` byte-identical, binds the stored run to the
  candidate's post-run head, and proves `plan(environment)` previews the
  exact target the run executes.
- Bearer auth: `401 API015` without/with a wrong token on every endpoint but
  `/status`.

## Deferred

- `ref`/`diff` model-level mutation endpoints;
- push/subscription diagnostics and live event streaming (progress is
  currently polled from the state store);
- dependency-aware (targeted) invalidation and performance benchmarks;
- LSP bridge;
- remote/multi-user security.
