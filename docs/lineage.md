# Lineage

Phlo's lineage is a **canonical graph built once from compiler output**, not a
set of reports derived independently. `lineage`, `impact`, planning,
`openlineage` export and future consumers all read the same structure:

```text
Compilation
   ↓
LineageGraph          (crates/phlo-transform-core/src/lineage.rs)
   ↓
OpenLineageExporter   (crates/phlo-transform-openlineage)
```

```text
              Phlo Lineage Graph
                     │
       ┌─────────────┼──────────────┐
       ▼             ▼              ▼
     CLI/API     OpenLineage      Impact
                     │
               ┌─────┴─────┐
               ▼           ▼
         OpenMetadata    DataHub
```

## Nodes

| Kind | Identity | Notes |
|---|---|---|
| model | `model://main/customers` | the transform that produces a dataset |
| dataset | `dataset://main/customers` | model outputs, sources and seeds |
| column | `dataset://main/customers#total` | endpoint of column-level derives edges |
| test | `test://check_clean` | SQL tests and generated assertion tests |

Datasets are the seam that decouples lineage from "a transform model". A model
output, a declared source and a CSV seed are all `dataset://` nodes; a future
ingestion system contributes the same node kind (`DatasetKind::External`
exists for it) without the graph changing shape:

```text
LIMS ─▶ dlt ─▶ dataset://raw/lims/samples ─▶ model ─▶ dataset://assay/results
```

Versioned datasets (`dataset://assay/raw @ A → dataset://assay/results @ B`)
attach to the same node shape later: model nodes already carry the
content-addressed `version`, so a version-aware view is a refinement, not a
redesign. Run and ingestion-asset nodes extend the enum the same way.

## Edges

| Kind | Direction | Meaning |
|---|---|---|
| `input` | dataset → model | the model reads the dataset — the canonical dependency edge |
| `output` | model → dataset | the model produces the dataset |
| `derives` | model→model, dataset→dataset, column→column | value lineage |
| `contains` | dataset → column | the column belongs to the dataset |
| `tests` | dataset → test | the test consumes and asserts on the dataset |

Every input connects the same way — a declared source, a seed and an
upstream model's output dataset are all `dataset ──input──▶ model`:

```text
model A ──output──▶ dataset A ──input──▶ model B ──output──▶ dataset B
```

The `model → model` and `dataset → dataset` `derives` edges are rollups over
that chain for one-hop queries — never a substitute for the `input` edge.
Because tests are consumers (`dataset → test`), `impact(dataset)` reaches
them with no special-casing.

Only column-level `derives` edges carry semantic metadata today; every field
is optional so other edge kinds stay plain:

- `directness`: `direct` (values flow in) or `indirect` (the input influences
  which rows/values are produced — join keys, filters, grouping and sort keys).
- `transformation`: the OpenLineage subtype vocabulary — `identity`,
  `transformation`, `aggregation`, `join`, `filter`, `group_by`, `sort`,
  `window`, `conditional`.
- `confidence`: `exact` when the SQL AST proved the link; `unknown` when part
  of the query could not be analysed. `inferred`, `declared` and `runtime`
  exist for future producers — nothing is silently elevated to `exact`.
- `expression`: the SQL text responsible, when recorded.

### Examples

```text
raw.titre ──direct/identity──────▶ clean.titre
raw.titre ──direct/transformation▶ clean.log_titre      -- ln(titre)
raw.sample_type ──indirect/filter▶ clean.log_titre      -- where sample_type = 'cell'
clean.sample_id ──indirect/join──▶ joined.log_titre     -- join keys on both sides
```

## Column semantics

The analyzer walks the real SQL AST (sqlparser), so directness and
transformation are proven rather than guessed:

- bare columns and aliases are `direct`/`identity`;
- value-producing expressions (`ln(x)`, `a * b`, `cast`) are
  `direct`/`transformation`;
- aggregates (`sum`, `count`, `avg`) are `direct`/`aggregation`;
- `join … on`, `where`/`having`/`qualify`, `group by`, `order by` and window
  partition/order keys contribute their columns as `indirect` inputs of the
  matching kind;
- when a construct cannot be fully analysed (unknown input schemas, table
  functions, subquery function arguments), the column is recorded with
  `confidence: unknown` instead of a partial claim presented as exact.

Seeds get their schema from the CSV header when the catalogue does not know
the relation, so `select *` against a seed resolves like any typed source.

## Query API

The graph is indexed; commands traverse it instead of re-walking the
dependency graph:

```text
upstream(node)                  upstream_transitive(node)
downstream(node)                downstream_transitive(node)
impact(node)                    column_upstream(col, indirect?)
input_datasets(model)           column_downstream(col, indirect?)
output_dataset(model)           column_*_transitive(col, indirect?)
dataset_columns(dataset)        tests_for_dataset(dataset)
dataset_by_name("a.b")          subgraph(models) / document_for(models)
document()                      → LineageDocument (JSON)
```

`document()` is the stable serialised form — `lineage --format graph` prints
it, `.phlo/transform/lineage.json` stores it, and exporters consume it.
`document_for`/`subgraph` scope the document to a selector result or a single
model: the selected models, their output datasets and columns, the datasets
they read, the columns feeding them, and the tests asserting on them.

## CLI

```bash
phlo-transform lineage                          # workspace model graph
phlo-transform lineage assay.results            # upstream/downstream models
phlo-transform lineage assay.results.titre      # column: direct/indirect/transitive
phlo-transform lineage --format graph           # canonical document (JSON)
phlo-transform lineage assay.results --format graph        # scoped document
phlo-transform lineage --format openlineage     # OpenLineage export
phlo-transform lineage --diff main              # semantic diff vs merge-base(main, HEAD)
phlo-transform lineage --diff main feature/foo  # exact ref → ref comparison
phlo-transform impact assay.results             # downstream models/tests
phlo-transform impact assay.results.titre       # downstream columns/models/tests
phlo-transform impact external.samples.volume   # source-column impact
phlo-transform impact raw.raw_events.status     # seed-column impact
```

