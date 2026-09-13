# Vendored OpenLineage schemas

JSON Schema files downloaded from `https://openlineage.io/spec/` so the
export tests can validate emitted events and facets offline — CI must never
fetch these at test time.

Files keep their upstream names with the spec version prefixed:
`facets/<version>_<Name>.json`. `OpenLineage-1-0-2.json` is vendored
because the 1-2-0 `ColumnLineageDatasetFacet` still references that spec
version.

To refresh, re-download from the canonical URLs:

    curl -O https://openlineage.io/spec/2-0-2/OpenLineage.json
    curl -O https://openlineage.io/spec/facets/1-0-0/SchemaDatasetFacet.json
    ...

The custom `phlo_*` facet schemas are *not* vendored — the tests read them
from `schemas/facets/` at the repository root so the published schema and
the tested schema can never drift apart. Their `_schemaURL`s reference the
immutable `schemas-facets-1.0.0` git tag rather than a branch — when the
schemas change, add a new versioned directory under `schemas/facets/` and
cut a new tag rather than editing the tagged files.
