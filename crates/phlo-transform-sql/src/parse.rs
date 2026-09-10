//! Thin wrapper around `sqlparser-rs`.
//!
//! Phase 0 focuses on Trino-first SQL. `sqlparser` has no dedicated Trino
//! dialect, so the permissive `GenericDialect` is used for Trino/native SQL
//! while DuckDB and PostgreSQL can opt into their dialects.

use sqlparser::ast::Statement;
use sqlparser::dialect::{
    Dialect as SqlparserDialect, DuckDbDialect, GenericDialect, PostgreSqlDialect,
};
use sqlparser::parser::Parser;

/// SQL dialect used when parsing a model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dialect {
    /// Permissive ANSI dialect, the default and the closest fit for Trino.
    #[default]
    Generic,
    /// Alias for [`Dialect::Generic`] kept so callers can be explicit about
    /// Trino-first intent.
    Trino,
    DuckDb,
    PostgreSql,
}

impl Dialect {
    fn to_sqlparser(self) -> Box<dyn SqlparserDialect> {
        match self {
            Dialect::Generic | Dialect::Trino => Box::new(GenericDialect {}),
            Dialect::DuckDb => Box::new(DuckDbDialect {}),
            Dialect::PostgreSql => Box::new(PostgreSqlDialect {}),
        }
    }
}

/// A SQL parse failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SqlParseError {
    message: String,
}

impl SqlParseError {
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Parse a SQL string into zero or more statements.
pub fn parse_statements(sql: &str, dialect: Dialect) -> Result<Vec<Statement>, SqlParseError> {
    Parser::parse_sql(dialect.to_sqlparser().as_ref(), sql).map_err(|error| SqlParseError {
        message: error.to_string(),
    })
}
