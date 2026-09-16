//! Source-state collection for model versioning.
//!
//! External source states are observed through the adapter (for Iceberg this
//! is the latest snapshot id) and lowered into a `SourceStateProvider` before
//! compilation, so source changes invalidate dependent model versions.

use phlo_transform_core::{CompiledSeed, Relation, SourceId, StaticSourceStateProvider};

pub use phlo_transform_core::resolve::relation_for_source;

use crate::adapter::Adapter;
use crate::error::EngineError;

/// The schema an adapter targets when a project configures no default.
/// DuckDB defaults to `main`; other adapters fall back to `default`.
pub fn adapter_default_schema(adapter_name: &str) -> Option<&'static str> {
    match adapter_name {
        "duckdb" => Some("main"),
        _ => None,
    }
}

/// The physical relation a seed loads into.
///
/// The seed's compiled schema already reflects `[seed.*]`/`[seeds]`/workspace
/// defaults; the adapter default is the last fallback.
pub fn seed_relation(
    seed: &CompiledSeed,
    default_catalog: Option<&str>,
    default_schema: Option<&str>,
    adapter_name: &str,
) -> Relation {
    let fallback = default_schema
        .or_else(|| adapter_default_schema(adapter_name))
        .unwrap_or("default");
    seed.relation(default_catalog, fallback)
}

/// The seed whose target relation matches `relation`, if any.
pub fn seed_for_relation<'a>(
    seeds: &'a [CompiledSeed],
    relation: &Relation,
    default_catalog: Option<&str>,
    default_schema: Option<&str>,
    adapter_name: &str,
) -> Option<&'a CompiledSeed> {
    seeds.iter().find(|seed| {
        seed_relation(seed, default_catalog, default_schema, adapter_name).display()
            == relation.display()
    })
}

/// Observe the state of every source and build a provider.
///
/// Sources backed by a discovered CSV seed take the seed's content hash as
/// their state — no adapter call — so editing a seed invalidates downstream
/// model versions.
pub async fn collect_source_states(
    adapter: &dyn Adapter,
    sources: &[SourceId],
    seeds: &[CompiledSeed],
    default_catalog: Option<&str>,
    default_schema: Option<&str>,
) -> Result<StaticSourceStateProvider, EngineError> {
    let mut provider = StaticSourceStateProvider::new();
    for source in sources {
        let relation = relation_for_source(source, default_catalog, default_schema);
        if let Some(seed) = seed_for_relation(
            seeds,
            &relation,
            default_catalog,
            default_schema,
            adapter.name(),
        ) {
            provider.insert(&source.logical_name(), format!("csv:{}", seed.content_hash));
            continue;
        }
        match adapter.source_state(&relation).await {
            Ok(Some(state)) => {
                provider.insert(&source.logical_name(), state);
            }
            Ok(None) => {}
            Err(error) => return Err(EngineError::Adapter(error)),
        }
    }
    Ok(provider)
}
