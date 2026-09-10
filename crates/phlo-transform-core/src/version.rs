//! Content-addressed model versions.
//!
//! Identity is derived from semantics, not raw file text: the canonical SQL
//! AST, the semantic parts of configuration, contracts, dependency versions,
//! source state and compiler semantics. Formatting and comment-only changes do
//! not alter the version.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::identity::{ModelId, SourceId};

/// Bumped only when compiler/materialisation semantics could change outputs.
pub const COMPILER_SEMANTICS_VERSION: u32 = 1;

/// Component hashes that make up a model version.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ModelVersion {
    pub hash: String,
    pub sql_hash: String,
    pub config_hash: String,
    pub contract_hash: String,
    pub dependency_hash: String,
    pub source_state_hash: String,
    pub compiler_version: String,
    pub target_hash: String,
}

impl ModelVersion {
    /// The short display form used in plans and inspect output.
    pub fn short(&self) -> &str {
        self.hash.get(..7).unwrap_or(&self.hash)
    }
}

/// Inputs used to derive a model version.
#[derive(Clone, Debug, Default)]
pub struct VersionInputs {
    /// Canonical SQL string (serialised AST).
    pub canonical_sql: String,
    /// Stable representation of semantic configuration.
    pub config: String,
    /// Stable representation of the contract and assertions.
    pub contract: String,
    /// `dependency logical name -> dependency version hash`.
    pub dependencies: Vec<(String, String)>,
    /// `source logical name -> source state`.
    pub sources: Vec<(String, String)>,
    /// Physical target display and materialisation.
    pub target: String,
}

/// Derive a model version from its inputs.
pub fn model_version(inputs: &VersionInputs) -> ModelVersion {
    let mut dependencies = inputs.dependencies.clone();
    dependencies.sort();
    dependencies.dedup();

    let dependency_hash = hash_lines(
        dependencies
            .iter()
            .map(|(name, version)| format!("{name}={version}")),
    );

    let mut sources = inputs.sources.clone();
    sources.sort();
    sources.dedup();
    let source_state_hash = hash_lines(
        sources
            .iter()
            .map(|(name, state)| format!("{name}={state}")),
    );

    let sql_hash = sha256_hex(&inputs.canonical_sql);
    let config_hash = sha256_hex(&inputs.config);
    let contract_hash = sha256_hex(&inputs.contract);
    let target_hash = sha256_hex(&inputs.target);
    let compiler_version = COMPILER_SEMANTICS_VERSION.to_string();

    let combined = [
        ("sql", sql_hash.clone()),
        ("config", config_hash.clone()),
        ("contract", contract_hash.clone()),
        ("dependencies", dependency_hash.clone()),
        ("sources", source_state_hash.clone()),
        ("compiler", compiler_version.clone()),
        ("target", target_hash.clone()),
    ];
    let hash = hash_lines(
        combined
            .iter()
            .map(|(label, value)| format!("{label}={value}")),
    );

    ModelVersion {
        hash,
        sql_hash,
        config_hash,
        contract_hash,
        dependency_hash,
        source_state_hash,
        compiler_version,
        target_hash,
    }
}

/// Supplies source state for versioning external relations.
pub trait SourceStateProvider: Send + Sync {
    fn source_state(&self, source: &SourceId) -> Option<String>;
}

/// A provider that reports no source state.
#[derive(Clone, Debug, Default)]
pub struct EmptySourceStateProvider;

impl SourceStateProvider for EmptySourceStateProvider {
    fn source_state(&self, _source: &SourceId) -> Option<String> {
        None
    }
}

/// A fixed source-state provider for tests.
#[derive(Clone, Debug, Default)]
pub struct StaticSourceStateProvider {
    states: std::collections::BTreeMap<String, String>,
}

impl StaticSourceStateProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, source: &str, state: impl Into<String>) -> &mut Self {
        self.states.insert(source.to_string(), state.into());
        self
    }
}

impl SourceStateProvider for StaticSourceStateProvider {
    fn source_state(&self, source: &SourceId) -> Option<String> {
        self.states.get(&source.logical_name()).cloned()
    }
}

/// The version of a model, keyed by its identity, used while compiling.
pub type ModelVersions = std::collections::BTreeMap<ModelId, ModelVersion>;

fn hash_lines<I, S>(lines: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_ref().as_bytes());
        hasher.update(b"\n");
    }
    hex(hasher.finalize())
}

/// Lower-case hex SHA-256 of a string.
pub fn sha256_hex(value: &str) -> String {
    hex(Sha256::digest(value.as_bytes()))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::new();
    for byte in bytes.as_ref() {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_inputs_hash_equally() {
        let a = model_version(&VersionInputs {
            canonical_sql: "SELECT * FROM assay.raw".to_string(),
            ..Default::default()
        });
        let b = model_version(&VersionInputs {
            canonical_sql: "SELECT * FROM assay.raw".to_string(),
            ..Default::default()
        });
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn changed_sql_changes_hash() {
        let a = model_version(&VersionInputs {
            canonical_sql: "SELECT * FROM assay.raw".to_string(),
            ..Default::default()
        });
        let b = model_version(&VersionInputs {
            canonical_sql: "SELECT id FROM assay.raw".to_string(),
            ..Default::default()
        });
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn dependency_change_changes_hash() {
        let base = model_version(&VersionInputs {
            dependencies: vec![("assay.raw".to_string(), "v1".to_string())],
            ..Default::default()
        });
        let changed = model_version(&VersionInputs {
            dependencies: vec![("assay.raw".to_string(), "v2".to_string())],
            ..Default::default()
        });
        assert_ne!(base.hash, changed.hash);
        assert_ne!(base.dependency_hash, changed.dependency_hash);
        assert_eq!(base.sql_hash, changed.sql_hash);
    }
}
