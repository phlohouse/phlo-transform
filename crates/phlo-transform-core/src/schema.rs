//! Catalogue schema provider.
//!
//! External relation schemas enter compilation through this interface, so the
//! compiler remains testable without a live warehouse. The Trino adapter
//! implements the same contract in `phlo-transform-trino`.

use std::collections::BTreeMap;

use crate::identity::SourceId;
use crate::semantic::{DataType, Nullability};

/// A column of an external relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaColumn {
    pub name: String,
    pub data_type: DataType,
    pub nullability: Nullability,
}

/// The schema of an external relation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationSchema {
    pub columns: Vec<SchemaColumn>,
}

impl RelationSchema {
    pub fn new(columns: Vec<SchemaColumn>) -> Self {
        Self { columns }
    }

    pub fn column(&self, name: &str) -> Option<&SchemaColumn> {
        self.columns.iter().find(|column| column.name == name)
    }
}

/// Supplies schemas for relations not produced by the workspace.
pub trait SchemaProvider: Send + Sync {
    fn source_schema(&self, source: &SourceId) -> Option<RelationSchema>;
}

/// A provider that knows nothing; external columns are unknown.
#[derive(Clone, Debug, Default)]
pub struct EmptySchemaProvider;

impl SchemaProvider for EmptySchemaProvider {
    fn source_schema(&self, _source: &SourceId) -> Option<RelationSchema> {
        None
    }
}

/// A fixed provider for tests and fixtures.
#[derive(Clone, Debug, Default)]
pub struct StaticSchemaProvider {
    schemas: BTreeMap<String, RelationSchema>,
}

impl StaticSchemaProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, source: &str, schema: RelationSchema) -> &mut Self {
        self.schemas.insert(source.to_string(), schema);
        self
    }
}

impl SchemaProvider for StaticSchemaProvider {
    fn source_schema(&self, source: &SourceId) -> Option<RelationSchema> {
        self.schemas.get(&source.logical_name()).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_trino_types() {
        assert_eq!(DataType::parse_trino("VARCHAR"), DataType::Varchar);
        assert_eq!(DataType::parse_trino("bigint"), DataType::BigInt);
        assert_eq!(DataType::parse_trino("timestamp(3)"), DataType::Timestamp);
        assert_eq!(DataType::parse_trino("nonsense"), DataType::Unknown);
    }
}
