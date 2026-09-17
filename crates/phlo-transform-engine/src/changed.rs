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

use phlo_transform_core::{Compilation, Materialization, ModelId};

use crate::error::EngineError;
use crate::state::StateStore;

/// Models whose desired version differs from the version recorded for
/// `environment` — the meaning of the `changed` selector.
///
/// Ephemeral models are never materialised, so they have no recorded
/// version to compare against and are skipped entirely; an edit to an
/// ephemeral still propagates into dependents' versions through the
/// dependency hash, so `changed+` keeps working.
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
        if model.config.materialization == Materialization::Ephemeral {
            continue;
        }
        let changed_model = match state {
            Some(state) => {
                match state.materialized_version(&model.id.logical_name(), environment)? {
                    Some(record) => {
                        // A moved target counts even when the version is
                        // unchanged — the environment's record still needs
                        // the retarget adoption (or rebuild) the plan will
                        // decide.
                        record.version.hash != model.version.hash
                            || record.target != model.target.display()
                    }
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
