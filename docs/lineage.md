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
| `input` | dataset → model | the model reads the dataset |
| `output` | model → dataset | the model produces the dataset |
| `derives` | model→model, dataset→dataset, column→column | value lineage |
| `contains` | dataset → column | the column belongs to the dataset |
| `tests` | test → dataset | the test asserts on the dataset |

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
phlo-transform impact assay.results             # downstream models/tests
phlo-transform impact assay.results.titre       # downstream columns/models/tests
phlo-transform impact external.samples.volume   # source-column impact
phlo-transform impact raw.raw_events.status     # seed-column impact
```

Human column output prints `Confidence:` when it is not `exact` and lists
`Indirect:` inputs separately from `Direct:` ones. `--format` output is JSON
regardless of `--json`; selector terms scope it the same way they scope
`plan`/`run`.

## OpenLineage export

`phlo-transform-openlineage` maps the graph to a design-time document — one
`JobEvent` per model (inputs, outputs, `columnLineage` facet) and one
`DatasetEvent` per dataset (`schema`, `datasetType`, `symlinks` for physical
targets). Phlo metadata with no standard equivalent rides in custom `phlo`
facets (dataset URI/kind, materialization/workflow/version, per-field
confidence). No `RunEvent` is fabricated — this is declared lineage, not an
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
