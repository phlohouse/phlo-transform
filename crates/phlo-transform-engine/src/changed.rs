//! The change set behind the `changed` selector.
//!
//! Today "changed" is derived from recorded state: a model counts as changed
//! when no version is recorded for the environment, or the recorded
//! materialised version hash differs from the desired version hash. Because
//! dependency versions are a version input, a model whose upstream moved —
//! including ephemeral upstreams — shows up changed without needing the
//! recorded version detail (the detail exists to explain *which* input moved,
//! not to detect that one did).
//!
//! A Git-aware provider (`--since <ref>`) will feed the same selector path;
//! resolution in `phlo-transform-core` does not care how the set was built.

use std::collections::BTreeSet;
use std::sync::Arc;

use phlo_transform_core::{Compilation, ModelId};

use crate::error::EngineError;
use crate::state::StateStore;

/// Models whose desired version differs from the version recorded for
/// `environment` — the meaning of the `changed` selector.
///
/// With no state store, nothing can be proven unchanged, so every model is
/// reported changed. Models are returned in deterministic order.
pub fn changed_models(
    compilation: &Compilation,
    state: Option<&Arc<dyn StateStore>>,
    environment: Option<&str>,
) -> Result<BTreeSet<ModelId>, EngineError> {
    let mut changed = BTreeSet::new();
    for model in &compilation.models {
        let changed_model = match state {
            Some(state) => {
                match state.materialized_version(&model.id.logical_name(), environment)? {
                    Some(record) => record.version.hash != model.version.hash,
                    None => true,
                }
            }
            None => true,
        };
        if changed_model {
            changed.insert(model.id.clone());
        }
    }
    Ok(changed)
}
