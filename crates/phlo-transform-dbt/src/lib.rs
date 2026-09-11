//! dbt-to-Phlo migration frontend.
//!
//! A one-way translator: it analyses a dbt project (`dbt_project.yml`, models,
//! property YAML, tests, macros, seeds, snapshots) and emits the smallest
//! equivalent native Phlo Transform workspace — plain `.sql` models with
//! `-- @` directives, `phlo.toml`/`transform.toml` config, `tests/` files —
//! plus a machine-readable migration manifest.
//!
//! Every dbt resource is classified `CLEAN`, `REVIEW` or `UNSUPPORTED`; the
//! translator never claims equivalence it cannot prove. All dbt-specific
//! semantics (Jinja, `ref()`, `source()`, `config()`, materialisations, dbt
//! YAML) live in this crate; the core compiler never sees them.

pub mod jinja;
mod macros;
pub mod project;
pub mod pylit;
pub mod report;
pub mod translate;

pub use project::{load, DbtProject, ProjectError};
pub use report::{
    codes, Classification, EmittedFile, MigrationIssue, MigrationManifest, MigrationReport,
    ResourceKind, ResourceOutcome,
};
pub use translate::{translate, write_translation, Translation, TRANSLATOR_VERSION};

/// Load and translate a dbt project in one step.
pub fn translate_project(root: &std::path::Path) -> Result<Translation, ProjectError> {
    let project = load(root)?;
    Ok(translate(&project))
}
