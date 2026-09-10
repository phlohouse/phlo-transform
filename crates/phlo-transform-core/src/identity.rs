//! Stable logical identity for models and external sources.
//!
//! Identity is related to, but distinct from, physical location. A model
//! derives its identity from its namespace and namespace-relative path unless
//! it pins one with `-- @id`.

use std::fmt;

/// A logical namespace, for example `assay` or `assay_ingest`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Namespace(String);

impl Namespace {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for Namespace {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for Namespace {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A logical model identity.
///
/// Human-facing form: `assay.staging.raw`.
/// Canonical URI form: `model://assay/staging/raw`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelId {
    namespace: Namespace,
    path: Vec<String>,
}

impl ModelId {
    /// Build an identity from a namespace and namespace-relative path.
    ///
    /// The path must be non-empty and every component must be a valid
    /// identifier-like segment. This prevents identities that cannot be
    /// unambiguously rendered back to dotted notation.
    pub fn new(namespace: Namespace, path: Vec<String>) -> Result<Self, IdentityError> {
        validate_segment(namespace.as_str())?;
        if path.is_empty() {
            return Err(IdentityError::EmptyPath);
        }
        for segment in &path {
            validate_segment(segment)?;
        }
        Ok(Self { namespace, path })
    }

    /// Parse a dotted logical name such as `assay.staging.raw`.
    pub fn parse(value: &str) -> Result<Self, IdentityError> {
        let value = value.trim();
        let value = value
            .strip_prefix("model://")
            .map(|rest| rest.replace('/', "."))
            .unwrap_or_else(|| value.to_string());

        let parts: Vec<String> = value.split('.').map(str::to_string).collect();
        if parts.len() < 2 {
            return Err(IdentityError::MissingPath(value));
        }
        let namespace = Namespace::new(parts[0].clone());
        let path = parts[1..].to_vec();
        ModelId::new(namespace, path)
    }

    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    /// The namespace-relative name, e.g. `staging.raw`.
    pub fn local_name(&self) -> String {
        self.path.join(".")
    }

    /// The full dotted name, e.g. `assay.staging.raw`.
    pub fn logical_name(&self) -> String {
        format!("{}.{}", self.namespace, self.local_name())
    }

    /// The canonical URI, e.g. `model://assay/staging/raw`.
    pub fn uri(&self) -> String {
        format!(
            "model://{}/{}",
            self.namespace.as_str(),
            self.path.join("/")
        )
    }

    /// The final component of the path, e.g. `raw`.
    pub fn last_segment(&self) -> &str {
        self.path.last().expect("model ids have a non-empty path")
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.logical_name())
    }
}

/// An external relation not produced by the workspace.
///
/// Example: `external.raw_assay_results`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId {
    parts: Vec<String>,
}

impl SourceId {
    pub fn new(parts: Vec<String>) -> Result<Self, IdentityError> {
        if parts.is_empty() {
            return Err(IdentityError::EmptyPath);
        }
        // External relation names are opaque: they may use quoting or dialect
        // specific characters, so only emptiness is rejected here.
        Ok(Self { parts })
    }

    pub fn parts(&self) -> &[String] {
        &self.parts
    }

    /// The dotted physical name, e.g. `external.raw_assay_results`.
    pub fn logical_name(&self) -> String {
        self.parts.join(".")
    }

    /// The canonical URI, e.g. `source://external/raw_assay_results`.
    pub fn uri(&self) -> String {
        format!("source://{}", self.parts.join("/"))
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.logical_name())
    }
}

/// Identity validation failures.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("model identity `{0}` must have at least a namespace and a name")]
    MissingPath(String),
    #[error("model identity is missing a name component")]
    EmptyPath,
    #[error("`{0}` is not a valid identifier segment")]
    InvalidSegment(String),
}

fn validate_segment(segment: &str) -> Result<(), IdentityError> {
    let valid = !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(IdentityError::InvalidSegment(segment.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_display_and_uri() {
        let id = ModelId::new(
            Namespace::from("assay"),
            vec!["staging".to_string(), "raw".to_string()],
        )
        .unwrap();
        assert_eq!(id.logical_name(), "assay.staging.raw");
        assert_eq!(id.uri(), "model://assay/staging/raw");
        assert_eq!(id.local_name(), "staging.raw");
        assert_eq!(id.last_segment(), "raw");
    }

    #[test]
    fn parses_dotted_name() {
        let id = ModelId::parse("assay.raw").unwrap();
        assert_eq!(id.logical_name(), "assay.raw");
        assert_eq!(id.path(), ["raw"]);
    }

    #[test]
    fn parses_uri_form() {
        let id = ModelId::parse("model://assay/staging/raw").unwrap();
        assert_eq!(id.logical_name(), "assay.staging.raw");
    }

    #[test]
    fn rejects_missing_path() {
        assert_eq!(
            ModelId::parse("assay"),
            Err(IdentityError::MissingPath("assay".to_string()))
        );
    }

    #[test]
    fn rejects_invalid_segments() {
        assert!(ModelId::parse("assay.bad name").is_err());
        assert!(ModelId::new(Namespace::from("assay"), vec!["bad name".into()]).is_err());
    }

    #[test]
    fn external_sources_allow_opaque_names() {
        let source = SourceId::new(vec!["weird name".into()]).unwrap();
        assert_eq!(source.logical_name(), "weird name");
    }
}
