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

/// Safety classification for an incremental schema change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchemaChangeSafety {
    Safe,
    Review,
    FullRebuildRequired,
    Error,
}

/// Classify a desired schema against a currently materialised schema.
pub fn classify_schema_change(
    desired: &[SchemaColumn],
    current: &[SchemaColumn],
) -> SchemaChangeSafety {
    let mut safety = SchemaChangeSafety::Safe;
    let mut raise = |candidate: SchemaChangeSafety| {
        if candidate > safety {
            safety = candidate;
        }
    };

    for column in current {
        let Some(desired) = desired
            .iter()
            .find(|candidate| candidate.name == column.name)
        else {
            // A removed column is not safe to evolve incrementally.
            raise(SchemaChangeSafety::FullRebuildRequired);
            continue;
        };
        if desired.data_type != column.data_type {
            if desired.data_type.is_known()
                && column.data_type.is_known()
                && desired.data_type.is_numeric()
                && column.data_type.is_numeric()
                && DataType::widen(&desired.data_type, &column.data_type) == desired.data_type
            {
                raise(SchemaChangeSafety::Review);
            } else {
                raise(SchemaChangeSafety::Error);
            }
        }
    }

    for column in desired {
        if current.iter().any(|existing| existing.name == column.name) {
            continue;
        }
        if column.nullability == Nullability::NotNull {
            raise(SchemaChangeSafety::Review);
        }
    }

    safety
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

    fn column(name: &str, data_type: DataType, nullability: Nullability) -> SchemaColumn {
        SchemaColumn {
            name: name.to_string(),
            data_type,
            nullability,
        }
    }

    #[test]
    fn classifies_added_nullable_column_as_safe() {
        let current = vec![column("id", DataType::BigInt, Nullability::NotNull)];
        let desired = vec![
            column("id", DataType::BigInt, Nullability::NotNull),
            column("note", DataType::Varchar, Nullability::Nullable),
        ];
        assert_eq!(
            classify_schema_change(&desired, &current),
            SchemaChangeSafety::Safe
        );
    }

    #[test]
    fn classifies_removed_column_as_full_rebuild() {
        let current = vec![
            column("id", DataType::BigInt, Nullability::NotNull),
            column("legacy", DataType::Varchar, Nullability::Nullable),
        ];
        let desired = vec![column("id", DataType::BigInt, Nullability::NotNull)];
        assert_eq!(
            classify_schema_change(&desired, &current),
            SchemaChangeSafety::FullRebuildRequired
        );
    }

    #[test]
    fn classifies_numeric_widening_as_review() {
        let current = vec![column("value", DataType::Integer, Nullability::Nullable)];
        let desired = vec![column("value", DataType::BigInt, Nullability::Nullable)];
        assert_eq!(
            classify_schema_change(&desired, &current),
            SchemaChangeSafety::Review
        );
    }

    #[test]
    fn classifies_incompatible_type_change_as_error() {
        let current = vec![column("value", DataType::BigInt, Nullability::Nullable)];
        let desired = vec![column("value", DataType::Varchar, Nullability::Nullable)];
        assert_eq!(
            classify_schema_change(&desired, &current),
            SchemaChangeSafety::Error
        );
    }
}
