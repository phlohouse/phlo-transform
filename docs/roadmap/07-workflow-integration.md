# Phase 7 — Workflow integration

## Objective

Integrate transformation DAGs into the wider Phlo workflow graph so data lineage and operational workflow lineage become one coherent model.

A workflow should be able to own a local `transforms/` tree without manually enumerating every SQL model as an orchestration step.

At the end of this phase, Phlo should understand relationships such as:

```text
instrument/file
      ↓
ingest task
      ↓
raw model
      ↓
clean model
      ↓
gold model
      ↓
publish/API task
```

rather than maintaining separate workflow and transformation graphs.

## Design principles

1. Transform DAG remains owned by Phlo Transform.
2. Workflow engine consumes the compiled graph through a stable interface.
3. Do not duplicate model scheduling logic in the workflow engine.
4. Workflow-local transform roots are first-class but remain part of the workspace-wide graph.
5. Cross-workflow data dependencies are explicit graph edges, not package imports.
6. Unified lineage should preserve graph type information rather than flattening everything into generic nodes.

## Workflow-local transforms

Canonical layout:

```text
workflows/
└── assay_ingest/
    ├── workflow.toml
    ├── tasks/
    └── transforms/
        ├── staging/
        │   └── raw_results.sql
        └── marts/
            └── assay_results.sql
```

The transform root namespace is normally `assay_ingest`.

## Transform group as workflow node

A workflow definition should be able to reference the transform namespace/group conceptually as:

```text
extract
  ↓
validate
  ↓
transform: assay_ingest
  ↓
publish
```

The workflow UI/compiler can expand that node into:

```text
extract
  ↓
validate
  ↓
assay_ingest.staging.raw_results
  ↓
assay_ingest.marts.assay_results
  ↓
publish
```

The exact workflow syntax belongs to Phlo workflow design, not this engine. Phlo Transform must expose enough graph metadata to make this possible.

## Integration API

Expose a stable graph representation containing at least:

```text
model id
workflow owner
transform root
upstream model/source edges
downstream model edges
execution/materialisation state
public/private visibility if configured
```

The workflow engine should use library/API structures rather than scrape CLI output.

## Unified node types

The wider graph may include:

```text
workflow task
external source
transform model
test/quality gate
published dataset
API/output
manual approval
```

Keep node types explicit.

Suggested conceptual enum:

```rust
enum WorkspaceNodeKind {
    Task,
    Source,
    TransformModel,
    QualityGate,
    PublishedDataset,
    Output,
}
```

This may live in the wider Phlo core rather than phlo-transform; avoid coupling transform internals unnecessarily.

## Cross-workflow dependencies

Example:

```text
workflows/manufacturing/transforms/batches.sql
workflows/analytics/transforms/monthly.sql
```

`monthly.sql`:

```sql
select *
from manufacturing.batches
```

This creates a real workspace graph edge:

```text
manufacturing.batches
       ↓
analytics.monthly
```

No package publishing is required.

## Ownership metadata

A model inherits workflow ownership from its transform root unless overridden.

Expose ownership in:

```bash
phlo transform inspect manufacturing.batches
```

Example:

```text
Workflow: manufacturing
Owner: Manufacturing Digital
```

Ownership should be semantic metadata, not part of physical rebuild hashing unless it genuinely changes execution.

## Visibility boundaries

Introduce only if a real need exists, but prepare the graph for:

```text
private   -> usable only within owning workflow
workspace -> reusable anywhere in current workspace
public    -> intended published contract
```

Possible directive/config:

```sql
-- @visibility workspace
```

Default should favour simplicity; likely workspace-visible unless stricter workflow encapsulation proves useful.

## Cross-workflow policy

Optional workspace policy:

```toml
[dependencies]
cross_workflow = "allow"
```

Potential values:

```text
allow
warn
error
```

Do not add approval systems or dependency registries at this stage.

## Workflow execution semantics

When a workflow reaches a transform group:

1. request/obtain a transform plan for the relevant namespace/selection;
2. apply required models using the transform scheduler;
3. receive structured completion status;
4. continue downstream workflow tasks only when the required transform outputs satisfy workflow policy.

The workflow engine must not execute individual SQL models itself.

## Partial transform failure

Suppose transform graph:

```text
A -> B
C -> D
```

If B fails but workflow output requires only D, policy may eventually permit partial success. Initial integration should be conservative: a transform-group workflow node succeeds only when all selected required models succeed.

More granular output dependencies can be introduced when a real workflow requires them.

## Quality-gate integration

Tests/contracts/data-diff status should be exposed to the wider workflow graph.

Example:

```text
transform apply
     ↓
quality gate
     ↓
publish task
```

For WAP-enabled execution, publication should normally remain Phlo Transform's `promote` operation rather than an arbitrary workflow SQL step.

## Unified lineage

Model lineage from Phase 2 can be joined with task-level lineage.

Example:

```text
Hamilton output file
       ↓
parse_results task
       ↓
raw.assay_results
       ↓
assay.cleaned_results
       ↓
assay.release_results
       ↓
publish_api task
       ↓
consumer
```

Column lineage remains available within transform-model segments.

## Impact analysis beyond transforms

Extend `impact` consumers so a model/column can report wider workflow context when the Phlo host provides it.

Example:

```text
phlo transform impact assay.release_results.result

Transform models:
  analytics.monthly_summary

Workflows:
  release_reporting

Outputs:
  assay-results API
```

The transform engine should accept/register external consumer nodes through a stable host integration interface rather than hard-code every Phlo feature.

## Run correlation

Correlate transform runs with workflow runs:

```text
workflow_run_id
transform_run_id
plan_id
nessie_ref
```

This enables one audit trail across ingestion, transformation, quality gates and publication.

## UI implications

The wider Phlo UI should be able to render a transform group collapsed:

```text
[ Transform: assay_ingest ]
```

or expanded into model nodes.

Phlo Transform should expose graph metadata, not implement the UI itself.

## Testing

### Integration fixture

Build a fixture containing:

```text
workflow task -> transform namespace -> workflow task
```

Verify:

- transform models expand in graph;
- execution ordering is correct;
- downstream workflow task waits for required transform completion;
- failed transform blocks downstream task;
- cross-workflow model dependency appears in unified graph;
- run IDs correlate correctly.

### Graph tests

Validate no duplicate logical edges/nodes arise when transform and workflow views are merged.

## Acceptance criteria

Phase 7 is complete when:

1. workflow-local `transforms/` roots are exposed as owned transform groups;
2. the workflow engine can invoke a transform group through a stable library/API boundary;
3. transform models remain scheduled by Phlo Transform, not duplicated in workflow scheduling code;
4. transform model nodes can be merged into a wider typed workspace graph;
5. cross-workflow SQL dependencies appear correctly in unified lineage;
6. transform tests/WAP status can gate downstream workflow continuation;
7. workflow and transform run IDs are correlated;
8. impact analysis can include registered non-transform consumers;
9. end-to-end workflow/transform integration tests pass.

## Explicitly deferred

- sophisticated workflow-private module systems;
- automatic approval routing between workflow owners;
- generalized package registry;
- visual graph editor;
- distributed workflow scheduler design unrelated to transforms.
