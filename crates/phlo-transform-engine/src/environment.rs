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
    pub base: ReferenceInfo,
    pub candidate: ReferenceInfo,
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
        created_branch,
        catalog: spec.catalog.clone(),
    })
}
