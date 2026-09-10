//! The typed semantic layer.
//!
//! Phase 2 promotes compiled models to typed outputs with column-level
//! lineage. Types use an explicit `Unknown` state rather than fake defaults so
//! compiler limitations are visible.

use crate::identity::{ModelId, SourceId};

/// A SQL scalar or nested type as understood by the compiler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataType {
    Boolean,
    TinyInt,
    SmallInt,
    Integer,
    BigInt,
    Real,
    Double,
    Decimal,
    Varchar,
    Varbinary,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Array(Box<DataType>),
    Map(Box<DataType>, Box<DataType>),
    Row(Vec<DataType>),
    /// Type inference was not available.
    Unknown,
}

impl DataType {
    /// Parse a Trino type name. Nested types are recognised but kept coarse.
    pub fn parse_trino(value: &str) -> DataType {
        let normalized = value.trim().to_ascii_lowercase();
        let base = normalized.split('(').next().unwrap_or("").trim();
        match base {
            "boolean" => DataType::Boolean,
            "tinyint" => DataType::TinyInt,
            "smallint" => DataType::SmallInt,
            "integer" | "int" => DataType::Integer,
            "bigint" => DataType::BigInt,
            "real" => DataType::Real,
            "double" => DataType::Double,
            "decimal" | "numeric" => DataType::Decimal,
            "varchar" | "char" => DataType::Varchar,
            "varbinary" => DataType::Varbinary,
            "date" => DataType::Date,
            "time" | "time with time zone" => DataType::Time,
            "timestamp" => DataType::Timestamp,
            "timestamp with time zone" => DataType::TimestampTz,
            "array" => DataType::Array(Box::new(DataType::Unknown)),
            "map" => DataType::Map(Box::new(DataType::Unknown), Box::new(DataType::Unknown)),
            "row" => DataType::Row(Vec::new()),
            _ => DataType::Unknown,
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, DataType::Unknown)
    }

    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            DataType::TinyInt
                | DataType::SmallInt
                | DataType::Integer
                | DataType::BigInt
                | DataType::Real
                | DataType::Double
                | DataType::Decimal
        )
    }

    /// Widest of two numeric types, or `Unknown` if not comparable.
    pub fn widen(left: &DataType, right: &DataType) -> DataType {
        use DataType::*;
        if matches!(left, Unknown) || matches!(right, Unknown) {
            return Unknown;
        }
        if left == right {
            return left.clone();
        }
        let rank = |data_type: &DataType| match data_type {
            TinyInt => 1,
            SmallInt => 2,
            Integer => 3,
            BigInt => 4,
            Real => 5,
            Double => 6,
            Decimal => 7,
            _ => 0,
        };
        if left.is_numeric() && right.is_numeric() {
            if rank(left) >= rank(right) {
                left.clone()
            } else {
                right.clone()
            }
        } else {
            Unknown
        }
    }
}

impl std::fmt::Display for DataType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            DataType::Boolean => "BOOLEAN".to_string(),
            DataType::TinyInt => "TINYINT".to_string(),
            DataType::SmallInt => "SMALLINT".to_string(),
            DataType::Integer => "INTEGER".to_string(),
            DataType::BigInt => "BIGINT".to_string(),
            DataType::Real => "REAL".to_string(),
            DataType::Double => "DOUBLE".to_string(),
            DataType::Decimal => "DECIMAL".to_string(),
            DataType::Varchar => "VARCHAR".to_string(),
            DataType::Varbinary => "VARBINARY".to_string(),
            DataType::Date => "DATE".to_string(),
            DataType::Time => "TIME".to_string(),
            DataType::Timestamp => "TIMESTAMP".to_string(),
            DataType::TimestampTz => "TIMESTAMP WITH TIME ZONE".to_string(),
            DataType::Array(inner) => format!("ARRAY({inner})"),
            DataType::Map(key, value) => format!("MAP({key}, {value})"),
            DataType::Row(fields) => format!(
                "ROW({})",
                fields
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            DataType::Unknown => "UNKNOWN".to_string(),
        };
        formatter.write_str(&text)
    }
}

/// Nullability of a column. `Unknown` is distinct from `Nullable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nullability {
    NotNull,
    Nullable,
    Unknown,
}

impl Nullability {
    pub fn is_definitely_not_null(self) -> bool {
        matches!(self, Nullability::NotNull)
    }

    /// Combine two nullabilities across a set operation or union.
    pub fn merge(self, other: Nullability) -> Nullability {
        match (self, other) {
            (Nullability::NotNull, Nullability::NotNull) => Nullability::NotNull,
            (Nullability::Unknown, _) | (_, Nullability::Unknown) => Nullability::Unknown,
            _ => Nullability::Nullable,
        }
    }
}

impl std::fmt::Display for Nullability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Nullability::NotNull => "not null",
            Nullability::Nullable => "nullable",
            Nullability::Unknown => "unknown",
        })
    }
}

/// Identifies a relation that contributes to lineage: a workspace model or an
/// external source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelationRef {
    Model(ModelId),
    Source(SourceId),
}

impl RelationRef {
    pub fn display(&self) -> String {
        match self {
            RelationRef::Model(id) => id.logical_name(),
            RelationRef::Source(id) => id.logical_name(),
        }
    }
}

/// Identifies a specific column of a relation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnRef {
    pub relation: RelationRef,
    pub column: String,
}

impl ColumnRef {
    pub fn model(model: ModelId, column: impl Into<String>) -> Self {
        Self {
            relation: RelationRef::Model(model),
            column: column.into(),
        }
    }

    pub fn source(source: SourceId, column: impl Into<String>) -> Self {
        Self {
            relation: RelationRef::Source(source),
            column: column.into(),
        }
    }

    pub fn display(&self) -> String {
        format!("{}.{}", self.relation.display(), self.column)
    }
}

/// A column produced by a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputColumn {
    pub name: String,
    pub data_type: DataType,
    pub nullability: Nullability,
    /// Direct inputs of this column, used to derive lineage.
    pub inputs: Vec<ColumnRef>,
}

/// The output schema of a model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelSchema {
    pub columns: Vec<OutputColumn>,
    /// True when every input schema was known, so column references were fully
    /// validated. False means unknown references were tolerated.
    pub known: bool,
}

impl ModelSchema {
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn column(&self, name: &str) -> Option<&OutputColumn> {
        self.columns.iter().find(|column| column.name == name)
    }
}

/// A logical data-quality assertion derived from directives or contracts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assertion {
    NotNull { column: String },
    Unique { columns: Vec<String> },
}

impl Assertion {
    pub fn describe(&self) -> String {
        match self {
            Assertion::NotNull { column } => format!("not_null {column}"),
            Assertion::Unique { columns } => format!("unique {}", columns.join(", ")),
        }
    }
}
