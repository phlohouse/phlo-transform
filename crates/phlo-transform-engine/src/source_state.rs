//! Source-state collection for model versioning.
//!
//! External source states are observed through the adapter (for Iceberg this
//! is the latest snapshot id) and lowered into a `SourceStateProvider` before
//! compilation, so source changes invalidate dependent model versions.

use phlo_transform_core::{Relation, SourceId, StaticSourceStateProvider};

use crate::adapter::Adapter;
use crate::error::EngineError;

/// Build the physical relation for a logical source.
pub fn relation_for_source(
    source: &SourceId,
    default_catalog: Option<&str>,
    default_schema: Option<&str>,
) -> Relation {
    let parts = source.parts();
    let schema = || default_schema.unwrap_or("default").to_string();
    let catalog = || default_catalog.map(str::to_string);
    match parts.len() {
        0 => Relation {
            catalog: catalog(),
            schema: schema(),
            table: String::new(),
        },
        1 => Relation {
            catalog: catalog(),
            schema: schema(),
            table: parts[0].clone(),
        },
        2 => Relation {
            catalog: catalog(),
            schema: parts[0].clone(),
            table: parts[1].clone(),
        },
        _ => Relation {
            catalog: Some(parts[0].clone()),
            schema: parts[1].clone(),
            table: parts[2..].join("."),
        },
    }
}

/// Observe the state of every source and build a provider.
pub async fn collect_source_states(
    adapter: &dyn Adapter,
    sources: &[SourceId],
    default_catalog: Option<&str>,
    default_schema: Option<&str>,
) -> Result<StaticSourceStateProvider, EngineError> {
    let mut provider = StaticSourceStateProvider::new();
    for source in sources {
        let relation = relation_for_source(source, default_catalog, default_schema);
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
