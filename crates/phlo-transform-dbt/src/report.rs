//! Migration report and manifest types.
//!
//! Every dbt resource is classified `CLEAN`, `REVIEW` or `UNSUPPORTED`. The
//! human report and the JSON report/manifest are derived from the same
//! structures.

use std::collections::BTreeMap;

use serde::Serialize;

/// Stable diagnostic codes for migration findings.
pub mod codes {
    /// `ref()` could not be resolved to a known model.
    pub const UNRESOLVED_REF: &str = "DBT001";
    /// `source()` could not be resolved to a declared source.
    pub const UNRESOLVED_SOURCE: &str = "DBT002";
    /// `var()` had no value in `vars:` and no default.
    pub const UNRESOLVED_VAR: &str = "DBT003";
    /// A macro or expression call with no native equivalent.
    pub const UNKNOWN_MACRO: &str = "DBT004";
    /// A `{% ... %}` construct the translator does not emulate.
    pub const JINJA_STATEMENT: &str = "DBT005";
    /// `is_incremental()` was used in a pattern with no native strategy.
    pub const INCREMENTAL_PATTERN: &str = "DBT006";
    /// A dbt config key has no Phlo equivalent and may matter.
    pub const UNTRANSLATED_CONFIG: &str = "DBT007";
    /// A materialisation with no direct native equivalent.
    pub const MATERIALIZATION: &str = "DBT008";
    /// Environment-dependent behaviour (`env_var`, `target`, `run_started_at`).
    pub const ENVIRONMENT_DEPENDENT: &str = "DBT009";
    /// A resource kind Phlo intentionally does not implement.
    pub const UNSUPPORTED_KIND: &str = "DBT010";
    /// The dbt resource is disabled (`enabled: false`).
    pub const DISABLED: &str = "DBT011";
    /// A cross-project or versioned `ref()`.
    pub const COMPLEX_REF: &str = "DBT012";
    /// Two resources map to the same emitted identity.
    pub const NAME_COLLISION: &str = "DBT013";
    /// A generic test with no native assertion and no safe SQL conversion.
    pub const UNSUPPORTED_TEST: &str = "DBT014";
    /// dbt seeds have no native static-data concept yet.
    pub const SEED: &str = "DBT015";
    /// A file could not be read or parsed.
    pub const LOAD_FAILURE: &str = "DBT016";
    /// A `ref()` target model was disabled or unsupported.
    pub const UNTRANSLATED_TARGET: &str = "DBT017";
}

/// How faithfully a resource was translated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Classification {
    Clean,
    Review,
    Unsupported,
}

impl Classification {
    pub fn label(self) -> &'static str {
        match self {
            Classification::Clean => "CLEAN",
            Classification::Review => "REVIEW",
            Classification::Unsupported => "UNSUPPORTED",
        }
    }
}

/// The dbt resource category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Model,
    Source,
    GenericTest,
    SingularTest,
    Seed,
    Snapshot,
    Macro,
    Package,
    Analysis,
    Exposure,
}

impl ResourceKind {
    pub fn label(self) -> &'static str {
        match self {
            ResourceKind::Model => "models",
            ResourceKind::Source => "sources",
            ResourceKind::GenericTest => "generic tests",
            ResourceKind::SingularTest => "singular tests",
            ResourceKind::Seed => "seeds",
            ResourceKind::Snapshot => "snapshots",
            ResourceKind::Macro => "macros",
            ResourceKind::Package => "packages",
            ResourceKind::Analysis => "analyses",
            ResourceKind::Exposure => "exposures",
        }
    }
}

/// One finding about a resource.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MigrationIssue {
    /// Stable code, e.g. `DBT004`.
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

impl MigrationIssue {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            suggestion: None,
        }
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }
}

/// The outcome for a single dbt resource.
#[derive(Clone, Debug, Serialize)]
pub struct ResourceOutcome {
    pub kind: ResourceKind,
    /// dbt resource name, e.g. `model.jaffle.customers` or `orders`.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub classification: Classification,
    /// Phlo-relative output path, when a file was emitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emitted_path: Option<String>,
    /// Human-readable descriptions of the semantic transformations applied.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transformations: Vec<String>,
    /// Non-blocking observations (dropped cosmetic config, metadata carried
    /// only in the manifest, ...).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Findings that made the resource REVIEW or UNSUPPORTED.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<MigrationIssue>,
    /// SHA-256 of the source file, for idempotence auditing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
}

