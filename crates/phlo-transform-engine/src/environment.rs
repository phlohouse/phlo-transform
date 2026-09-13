//! Environment provisioning: Nessie branch plus a branch-scoped catalog.
//!
//! For Nessie/Iceberg targets a candidate environment is a Nessie branch. Trino
//! cannot switch the Nessie reference of an existing catalog at query time, so
//! each environment is provisioned as a dynamic Trino catalog pointing at the
//! branch (verified against Trino 483). The engine stays adapter-agnostic: the
//! adapter owns catalog provisioning.

use serde::{Deserialize, Serialize};

use phlo_transform_nessie::{NessieClient, ReferenceInfo};

use crate::adapter::{Adapter, CatalogRequest};
use crate::error::EngineError;

/// A request to ensure an environment exists.
#[derive(Clone, Debug)]
pub struct EnvironmentSpec {
    pub base_ref: String,
    pub candidate_ref: String,
    /// Nessie API base URI, e.g. `http://nessie:19120`.
    pub nessie_uri: Option<String>,
    /// Iceberg warehouse location for the candidate catalog.
    pub warehouse: Option<String>,
    /// Physical catalog name for the candidate.
    pub catalog: String,
}

/// The provisioned environment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentSetup {
    /// The base reference as resolved at provisioning time — informational:
    /// it says which ref `base_ref` named, not what the candidate was cut
    /// from. Provenance lives in `created_from`.
    pub base: ReferenceInfo,
    pub candidate: ReferenceInfo,
    /// Immutable provenance: the reference (and commit) the candidate branch
    /// was actually created from. Recorded only when provable — when this
    /// call created the branch, or a prior artifact recorded it. `None` for
    /// a pre-existing branch with no recorded origin: promotion must never
    /// silently redefine an existing branch's base as today's target.
    #[serde(default)]
    pub created_from: Option<ReferenceInfo>,
    pub created_branch: bool,
    pub catalog: String,
}

/// Ensure the candidate branch and its catalog exist.
pub async fn ensure_environment(
    nessie: &dyn NessieClient,
    adapter: &dyn Adapter,
    spec: &EnvironmentSpec,
) -> Result<EnvironmentSetup, EngineError> {
    let base = nessie
        .get_reference(&spec.base_ref)
        .await
        .map_err(|error| EngineError::Environment(error.to_string()))?
        .ok_or_else(|| {
            EngineError::Environment(format!("base reference `{}` was not found", spec.base_ref))
        })?;

    let (candidate, created_branch) = if spec.candidate_ref == spec.base_ref {
        (base.clone(), false)
    } else {
        match nessie
            .get_reference(&spec.candidate_ref)
            .await
            .map_err(|error| EngineError::Environment(error.to_string()))?
        {
            Some(existing) => (existing, false),
            None => (
                nessie
                    .create_branch(&spec.candidate_ref, &base)
                    .await
                    .map_err(|error| EngineError::Environment(error.to_string()))?,
                true,
            ),
        }
    };
    // Provenance is recorded only when it is provable: a branch we just
    // created is by construction cut from `base`; a pre-existing branch's
    // origin is unknown here — the caller may preserve a prior artifact's
    // `created_from`, but this function must not guess.
    let created_from = if created_branch || candidate.name == base.name {
        Some(base.clone())
    } else {
        None
    };

    adapter
        .ensure_catalog(&CatalogRequest {
            catalog: spec.catalog.clone(),
            reference: Some(spec.candidate_ref.clone()),
            nessie_uri: spec.nessie_uri.clone(),
            warehouse: spec.warehouse.clone(),
        })
        .await
        .map_err(EngineError::Adapter)?;

    Ok(EnvironmentSetup {
        base,
        candidate,
        created_from,
        created_branch,
        catalog: spec.catalog.clone(),
    })
}

/// The conventional catalog name for a candidate environment: `phlo_<ref>`
/// with characters unsafe in a catalog name folded to `_`. Provisioning and
/// diff/promotion evidence resolution share this convention.
pub fn catalog_name(reference: &str) -> String {
    let mut name = String::from("phlo_");
    let mut previous_underscore = false;
    for character in reference.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
            previous_underscore = false;
        } else if !previous_underscore {
            name.push('_');
            previous_underscore = true;
        }
    }
    name.trim_end_matches('_').to_string()
}
