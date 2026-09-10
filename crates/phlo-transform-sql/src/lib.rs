//! SQL parsing, directive parsing and relation extraction for Phlo Transform.
//!
//! This crate is deliberately low-level: it knows how to turn SQL text into a
//! real AST (via `sqlparser-rs`), pull model header directives out of the raw
//! text, and extract the relation names referenced by a query. It has no
//! notion of workspaces, namespaces or model identity; those belong to
//! `phlo-transform-core`.
//!
//! Keeping this crate free of semantic concepts is what allows a future
//! importer (for example a dbt frontend) to reuse parsing without inheriting
//! the native Phlo filesystem frontend.

pub mod directives;
pub mod parse;
pub mod relations;

pub use directives::{
    parse_directives, DirectiveIssue, DirectiveIssueKind, Directives, Materialization,
};
pub use parse::{parse_statements, Dialect, SqlParseError};
pub use relations::{extract_relations, ExtractedRelation, RelationName};
