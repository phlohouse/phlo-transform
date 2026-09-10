# Phase 7 workflow integration

Transform DAGs merge into the wider Phlo workflow graph; scheduling stays in
Phlo Transform.

## Ownership

A model in `workflows/<name>/transforms/**` inherits workflow ownership
`<name>`. It is exposed on `CompiledModel.workflow` and in `inspect`
(`workflow` field in JSON, shown in human output).

## Unified graph

`Compilation::graph_artifact()` is the stable graph interface. Nodes are typed:

- `model` (with `workflow` ownership),
- `source`,
- `quality_gate` (tests, including generated assertion tests).

Edges are typed `model`, `source` and `quality_gate`. The workflow engine can
consume this structure (or the library) instead of scraping CLI output and can
collapse a workflow's models into a single node.

## Cross-workflow dependencies

`select * from manufacturing.batches` inside another workflow creates a normal
workspace edge. Policy is declarative:

```toml
[dependencies]
cross_workflow = "allow" | "warn" | "error"
```

`warn` and `error` emit a `DEPENDENCIES001` diagnostic; `error` fails
`check`/planning.

## Consumer registration

`ConsumerRegistry` lets the Phlo host register non-transform consumers (API
outputs, published datasets). `Compilation::impact_report_with(target, registry)`
includes them; `impact_report` uses an empty registry. `impact` output has a
`consumers` field.

## Quality gates

Tests and generated assertion tests appear as `quality_gate` nodes, so a
workflow can gate a downstream task on transform quality status. WAP promotion
remains Phlo Transform's `promote` operation.

## Run correlation

Workflow/transform run correlation is represented by the environment/reference
recorded on runs. A dedicated `workflow_run_id` field is not yet added.

## Limitations

- `workflow.toml` parsing and task-level graph nodes belong to the Phlo host;
  this engine exposes transform-side metadata only.
- Partial transform-group success policy is conservative (all selected models
  must succeed).
- Visibility boundaries (`@visibility`) are not implemented.