Human column output prints `Confidence:` when it is not `exact` and lists
`Indirect:` inputs separately from `Direct:` ones. `--format` output is JSON
regardless of `--json`; selector terms scope it the same way they scope
`plan`/`run`.

## Diffing lineage across refs

`lineage --diff <git-ref>` answers "what does this branch change about the
graph itself" — the semantic complement to `branch_diff`'s data comparison.
One ref uses the same merge-base semantics as `--since`: the base is
`merge-base(ref, HEAD)`, so a feature branch is compared against where it
diverged — never against the other ref's current head. Two refs
(`lineage --diff main feature/foo`) compare the exact refs with no worktree
involved. The base side is compiled for real: the workspace subtree at the
baseline commit is materialised into a temporary directory (`git ls-tree` +
`cat-file`, never a worktree, so the checkout is untouched), discovered and
compiled with the same options, then the two `LineageGraph`s are compared
node-for-node and edge-for-edge. Edges are a multiset — parallel edges
between the same two nodes (say, a column that is both selected and
filtered on) are compared by their full metadata, not collapsed:

- `nodes_added` / `nodes_removed` — models, datasets, columns and tests
  that exist on only one side;
- `nodes_changed` — the field-level delta (`version`, `materialization`,
  `target`, `type`, `path`, …) for nodes present on both sides;
- `edges_added` / `edges_removed` — dependency structure: a model that
  started (or stopped) reading a dataset is a changed `input` edge, not
  just a changed file;
- `edges_changed` — column-level `derives` edges whose `transformation`,
  `directness`, `confidence` or `expression` moved;
- `impacts` — for every removed node, the base-side consumers it orphans
  (the models and tests that were reading it);
- `edge_impacts` — for every removed or metadata-moved edge, the base-side
  downstream that loses or alters that lineage path, so dropping a
  dependency while both nodes survive still reports impact.

```text
Lineage diff vs main (merge-base 1a2b3c4d5e6f)

Removed
  - assay.legacy (model)
  - assay.legacy (dataset)

Changed
  ~ assay.results (model)
      version: 3f2a1c9d… -> 9be21f7a…

Edges
  + dataset://assay/clean --input-> model://assay/results
  - dataset://assay/legacy --input-> model://assay/results

Impacts
  model://assay/legacy orphans: assay.report, test://legacy_range
```

The report is persisted as lineage-diff evidence in the state store — the
record `promote` audits, portable to whichever machine runs the promotion
— and exported to `.phlo/transform/lineage_diff.json`; `--json`
prints the same document. Beyond the diff itself the evidence binds its
provenance: `base_kind` (`merge-base` or `ref`), the resolved `base_commit`,
the candidate's git head and worktree state, a `lineage_hash` fingerprint
of the candidate's canonical graph, and — when `--ref`/`--from`
names a Nessie environment that resolves — the candidate and target branch
hashes it was produced for. `promote` reads that binding: a lineage record
covering this candidate and target at their current commits *and* whose
fingerprint still matches the compiled workspace reports
`current`; one produced for another pair, before either side moved, before
the candidate's definitions changed, or naming refs that no longer resolve,
reports `stale` with the reason rather than standing as evidence; an
unbound record is advisory only. A base ref that fails to load or compile
reports its diagnostics and exits non-zero rather than diffing a partial
graph.

In the branch workflow this is the code-side review step: `branch_diff`
proves what the data would look like after promotion, `lineage --diff`
proves what the *definitions* would look like — new dependencies, dropped
models, orphaned consumers — before `promote` merges the data branch.

## OpenLineage export

`phlo-transform-openlineage` maps the graph to a design-time document — a
bare JSON array containing one `JobEvent` per model (inputs, outputs,
`columnLineage` facet) and one `DatasetEvent` per dataset (`schema`,
`datasetType`, `symlinks` for physical targets). Every event carries the
required `eventTime`, `producer` and `schemaURL` fields and validates
against the OpenLineage 2-0-2 spec (the test suite checks this against the
vendored JSON schemas). The document serializes as the event array itself,
so it is a valid request body for the OpenLineage batch endpoint.

Each standard facet names its own published schema URL; Phlo metadata with
no standard equivalent rides in namespaced `phlo_job` / `phlo_dataset`
facets whose schemas are published under `schemas/facets/` in this
repository, referenced by the immutable `schemas-facets-1.0.0` tag rather
than a branch. `datasetType` reflects the materialization — `TABLE` for
tables/incrementals, `VIEW` for views, `JOB_OUTPUT`/`TEMPORARY` for
ephemeral models — and `symlinks` only appear when a physical relation
actually exists. No `run` is fabricated — this is declared lineage, not an
observed run.

OpenLineage is an **export boundary**, not the internal model: none of its
types appear in the compiler. OpenMetadata, DataHub and similar tools ingest
the export; native adapters are only warranted if the format proves
insufficient.

The same document is written to `.phlo/transform/openlineage.json` alongside
`manifest.json`/`graph.json`/`lineage.json`.

## Future extensions the shape already allows

- **Ingestion lineage** — external systems contribute `dataset`/`external`
  nodes and `derives` edges into the same graph.
- **Branch lineage diff** — compare two `LineageDocument`s (node/edge sets
  are deterministic and serialisable).
- **Versioned lineage** — dataset@version nodes refine existing dataset nodes.
- **Contract impact** — contracts hang off dataset columns; impact traversal
  is already indexed.
- **Agent APIs** — `document()` is the wire format.
