# Phase 5 — Nessie-native environments and Write-Audit-Publish

## Objective

Make versioned lakehouse state a first-class execution primitive.

For the primary Phlo deployment, environments should map directly onto Nessie references rather than introducing a second, overlapping environment abstraction.

Production transformation should use Write-Audit-Publish (WAP) by default:

```text
WRITE
  ↓
AUDIT
  ↓
PUBLISH
```

At the end of this phase, a candidate transformation run can execute safely on an isolated Nessie branch, pass quality gates, and then be promoted to `main` without writing directly into canonical production state during build/test.

## Design principles

1. Nessie references are the environment primitive where Nessie is available.
2. Compilation remains independent of Nessie side effects.
3. Apply writes to the selected candidate reference.
4. Production publication is a distinct promotion operation.
5. Promotion must validate that the reviewed candidate state is still current.
6. Rollback should use versioned data state rather than bespoke reverse-SQL where possible.
7. Do not duplicate capabilities already provided cleanly by Nessie/Iceberg.

## Environment mapping

Typical references:

```text
main
feature/assay-normalisation
ci/pr-184
release/2026-09
```

CLI examples:

```bash
phlo transform --ref feature/assay-normalisation plan
phlo transform --ref feature/assay-normalisation apply
```

A later convenience layer may derive the default Nessie branch from the Git branch, but explicit `--ref` behaviour must remain deterministic and inspectable.

## Nessie client boundary

Keep Nessie operations separate from SQL/Trino execution.

Suggested interface responsibilities:

```text
get_reference
create_branch
resolve_hash
compare_references
merge
list_conflicts
delete_branch
rollback/reset where supported and safe
```

Do not put SQL execution into the Nessie abstraction.

## Candidate branch lifecycle

Typical local/CI flow:

```text
resolve base main@abc123
       ↓
create candidate branch
       ↓
plan against candidate + base
       ↓
apply transforms to candidate
       ↓
run contracts/tests
       ↓
record candidate final hash
       ↓
ready for promotion
```

Branch lifecycle metadata should be associated with the Phlo run/plan.

## WAP execution

Production-oriented run:

```text
PLAN
  ↓
WRITE candidate state
  ↓
AUDIT
  ├─ structural contracts
  ├─ runtime tests
  ├─ schema policy
  ├─ freshness policy
  └─ later: data-diff policy
  ↓
PUBLISH
```

A failed audit leaves the candidate isolated and does not advance `main`.

## `promote`

Implement:

```bash
phlo transform promote feature/new-assay --to main
```

Promotion preconditions should include:

- candidate run completed successfully;
- required tests passed;
- schema policy passed;
- candidate state still matches the audited run;
- target base has not moved incompatibly since plan/audit;
- no unresolved Nessie merge conflict;
- optional external approval policy where configured.

The command should support a dry/plan form before merge if useful:

```bash
phlo transform promote feature/new-assay --to main --check
```

## Promotion artifact

Record:

```text
promotion_id
candidate_ref
candidate_hash
target_ref
target_hash_before
target_hash_after
plan_id
run_id
quality_gate_results
conflicts/merge mode
timestamp
actor identity where available
```

## Staleness and target movement

Suppose a candidate was planned from:

```text
main@100
```

but `main` is now:

```text
main@125
```

Promotion must not blindly merge because tests were performed against the older base.

The engine should:

1. ask Nessie whether the candidate can merge cleanly;
2. determine whether relevant upstream/source state changed;
3. require re-plan/re-audit when the previous evidence is no longer valid.

Be conservative initially.

## Conflict handling

Nessie conflicts should be presented in Phlo terms where possible.

Example:

```text
error[PROMOTION_003]: candidate cannot be promoted

Conflicting content:
  iceberg.analytics.assay_results

Candidate base:
  main@abc123
Current target:
  main@def456

Rebase/re-plan the candidate before promotion.
```

Do not hide raw conflict details; include them in structured output.

## Rollback

Support two related concepts:

### Environment rollback

Move/restore a Nessie reference to a known prior published state where the operational model permits this.

### Model-level rollback

Where practical, restore an earlier known model/table state without pretending independent rollback is safe when downstream/environment consistency would be broken.

Prefer environment-level rollback as the strongest consistency primitive.

CLI direction:

```bash
phlo transform rollback --ref main --to <nessie-hash>
```

Model-scoped rollback should be added only with explicit consistency checks.

## Branch cleanup

Support cleanup of temporary CI/feature branches only when they are known Phlo-managed branches or the user explicitly targets them.

Never delete arbitrary Nessie references based on pattern matching alone.

## Schema policy as audit gate

Use Phase 2 schema classifications during WAP.

Example:

```text
AUDIT assay.results
  tests: PASS
  schema: BLOCKED

Breaking change:
  removed `legacy_result`
```

Promotion stops before publish.

## Incremental models on branches

Phase 4 incremental strategies must work against the candidate branch.

State must include:

- Nessie reference;
- resolved hash before run;
- resulting hash after write;
- Iceberg snapshot IDs where relevant.

A model materialised on one reference must not be considered current on another reference solely because logical model hashes match.

## CI workflow

Target shape:

```text
PR opened/updated
      ↓
create ci/pr-184 from main
      ↓
phlo transform plan --ref ci/pr-184 --base main
      ↓
apply
      ↓
audit
      ↓
report result to PR
      ↓
(optional later) approve/promote after merge policy
```

## Security and permissions

Separate permissions for:

- reading catalog/reference metadata;
- writing candidate branches;
- merging/promoting to protected references.

A developer credential that can write a feature branch should not necessarily be able to publish to `main`.

## Failure handling

### Write failure

Candidate remains isolated. No promotion.

### Test/audit failure

Candidate remains available for inspection unless cleanup policy removes it later.

### Promotion failure

Do not mutate local state to imply success. Preserve the audited candidate and return structured conflict/error details.

### Partial model execution

A candidate with partial failure is not promotable by default.

## Tests

### Integration

Exercise a real or representative Nessie + Iceberg environment:

1. create branch from main;
2. apply table/view/incremental changes;
3. verify main is unchanged;
4. run tests;
5. promote candidate;
6. verify main now contains candidate state;
7. create conflicting target advancement;
8. verify stale/conflicting candidate cannot silently promote;
9. verify rollback to known prior reference state.

### Permission tests

Where feasible, verify candidate-writer credentials cannot promote if promotion permissions are absent.

## Acceptance criteria

Phase 5 is complete when:

1. a Nessie reference can be selected explicitly as a transform environment;
2. candidate branches can be created and tracked by Phlo Transform;
3. `apply` writes candidate data without changing `main`;
4. contracts/tests/schema policy run before publication;
5. `promote` merges only a successfully audited, still-valid candidate;
6. stale or conflicting candidates are blocked with useful diagnostics;
7. promotion state is persisted as a reproducible artifact;
8. rollback to a known previous environment state works safely;
9. table, view and incremental models all work on candidate references;
10. end-to-end Nessie/Iceberg integration tests run in CI or a dedicated integration environment.

## Explicitly deferred

- automatic organization-wide approval workflows;
- complex branch retention policies;
- cross-catalog publication semantics;
- data diff gates, added in Phase 6;
- deployment UI.
