//! Typed, machine-readable diagnostics.
//!
//! Every diagnostic carries a stable code so agents and CI can key off it
//! without parsing prose. The code prefixes follow the categories in
//! `SPEC.md` §90 (`PROJECT`, `CONFIG`, `PARSE`, `RESOLUTION`, `GRAPH`, ...).

use serde::Serialize;

/// Stable diagnostic codes. Treat these as public API.
pub mod codes {
    pub const PROJECT_WORKSPACE_NOT_FOUND: &str = "PROJECT001";
    pub const PROJECT_DUPLICATE_MODEL: &str = "PROJECT002";
    pub const PROJECT_DUPLICATE_NAMESPACE: &str = "PROJECT003";
    pub const PROJECT_INVALID_MODEL_ID: &str = "PROJECT004";
    pub const PROJECT_UNRESOLVABLE_PATH: &str = "PROJECT005";
    pub const PROJECT_FILE_READ: &str = "PROJECT006";
    pub const PROJECT_TARGET_COLLISION: &str = "PROJECT007";
    pub const PROJECT_SEED_NAME_COLLISION: &str = "PROJECT008";
    pub const PROJECT_NO_ROOTS: &str = "PROJECT009";

    pub const CONFIG_INVALID: &str = "CONFIG001";
    pub const CONFIG_INVALID_ROOT: &str = "CONFIG002";

    pub const PARSE_INVALID_SQL: &str = "PARSE001";
    pub const PARSE_MALFORMED_DIRECTIVE: &str = "PARSE002";
    pub const PARSE_UNKNOWN_DIRECTIVE: &str = "PARSE003";

    pub const RESOLUTION_AMBIGUOUS: &str = "RESOLUTION001";

    pub const DEPENDENCIES_CROSS_WORKFLOW: &str = "DEPENDENCIES001";

    pub const TYPE_UNKNOWN_COLUMN: &str = "TYPE001";
    pub const TYPE_AMBIGUOUS_COLUMN: &str = "TYPE002";
    pub const TYPE_DUPLICATE_COLUMN: &str = "TYPE003";
    pub const TYPE_INCOMPATIBLE_UNION: &str = "TYPE004";
    pub const TYPE_CONTRACT: &str = "TYPE005";

    pub const GRAPH_CYCLE: &str = "GRAPH001";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn is_error(self) -> bool {
        matches!(self, Severity::Error)
    }

    /// Lower-case label used in human output.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// A single typed diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, message)
    }

    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Info, code, message)
    }

    fn new(severity: Severity, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            severity,
            message: message.into(),
            path: None,
            line: None,
            column: None,
            labels: Vec::new(),
            help: None,
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Attach a 1-based source position for `path`.
    pub fn with_location(mut self, line: usize, column: usize) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    pub fn with_labels<I, S>(mut self, labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.labels = labels.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Render in the compact form used by the CLI:
    ///
    /// ```text
    /// error[GRAPH001]: transformation cycle detected
    ///   assay.a -> assay.b -> assay.a
    /// ```
    pub fn render_human(&self) -> String {
        let mut output = format!("{}[{}]: {}", self.severity.label(), self.code, self.message);
        for label in &self.labels {
            output.push_str("\n  ");
            output.push_str(label);
        }
        if let Some(help) = &self.help {
            output.push_str("\n  help: ");
            output.push_str(help);
        }
        output
    }
}
