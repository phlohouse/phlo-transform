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
    /// Parse a Trino type name, including nested `array`, `map` and `row`.
    pub fn parse_trino(value: &str) -> DataType {
        let value = value.trim();
        let lower = value.to_ascii_lowercase();

        if lower.starts_with("array") {
            return match type_arguments(value, "array").and_then(|args| args.first().cloned()) {
                Some(inner) => DataType::Array(Box::new(DataType::parse_trino(&inner))),
                None => DataType::Array(Box::new(DataType::Unknown)),
            };
        }
        if lower.starts_with("map") {
            return match type_arguments(value, "map") {
                Some(args) if args.len() == 2 => DataType::Map(
                    Box::new(DataType::parse_trino(&args[0])),
                    Box::new(DataType::parse_trino(&args[1])),
                ),
                _ => DataType::Map(Box::new(DataType::Unknown), Box::new(DataType::Unknown)),
            };
        }
        if lower.starts_with("row") {
            let fields = type_arguments(value, "row")
                .map(|fields| {
                    fields
                        .iter()
                        .map(|field| {
                            let field_type = field
                                .split_once(' ')
                                .map(|(_, field_type)| field_type)
                                .unwrap_or(field.as_str());
                            DataType::parse_trino(field_type)
                        })
                        .collect()
                })
                .unwrap_or_default();
            return DataType::Row(fields);
        }

        let base = lower.split(['(', '<']).next().unwrap_or("").trim();
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
            "time" => DataType::Time,
            "timestamp" => {
                if lower.contains("with time zone") {
                    DataType::TimestampTz
                } else {
                    DataType::Timestamp
                }
            }
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

impl serde::Serialize for ColumnRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.display())
    }
}

/// Whether an input column contributes its values to an output directly or
/// only influences which rows and values appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Directness {
    /// The input's values flow into the output column.
    Direct,
    /// The input influences the output without flowing into it — join
    /// constraints, filters, grouping keys, sort keys and the like.
    Indirect,
}

/// How an input column is transformed on its way into an output column.
///
/// The variants follow the OpenLineage column-lineage subtypes so the export
/// boundary can map them one-to-one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transformation {
    /// The value passes through unchanged (bare column or alias).
    Identity,
    /// The input participates in a value-producing expression.
    Transformation,
    /// The input is aggregated.
    Aggregation,
    /// The input participates in a join constraint.
    Join,
    /// The input participates in a row filter (WHERE, HAVING, QUALIFY).
    Filter,
    /// The input is a grouping key.
    GroupBy,
    /// The input is a sort key.
    Sort,
    /// The input feeds a window function.
    Window,
    /// The input participates in conditional logic (CASE, `if`, `coalesce`).
    Conditional,
}

/// Confidence in a lineage claim.
///
/// `Exact` means the SQL AST proved the link. `Inferred` is for name-based or
/// heuristic links, `Declared` for user-asserted lineage, `Runtime` for links
/// observed during execution and `Unknown` for lineage the compiler could not
/// prove. Only `Exact` is produced today; the other variants exist so future
/// producers do not have to smuggle weaker claims in as exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageConfidence {
    Exact,
    Inferred,
    Declared,
    Runtime,
    Unknown,
}

/// One upstream column an output column draws from, with provenance.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ColumnInput {
    /// The upstream column.
    pub column: ColumnRef,
    /// Whether the input's values flow into the output.
    pub directness: Directness,
    /// How the input is transformed.
    pub transformation: Transformation,
    /// Provenance of this link. AST-proven links are
    /// [`LineageConfidence::Exact`].
    pub confidence: LineageConfidence,
    /// The SQL expression producing this link, when recorded.
    pub expression: Option<String>,
}

impl ColumnInput {
    /// The leaf link created when a column resolves to a relation column.
    pub fn identity(column: ColumnRef) -> Self {
        Self {
            column,
            directness: Directness::Direct,
            transformation: Transformation::Identity,
            confidence: LineageConfidence::Exact,
            expression: None,
        }
    }
}

/// A column produced by a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputColumn {
    pub name: String,
    pub data_type: DataType,
    pub nullability: Nullability,
    /// Inputs of this column, with how each contributes.
    pub inputs: Vec<ColumnInput>,
    /// Confidence that `inputs` is complete: `Exact` when every part of the
    /// producing expression was analysed, `Unknown` when part of it could not
    /// be reasoned about (the recorded inputs may then be incomplete).
    pub confidence: LineageConfidence,
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

/// An explicit column-level contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnContract {
    pub name: String,
    pub data_type: Option<DataType>,
    pub nullable: Option<bool>,
}

/// An explicit model contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelContract {
    pub enforced: bool,
    pub columns: Vec<ColumnContract>,
}

/// Numeric tolerance for a diffed column.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColumnTolerance {
    pub absolute: Option<f64>,
    pub relative: Option<f64>,
}

/// Declarative data-diff policy and tolerances for a model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiffPolicySpec {
    pub max_added_rows: Option<i64>,
    pub max_removed_rows: Option<i64>,
    pub max_modified_rows: Option<i64>,
    pub max_changed_fraction: Option<f64>,
    pub require_full_diff: bool,
    pub require_keyed_diff: bool,
    pub tolerances: std::collections::BTreeMap<String, ColumnTolerance>,
}

/// Extract the argument list of a parameterised type such as `array(bigint)`
/// or `row(a bigint, b varchar)`.
fn type_arguments(value: &str, keyword: &str) -> Option<Vec<String>> {
    let rest = value.to_ascii_lowercase();
    let rest = rest.strip_prefix(keyword)?.trim_start();
    let (open, close) = match rest.chars().next()? {
        '(' => ('(', ')'),
        '<' => ('<', '>'),
        _ => return None,
    };
    let inner = rest.strip_prefix(open)?.strip_suffix(close)?;
    Some(split_top_level(inner))
}

/// Split a comma-separated type argument list, ignoring commas nested inside
/// parentheses, angle brackets or brackets.
fn split_top_level(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in value.chars() {
        match character {
            '(' | '<' | '[' => {
                depth += 1;
                current.push(character);
            }
            ')' | '>' | ']' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}