/// A file the translation would write, relative to the output root.
#[derive(Clone, Debug)]
pub struct EmittedFile {
    pub rel_path: String,
    pub contents: String,
}

/// The full analysis report.
#[derive(Clone, Debug, Serialize)]
pub struct MigrationReport {
    pub translator_version: String,
    pub source_root: String,
    pub project_name: String,
    /// `(kind, classification)` → count.
    pub summary: BTreeMap<String, BTreeMap<String, usize>>,
    /// Model conversion coverage as a fraction 0..1.
    pub model_coverage: f64,
    /// Files that could not be read or parsed while loading the project.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub load_warnings: Vec<String>,
    pub resources: Vec<ResourceOutcome>,
}

impl MigrationReport {
    /// The count for one `(kind, classification)` pair.
    pub fn count(&self, kind: ResourceKind, class: Classification) -> usize {
        self.summary
            .get(kind.label())
            .and_then(|classes| classes.get(class.label()))
            .copied()
            .unwrap_or(0)
    }

    /// Distinct `(code, message)` pairs with occurrence counts.
    pub fn reason_summary(&self) -> BTreeMap<String, usize> {
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        for resource in &self.resources {
            for issue in &resource.issues {
                *reasons
                    .entry(format!("{}: {}", issue.code, issue.message))
                    .or_default() += 1;
            }
        }
        reasons
    }

    /// Render the human-readable analysis (matching `docs/roadmap/dbt-migration.md`).
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str("dbt migration analysis\n\n");
        out.push_str(&format!(
            "Project: {} ({})\n\n",
            self.project_name, self.source_root
        ));
        out.push_str("Resources\n");
        let mut kinds: Vec<ResourceKind> = self
            .summary
            .keys()
            .filter_map(|key| {
                [
                    ResourceKind::Model,
                    ResourceKind::Source,
                    ResourceKind::GenericTest,
                    ResourceKind::SingularTest,
                    ResourceKind::Seed,
                    ResourceKind::Snapshot,
                    ResourceKind::Macro,
                    ResourceKind::Package,
                    ResourceKind::Analysis,
                    ResourceKind::Exposure,
                ]
                .into_iter()
                .find(|kind| kind.label() == key)
            })
            .collect();
        kinds.sort();
        kinds.dedup();
        for kind in &kinds {
            let total: usize = self
                .summary
                .get(kind.label())
                .map(|classes| classes.values().sum())
                .unwrap_or(0);
            out.push_str(&format!("  {:<18} {:>4}\n", kind.label(), total));
        }
        for class in [
            Classification::Clean,
            Classification::Review,
            Classification::Unsupported,
        ] {
            let mut lines = Vec::new();
            for kind in &kinds {
                let count = self.count(*kind, class);
                if count > 0 {
                    lines.push(format!("  {:<18} {:>4}", kind.label(), count));
                }
            }
            if !lines.is_empty() {
                out.push_str(&format!("\n{}\n", class.label()));
                for line in lines {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }
        let model_count: usize = self
            .summary
            .get("models")
            .map(|classes| classes.values().sum())
            .unwrap_or(0);
        if model_count == 0 {
            out.push_str("\nModel conversion coverage: n/a (no models)\n");
        } else {
            out.push_str(&format!(
                "\nModel conversion coverage: {:.1}%\n",
                self.model_coverage * 100.0
            ));
        }
        let reasons = self.reason_summary();
        if !reasons.is_empty() {
            out.push_str("\nReview reasons\n");
            for (reason, count) in &reasons {
                out.push_str(&format!("  {count:>3} {reason}\n"));
            }
        }
        out
    }
}

/// The machine-readable migration manifest written to
/// `.phlo/migration/dbt-translation.json`.
#[derive(Clone, Debug, Serialize)]
pub struct MigrationManifest {
    pub translator_version: String,
    pub project_name: String,
    pub source_root: String,
    /// SHA-256 over the concatenated source hashes; reruns are comparable.
    pub source_fingerprint: String,
    pub entries: Vec<ResourceOutcome>,
}
