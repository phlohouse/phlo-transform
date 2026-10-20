# Phlo integration

Phlo consumes this engine through the `phlo-transform` Python package in the
[Phlo monorepo](https://github.com/phlohouse/phlo/tree/main/packages/phlo-transform).
The package is an adapter, not a second compiler: every semantic answer comes
from this CLI's JSON reports.

## Contract the adapter depends on

| Command | Fields used |
| --- | --- |
| `list --json` | `models[].{name, namespace, materialization, target, version, path, tags, depends_on, sources}`, `tests[].{name, targets}`, `sources[].name` |
| `run --json` | `status`, `run_id`, `plan_id`, `counts`, `models[].{model, duration_ms, query_id}`, `tests[].{test, status, row_count, error}` |
| `test --json` | `tests[].{test, status, row_count, error}` |
| `check --json`, `plan --json`, `lineage --json`, `doctor --json` | Passed through unchanged by `phlo transform <command> --json` |

These fields are treated as a machine-facing API. Additive fields are safe;
renames or removals need a coordinated adapter change.

## Mapping to Phlo assets

- One compiled model becomes one Phlo asset. The default key is the last
  dotted name segment (`analytics.stg_orders` becomes `stg_orders`), which
  matches a dbt model name so a migrated project keeps its asset keys.
  `PHLO_TRANSFORM_ASSET_KEY=logical` keeps the namespace prefix.
- `depends_on` becomes asset dependencies. Source-to-asset edges are declared
  by the project in the provider-owned table below, because the engine
  resolves sources to physical relations and does not know the ingestion
  assets.
- Each test whose `targets` include the model becomes a blocking asset check.
- Materialising an asset runs `run --select <model> --json`; a failed model
  or test becomes a failed asset run, never a silent skip.

```toml
# phlo.toml - ignored by the engine, read by the Phlo provider
[phlo_transform.sources]
"raw.orders" = "sling_orders"
```

## Orchestrated execution

The adapter resolves the Phlo query engine and passes `--adapter trino`, the
endpoint and catalog, and `--trino-catalog`. A non-main runtime ref is
provisioned through Phlo's `RefQueryCatalogManager` and passed as
`--catalog` with `--environment <ref>`, so writes land in the ref's catalog
rather than `main`.

Without orchestration the adapter does not guess: the CLI's own adapter
defaults and error guidance apply unless `PHLO_TRANSFORM_ADAPTER` is set.

## Version compatibility

The provider pins a release for `phlo transform install`
(`PHLO_TRANSFORM_RELEASE_VERSION`, default `0.1.0`) and verifies the release
archive checksum. The JSON contract above is what makes an independently
released engine safe to upgrade.
