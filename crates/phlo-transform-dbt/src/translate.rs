//! The translation pipeline.
//!
//! Loads the dbt project model, lowers each resource to the smallest native
//! Phlo representation, and produces emitted files plus a report/manifest.
//! Translation is deterministic: identical input produces identical output.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};

use crate::jinja::{self, Call, Segment};
use crate::project::DbtProject;
use crate::pylit::Lit;
use crate::report::{
    codes, Classification, EmittedFile, MigrationIssue, MigrationManifest, MigrationReport,
    ResourceKind, ResourceOutcome,
};

/// Translator version recorded in manifests.
pub const TRANSLATOR_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The output of translating a dbt project.
pub struct Translation {
    pub report: MigrationReport,
    pub manifest: MigrationManifest,
    /// Files to write under the chosen output root (already includes the
    /// manifest path `.phlo/migration/dbt-translation.json` is added by
    /// [`write_translation`]).
    pub files: Vec<EmittedFile>,
}

/// dbt expression names whose value depends on the run environment and can
/// never be made deterministic.
const ENVIRONMENT_EXPRS: &[&str] = &[
    "env_var",
    "run_started_at",
    "invocation_id",
    "dbt_version",
    "flags",
    "execute",
    "statement",
    "load_result",
    "store_result",
    "store_raw_result",
    "print",
    "log",
    "modules.datetime",
    "modules.pytz",
    "modules.time",
    "modules.re",
    "modules.itertools",
];

/// dbt config keys that carry semantics Phlo cannot express; their presence
/// forces REVIEW.
const REVIEW_CONFIG_KEYS: &[&str] = &[
    "pre-hook",
    "post-hook",
    "pre_hook",
    "post_hook",
    "grants",
    "database",
];

/// dbt config keys that are safe to drop: cosmetic or runtime-only.
const IGNORED_CONFIG_KEYS: &[&str] = &[
    "persist_docs",
    "docs",
    "labels",
    "group",
    "access",
    "event_time",
    "concurrent_batches",
    "batch_id",
    "lookback",
    "meta_fields",
    "quoting",
    "bind",
    "sql_header",
    "fail_calc",
    "severity",
    "error_if",
    "warn_if",
    "limit",
    "where",
    "store_failures",
    "store_failures_as",
    "full_refresh",
    "on_configuration_change",
];

/// Translate a loaded dbt project into a native Phlo workspace.
pub fn translate(project: &DbtProject) -> Translation {
    let mut ctx = Context::new(project);

    // Pass 1: register every model name so `ref()` can resolve before
    // per-model translation runs.
    let mut model_targets: BTreeMap<String, String> = BTreeMap::new();
    let mut collisions: Vec<String> = Vec::new();
    for model in &project.models {
        let segments = emitted_segments(&model.dir, &model.stem);
        let logical = logical_name(&segments, &project.name);
        if let Some(_existing) = model_targets.get(&model.stem) {
            collisions.push(model.stem.clone());
        } else {
            model_targets.insert(model.stem.clone(), logical);
        }
    }
    for name in &collisions {
        model_targets.remove(name);
    }

    ctx.model_targets = model_targets.clone();
    ctx.collided_names = collisions;
    ctx.disabled_models = disabled_models(project);
    ctx.sources = collect_sources(project);
    ctx.model_properties = collect_model_properties(project);

    let mut files: Vec<EmittedFile> = Vec::new();
    let mut outcomes: Vec<ResourceOutcome> = Vec::new();

    // Project-level `models:` configuration (workspace defaults).
    let project_model_config = dir_config(&project.models_tree, &project.name, &[]);
    let mut phlo_toml = PhloToml::default();
    if let Some(materialized) = config_str(&project_model_config, "materialized") {
        if materialized != "view" {
            phlo_toml.default_materialization = Some(materialized.clone());
        }
        ctx.default_materialization = materialized;
    }
    if let Some(schema) = config_str(&project_model_config, "schema") {
        phlo_toml.default_schema = Some(schema);
    }
    if let Some(database) = config_str(&project_model_config, "database") {
        phlo_toml.default_catalog = Some(database);
    }
    if let Some(profile) = &project.profile {
        if phlo_toml.default_schema.is_none() {
            phlo_toml.default_schema = profile.schema.clone();
        }
        if phlo_toml.default_catalog.is_none() {
            phlo_toml.default_catalog =
                profile.catalog.clone().or_else(|| profile.database.clone());
        }
    }

    // Whether any model lands directly under `transforms/` (needing a
    // `default_namespace`).
    let has_top_level_models = project.models.iter().any(|model| model.dir.is_empty());
    if has_top_level_models {
        phlo_toml.default_namespace = Some(sanitize_segment(&project.name));
    }

    // Per-namespace and per-folder transform.toml fragments.
    let mut root_tomls: BTreeMap<PathBuf, RootToml> = BTreeMap::new();

    for model in &project.models {
        let outcome = translate_model(model, &mut ctx, &project_model_config, &mut root_tomls);
        if let Some((path, contents)) = outcome_file(&outcome) {
            files.push(EmittedFile {
                rel_path: path,
                contents,
            });
        }
        outcomes.push(outcome.0);
    }

    // Property-only resources: sources, seeds, snapshots, exposures, ...
    collect_property_resources(project, &mut outcomes);

    // Declared sources resolve to physical relations — CLEAN, with metadata
    // carried in the manifest only (Phlo infers sources from SQL).
    let source_names: Vec<(String, String)> = ctx.sources.keys().cloned().collect();
    for (source, table) in source_names {
        let info = ctx.sources[&(source.clone(), table.clone())].clone();
        let mut notes = Vec::new();
        if let Some(description) = &info.description {
            notes.push(format!("description: {description}"));
        }
        let mut outcome = ResourceOutcome {
            kind: ResourceKind::Source,
            name: format!("{source}.{table}"),
            source_path: None,
            classification: Classification::Clean,
            emitted_path: None,
            transformations: vec![format!("resolved to {}", info.relation)],
            notes,
            issues: Vec::new(),
            source_hash: None,
        };
        emit_source_tests(&info, &source, &table, &mut ctx, &mut outcome);
        outcomes.push(outcome);
    }

    // Singular tests.
    for test in &project.singular_tests {
        let outcome = translate_singular_test(test, &ctx);
        if let Some((path, contents)) = outcome_file(&outcome) {
            files.push(EmittedFile {
                rel_path: path,
                contents,
            });
        }
        outcomes.push(outcome.0);
    }

    // Seeds are copied so teams can decide how to host them.
    for seed in &project.seeds {
        let rel = display(seed);
        outcomes.push(ResourceOutcome {
            kind: ResourceKind::Seed,
            name: seed
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string(),
            source_path: Some(rel.clone()),
            classification: Classification::Review,
            emitted_path: None,
            transformations: Vec::new(),
            notes: vec![
                "Phlo has no native seed concept yet; load the CSV as a source or keep it under version control".to_string(),
            ],
            issues: vec![MigrationIssue::new(
                codes::SEED,
                "dbt seed has no native Phlo representation",
            )],
            source_hash: file_hash(&project.root.join(seed)),
        });
    }

    for snapshot in &project.snapshots {
        outcomes.push(ResourceOutcome {
            kind: ResourceKind::Snapshot,
            name: snapshot.stem.clone(),
            source_path: Some(display(&snapshot.rel_path)),
            classification: Classification::Unsupported,
            emitted_path: None,
            transformations: Vec::new(),
            notes: Vec::new(),
            issues: vec![MigrationIssue::new(
                codes::UNSUPPORTED_KIND,
                "dbt snapshots have no Phlo equivalent",
            )],
            source_hash: Some(hash_text(&snapshot.sql)),
        });
    }

    for analysis in &project.analyses {
        outcomes.push(ResourceOutcome {
            kind: ResourceKind::Analysis,
            name: analysis.stem.clone(),
            source_path: Some(display(&analysis.rel_path)),
            classification: Classification::Unsupported,
            emitted_path: None,
            transformations: Vec::new(),
            notes: vec!["analyses are not part of the transform graph".to_string()],
            issues: vec![MigrationIssue::new(
                codes::UNSUPPORTED_KIND,
                "dbt analysis files are not translated",
            )],
            source_hash: Some(hash_text(&analysis.sql)),
        });
    }

    for (name, file) in &ctx.macro_files_classified {
        outcomes.push(ResourceOutcome {
            kind: ResourceKind::Macro,
            name: name.clone(),
            source_path: Some(display(file)),
            classification: if ctx.macro_unsupported.contains(name) {
                Classification::Unsupported
            } else {
                Classification::Review
            },
            emitted_path: None,
            transformations: Vec::new(),
            notes: Vec::new(),
            issues: vec![MigrationIssue::new(
                codes::UNKNOWN_MACRO,
                "macros are not translated; model usages are classified individually",
            )],
            source_hash: None,
        });
    }

    for package in &project.packages {
        outcomes.push(ResourceOutcome {
            kind: ResourceKind::Package,
            name: package.clone(),
            source_path: None,
            classification: Classification::Review,
            emitted_path: None,
            transformations: Vec::new(),
            notes: vec!["package macros used by models are classified per call site".to_string()],
            issues: vec![MigrationIssue::new(
                codes::UNKNOWN_MACRO,
                "package dependency is not carried over; usages are classified per call site",
            )],
            source_hash: None,
        });
    }

    // Emit transform.toml fragments for namespace/folder config.
    for (dir, toml) in root_tomls {
        let contents = toml.render();
        if !contents.is_empty() {
            files.push(EmittedFile {
                rel_path: display(&dir.join("transform.toml")),
                contents,
            });
        }
    }

    phlo_toml.model_sections = std::mem::take(&mut ctx.contract_sections);

    // Emit phlo.toml when it carries real configuration.
    let phlo = phlo_toml.render();
    if !phlo.is_empty() {
        files.push(EmittedFile {
            rel_path: "phlo.toml".to_string(),
            contents: phlo,
        });
    }

    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    outcomes
        .sort_by(|a, b| (a.kind, &a.name, &a.source_path).cmp(&(b.kind, &b.name, &b.source_path)));

    // Generated tests were queued during model/source translation.
    files.append(&mut ctx.generated_tests);
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    let report = build_report(project, &outcomes);
    let manifest = MigrationManifest {
        translator_version: TRANSLATOR_VERSION.to_string(),
        project_name: project.name.clone(),
        source_root: display(&project.root),
        source_fingerprint: fingerprint_outcomes(&outcomes),
        entries: outcomes,
    };

    Translation {
        report,
        manifest,
        files,
    }
}

/// Per-model outcome plus the file it produced, if any.
type ModelOutcome = (ResourceOutcome, Option<(String, String)>);

fn outcome_file(outcome: &ModelOutcome) -> Option<(String, String)> {
    outcome.1.clone()
}

/// Shared translation context.
struct Context<'a> {
    project: &'a DbtProject,
    /// dbt model name → emitted logical name (`staging.users`).
    model_targets: BTreeMap<String, String>,
    /// Names of models disabled via `enabled: false`.
    disabled_models: Vec<String>,
    /// `(source_name, table_name)` → physical relation.
    sources: BTreeMap<(String, String), SourceInfo>,
    /// dbt model name → its `models:` property entry.
    model_properties: BTreeMap<String, Value>,
    /// Queued generated test files.
    generated_tests: Vec<EmittedFile>,
    /// Workspace default materialization (view unless configured).
    default_materialization: String,
    /// Macro name → defining file; macro name → whether it is unsupported.
    macro_files_classified: BTreeMap<String, PathBuf>,
    macro_unsupported: Vec<String>,
    /// Contract sections appended to the emitted `phlo.toml`.
    contract_sections: Vec<String>,
    /// Model stems that collided on emitted identity.
    collided_names: Vec<String>,
}

#[derive(Clone)]
struct SourceInfo {
    relation: String,
    description: Option<String>,
    /// The raw `tables:` entry, for test extraction.
    table: Value,
}

impl<'a> Context<'a> {
    fn new(project: &'a DbtProject) -> Self {
        let mut macro_files_classified = BTreeMap::new();
        let mut macro_unsupported = Vec::new();
        for file in &project.macro_files {
            let unsupported = file.sql.contains("adapter.dispatch")
                || file.sql.contains("run_query")
                || file.sql.contains("{% materialization")
                || file.sql.contains("load_result")
                || file.sql.contains("store_result");
            for segment in jinja::scan(&file.sql) {
                if let Segment::Stmt { inner, .. } = &segment {
                    let rest = inner
                        .strip_prefix("macro ")
                        .or_else(|| inner.strip_prefix("materialization "));
                    if let Some(rest) = rest {
                        let name: String = rest
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
                            .collect();
                        if !name.is_empty() {
                            macro_files_classified.insert(name.clone(), file.rel_path.clone());
                            if unsupported {
                                macro_unsupported.push(name);
                            }
                        }
                    }
                }
            }
        }
        Self {
            project,
            model_targets: BTreeMap::new(),
            disabled_models: Vec::new(),
            sources: BTreeMap::new(),
            model_properties: BTreeMap::new(),
            generated_tests: Vec::new(),
            default_materialization: "view".to_string(),
            macro_files_classified,
            macro_unsupported,
            contract_sections: Vec::new(),
            collided_names: Vec::new(),
        }
    }

    fn is_project_macro(&self, name: &str) -> bool {
        self.macro_files_classified.contains_key(name)
    }

    fn package_of<'n>(&self, name: &'n str) -> Option<&'n str> {
        let package = name.split('.').next()?;
        self.project
            .packages
            .iter()
            .map(|dependency| {
                dependency
                    .rsplit('/')
                    .next()
                    .unwrap_or(dependency.as_str())
                    .replace('-', "_")
            })
            .any(|short| short == package)
            .then_some(package)
    }
}

/// Accumulates `phlo.toml` content.
#[derive(Default)]
struct PhloToml {
    default_namespace: Option<String>,
    default_materialization: Option<String>,
    default_catalog: Option<String>,
    default_schema: Option<String>,
    /// `model.<logical>.contract/columns` sections, appended verbatim.
    model_sections: Vec<String>,
}

impl PhloToml {
    fn render(&self) -> String {
        let mut out = String::new();
        let mut transform = Vec::new();
        if let Some(namespace) = &self.default_namespace {
            transform.push(format!("default_namespace = \"{namespace}\""));
        }
        if let Some(materialization) = &self.default_materialization {
            transform.push(format!("default_materialization = \"{materialization}\""));
        }
        if let Some(catalog) = &self.default_catalog {
            transform.push(format!("default_catalog = \"{catalog}\""));
        }
        if let Some(schema) = &self.default_schema {
            transform.push(format!("default_schema = \"{schema}\""));
        }
        if !transform.is_empty() {
            out.push_str("[transform]\n");
            for line in transform {
                out.push_str(&line);
                out.push('\n');
            }
        }
        for section in &self.model_sections {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(section);
        }
        out
    }
}

/// Accumulates `transform.toml` content for one namespace root.
#[derive(Default)]
struct RootToml {
    materialized: Option<String>,
    owner: Option<String>,
    tags: Vec<String>,
    /// folder key → `(key, value)` lines rendered as `[folder."key"]` tables.
    folders: BTreeMap<String, Vec<(String, String)>>,
}

impl RootToml {
    /// Set `key` inside `[folder."folder"]`, ignoring repeats from sibling
    /// models that share the same inherited config.
    fn set_folder(&mut self, folder: &str, key: &str, value: &str) {
        let settings = self.folders.entry(folder.to_string()).or_default();
        if !settings.iter().any(|(k, _)| k == key) {
            settings.push((key.to_string(), value.to_string()));
        }
    }
}

impl RootToml {
    fn is_empty(&self) -> bool {
        self.materialized.is_none()
            && self.owner.is_none()
            && self.tags.is_empty()
            && self.folders.is_empty()
    }

    fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        if let Some(materialized) = &self.materialized {
            out.push_str(&format!("materialized = \"{materialized}\"\n"));
        }
        if let Some(owner) = &self.owner {
            out.push_str(&format!("owner = \"{owner}\"\n"));
        }
        if !self.tags.is_empty() {
            let tags = self
                .tags
                .iter()
                .map(|tag| format!("\"{tag}\""))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("tags = [{tags}]\n"));
        }
        for (folder, settings) in &self.folders {
            out.push_str(&format!("\n[folder.\"{folder}\"]\n"));
            for (key, value) in settings {
                out.push_str(&format!("{key} = {value}\n"));
            }
        }
        out
    }
}

/// Sanitize a path segment into a valid Phlo identifier segment.
fn sanitize_segment(segment: &str) -> String {
    let cleaned: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// The `transforms/`-relative segments for a model file.
fn emitted_segments(dir: &[String], stem: &str) -> Vec<String> {
    let mut segments: Vec<String> = dir.iter().map(|s| sanitize_segment(s)).collect();
    segments.push(sanitize_segment(stem));
    segments
}

/// The Phlo logical name for emitted segments (`transforms/<...>/<stem>.sql`).
fn logical_name(segments: &[String], project_name: &str) -> String {
    match segments {
        [] | [_] => {
            // `transforms/<stem>.sql` → `<default_namespace>.<stem>`.
            let name = segments.last().cloned().unwrap_or_default();
            format!("{}.{}", sanitize_segment(project_name), name)
        }
        _ => segments.join("."),
    }
}

/// The workspace-relative output path for emitted segments.
fn emitted_path(segments: &[String]) -> String {
    let mut path = String::from("transforms");
    for segment in segments {
        path.push('/');
        path.push_str(segment);
    }
    path.push_str(".sql");
    path
}

fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn hash_text(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn file_hash(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| hash_text_bytes(&bytes))
}

fn hash_text_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fingerprint_outcomes(outcomes: &[ResourceOutcome]) -> String {
    let mut hash = String::new();
    for outcome in outcomes {
        if let Some(source) = &outcome.source_hash {
            hash.push_str(source);
            hash.push('\n');
        }
    }
    format!("{:x}", Sha256::digest(hash.as_bytes()))
}

/// Result of lowering one model's SQL text.
#[derive(Default)]
struct Lowered {
    sql: String,
    issues: Vec<MigrationIssue>,
    notes: Vec<String>,
    transformations: Vec<String>,
    /// Merged `{{ config(...) }}` keyword arguments, in order.
    config: Vec<(String, Lit)>,
    /// Watermark column detected in an `is_incremental()` block.
    window_column: Option<String>,
    /// An `is_incremental()` body was dropped or reduced, meaning the result
    /// is a full-refresh rather than a filtered incremental scan.
    incremental_reduced: bool,
}

impl Lowered {
    fn review(&mut self, issue: MigrationIssue) {
        self.issues.push(issue);
    }
}

/// Translate a dbt SQL body (model or singular test) into native SQL.
fn lower_sql(sql: &str, ctx: &Context) -> Lowered {
    let mut lowered = Lowered::default();
    let segments = jinja::scan(sql);
    let mut out = String::new();
    lower_segments(&segments, ctx, &mut lowered, &mut out);
    lowered.sql = out;
    lowered
}

/// Lower a segment slice into `out`. Used for the top-level scan and
/// recursively for the else-branch of an `is_incremental()` conditional.
fn lower_segments(segments: &[Segment], ctx: &Context, lowered: &mut Lowered, out: &mut String) {
    let mut index = 0usize;
    while index < segments.len() {
        match &segments[index] {
            Segment::Text(text) => out.push_str(text),
            Segment::Comment(_) => {
                lowered
                    .transformations
                    .push("stripped Jinja comment".to_string());
            }
            Segment::Expr { raw, inner } => {
                // dbt convention: `'{{ var('x') }}'` — the expression sits
                // inside an already-quoted string, so emit the value unquoted.
                let quoted = out.trim_end().ends_with('\'')
                    && matches!(segments.get(index + 1), Some(Segment::Text(t)) if t.starts_with('\''));
                if let Some(call) = jinja::parse_call(inner) {
                    if call.name == "config" {
                        for (key, value) in &call.keyword {
                            lowered.config.push((key.clone(), value.clone()));
                        }
                        for positional in &call.positional {
                            if let Lit::Dict(items) = positional {
                                for (key, value) in items {
                                    lowered.config.push((key.clone(), value.clone()));
                                }
                            }
                        }
                        lowered
                            .transformations
                            .push("removed {{ config(...) }} block".to_string());
                    } else {
                        out.push_str(&translate_call(&call, raw, ctx, lowered, quoted));
                    }
                } else {
                    out.push_str(&translate_bare_expr(inner, raw, lowered));
                }
            }
            Segment::Stmt { raw, inner } => {
                // `translate_stmt` returns the last consumed index; the loop
                // still advances past it.
                index = translate_stmt(segments, index, inner, raw, ctx, lowered, out);
            }
        }
        index += 1;
    }
}

/// Handle a `{% ... %}` segment; returns the index of the last segment
/// consumed (the `if` handler may skip to its `endif`).
fn translate_stmt(
    segments: &[Segment],
    index: usize,
    inner: &str,
    raw: &str,
    ctx: &Context,
    lowered: &mut Lowered,
    out: &mut String,
) -> usize {
    let keyword = jinja::stmt_keyword(inner);
    match keyword {
        "if" if jinja::is_incremental_condition(inner) => match find_if_block(segments, index) {
            Some(block) => {
                let endif_index = block.endif_index;
                handle_incremental_block(segments, &block, ctx, lowered, out);
                endif_index
            }
            None => {
                lowered.review(MigrationIssue::new(
                    codes::JINJA_STATEMENT,
                    "unterminated `{% if is_incremental() %}` block",
                ));
                out.push_str(raw);
                index
            }
        },
        "if" | "elif" | "else" => {
            lowered.review(MigrationIssue::new(
                codes::JINJA_STATEMENT,
                format!("conditional `{{% {inner} %}}` cannot be evaluated statically"),
            ));
            out.push_str(raw);
            index
        }
        "set" | "for" | "do" | "call" | "filter" | "block" | "with" | "include" | "import"
        | "from" | "extends" | "raw" | "endraw" => {
            lowered.review(MigrationIssue::new(
                codes::JINJA_STATEMENT,
                format!("Jinja statement `{keyword}` has no native equivalent"),
            ));
            out.push_str(raw);
            index
        }
        "macro" | "materialization" | "test" | "docs" | "snapshot" | "comment" => {
            lowered.review(
                MigrationIssue::new(
                    codes::JINJA_STATEMENT,
                    format!("Jinja `{keyword}` block inside a model is not translatable"),
                )
                .with_suggestion("move the logic into ordinary SQL or a native directive"),
            );
            out.push_str(raw);
            index
        }
        _ => {
            lowered.review(MigrationIssue::new(
                codes::JINJA_STATEMENT,
                format!("unmatched Jinja statement `{keyword}`"),
            ));
            out.push_str(raw);
            index
        }
    }
}

/// The span of an `{% if %}` block: body and optional else body.
struct IfBlock {
    body_start: usize,
    /// Index of the `{% else %}`/`{% elif %}` at depth zero, if any.
    else_index: Option<usize>,
    /// Index of the matching `{% endif %}`.
    endif_index: usize,
}

fn find_if_block(segments: &[Segment], if_index: usize) -> Option<IfBlock> {
    let mut depth = 0usize;
    let mut else_index = None;
    for (i, segment) in segments.iter().enumerate().skip(if_index + 1) {
        if let Segment::Stmt { inner, .. } = segment {
            match jinja::stmt_keyword(inner) {
                "if" => depth += 1,
                "endif" if depth == 0 => {
                    return Some(IfBlock {
                        body_start: if_index + 1,
                        else_index,
                        endif_index: i,
                    });
                }
                "endif" => depth -= 1,
                "else" | "elif" if depth == 0 => else_index = else_index.or(Some(i)),
                _ => {}
            }
        }
    }
    None
}

/// Handle `{% if is_incremental() %} body [{% else %} alt] {% endif %}`.
fn handle_incremental_block(
    segments: &[Segment],
    block: &IfBlock,
    ctx: &Context,
    lowered: &mut Lowered,
    out: &mut String,
) {
    let body_end = block.else_index.unwrap_or(block.endif_index);
    let body = &segments[block.body_start..body_end];
    match block.else_index {
        Some(else_index) => {
            // The non-incremental branch is the full-refresh semantic.
            lowered.review(MigrationIssue::new(
                codes::INCREMENTAL_PATTERN,
                "is_incremental() else-branch kept; model degrades to full-refresh semantics",
            ));
            lowered.incremental_reduced = true;
            lower_segments(
                &segments[else_index + 1..block.endif_index],
                ctx,
                lowered,
                out,
            );
        }
        None => match watermark_column(body) {
            Some(column) => {
                lowered.transformations.push(format!(
                    "is_incremental() watermark became @incremental window={column}"
                ));
                lowered.window_column = Some(column);
            }
            None => {
                lowered.review(MigrationIssue::new(
                    codes::INCREMENTAL_PATTERN,
                    "unrecognised is_incremental() body dropped; model degrades to full-refresh semantics",
                ));
                lowered.incremental_reduced = true;
            }
        },
    }
}

/// Detect the common `col > (select max(col) from {{ this }})` watermark
/// pattern. Returns the watermark column when every Jinja expression in the
/// body is `this` and a `max(<col>)` call is present.
fn watermark_column(body: &[Segment]) -> Option<String> {
    let mut text = String::new();
    for segment in body {
        match segment {
            Segment::Text(part) => text.push_str(part),
            Segment::Expr { inner, .. } if inner.trim() == "this" => {}
            _ => return None,
        }
    }
    // `max(<column>)` — first call wins.
    let lower = text.to_ascii_lowercase();
    let start = lower.find("max(")? + 4;
    let rest = &text[start..];
    let column: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    if column.is_empty() {
        return None;
    }
    Some(column.rsplit('.').next().unwrap_or(&column).to_string())
}

/// Translate a `{{ name(...) }}` call. Returns the replacement SQL text; on
/// failure the raw text is preserved so the emitted file fails loudly.
fn translate_call(
    call: &Call,
    raw: &str,
    ctx: &Context,
    lowered: &mut Lowered,
    quoted: bool,
) -> String {
    match call.name.as_str() {
        "ref" => translate_ref(call, raw, ctx, lowered),
        "source" => translate_source(call, raw, ctx, lowered),
        "var" => translate_var(call, raw, ctx, lowered, quoted),
        name if ENVIRONMENT_EXPRS.contains(&name) || name.starts_with("modules.") => {
            lowered.review(MigrationIssue::new(
                codes::ENVIRONMENT_DEPENDENT,
                format!("`{{{{ {name}(...) }}}}` depends on the run environment"),
            ));
            raw.to_string()
        }
        name if name == "this" || name.starts_with("target.") || name.starts_with("adapter.") => {
            lowered.review(MigrationIssue::new(
                codes::ENVIRONMENT_DEPENDENT,
                format!("`{{{{ {name}(...) }}}}` is target-dependent"),
            ));
            raw.to_string()
        }
        name => {
            let detail = if let Some(package) = ctx.package_of(name) {
                format!("package macro `{name}` (from `{package}`)")
            } else if ctx.is_project_macro(name) {
                format!("project macro `{name}`")
            } else if name.contains('.') {
                format!("qualified macro `{name}` (package not declared)")
            } else {
                format!("unknown macro `{name}`")
            };
            lowered.review(
                MigrationIssue::new(
                    codes::UNKNOWN_MACRO,
                    format!("{detail} has no native equivalent"),
                )
                .with_suggestion("inline the SQL or reimplement it as a native directive"),
            );
            raw.to_string()
        }
    }
}

fn translate_ref(call: &Call, raw: &str, ctx: &Context, lowered: &mut Lowered) -> String {
    // Versioned refs (`ref('x', v = 2)`, `ref('x', version = 2)`).
    if call.kwarg("v").is_some() || call.kwarg("version").is_some() {
        lowered.review(MigrationIssue::new(
            codes::COMPLEX_REF,
            "versioned `ref()` has no native equivalent",
        ));
        return raw.to_string();
    }
    match call.positional.len() {
        1 => {
            let Some(name) = call.arg(0) else {
                lowered.review(MigrationIssue::new(
                    codes::UNRESOLVED_REF,
                    "non-literal `ref()` argument",
                ));
                return raw.to_string();
            };
            resolve_model(name, raw, ctx, lowered)
        }
        2 => {
            let (Some(package), Some(name)) = (call.arg(0), call.arg(1)) else {
                lowered.review(MigrationIssue::new(
                    codes::UNRESOLVED_REF,
                    "non-literal `ref()` arguments",
                ));
                return raw.to_string();
            };
            if package == ctx.project.name {
                resolve_model(name, raw, ctx, lowered)
            } else {
                lowered.review(MigrationIssue::new(
                    codes::COMPLEX_REF,
                    format!("cross-package `ref('{package}', '{name}')` is not resolved"),
                ));
                raw.to_string()
            }
        }
        _ => {
            lowered.review(MigrationIssue::new(
                codes::UNRESOLVED_REF,
                format!("unsupported `ref()` arity in {raw}"),
            ));
            raw.to_string()
        }
    }
}

fn resolve_model(name: &str, raw: &str, ctx: &Context, lowered: &mut Lowered) -> String {
    match ctx.model_targets.get(name) {
        Some(logical) if ctx.disabled_models.iter().any(|d| d == name) => {
            lowered.review(MigrationIssue::new(
                codes::UNTRANSLATED_TARGET,
                format!("`ref('{name}')` targets a disabled model"),
            ));
            let _ = logical;
            raw.to_string()
        }
        Some(logical) => {
            lowered
                .transformations
                .push(format!("ref('{name}') → {logical}"));
            logical.clone()
        }
        None => {
            lowered.review(MigrationIssue::new(
                codes::UNRESOLVED_REF,
                format!("`ref('{name}')` has no known target"),
            ));
            raw.to_string()
        }
    }
}

fn translate_source(call: &Call, raw: &str, ctx: &Context, lowered: &mut Lowered) -> String {
    let (Some(source), Some(table)) = (call.arg(0), call.arg(1)) else {
        lowered.review(MigrationIssue::new(
            codes::UNRESOLVED_SOURCE,
            format!("unsupported `source()` arguments in {raw}"),
        ));
        return raw.to_string();
    };
    match ctx.sources.get(&(source.to_string(), table.to_string())) {
        Some(info) => {
            lowered
                .transformations
                .push(format!("source('{source}', '{table}') → {}", info.relation));
            info.relation.clone()
        }
        None => {
            // dbt allows sources without declarations only in dashboards; a
            // `source()` call implies a declaration is expected.
            lowered.review(MigrationIssue::new(
                codes::UNRESOLVED_SOURCE,
                format!("`source('{source}', '{table}')` is not declared"),
            ));
            raw.to_string()
        }
    }
}

fn translate_var(
    call: &Call,
    raw: &str,
    ctx: &Context,
    lowered: &mut Lowered,
    quoted: bool,
) -> String {
    let Some(name) = call.arg(0) else {
        lowered.review(MigrationIssue::new(
            codes::UNRESOLVED_VAR,
            "non-literal `var()` name",
        ));
        return raw.to_string();
    };
    let value = ctx
        .project
        .vars
        .get(Value::String(name.to_string()))
        .map(yaml_to_lit)
        .or_else(|| call.positional.get(1).cloned());
    match value {
        Some(Lit::List(items)) => {
            let parts: Option<Vec<String>> = items.iter().map(Lit::to_sql_literal).collect();
            match parts {
                Some(parts) => {
                    lowered
                        .transformations
                        .push(format!("var('{name}') → {}", parts.join(", ")));
                    parts.join(", ")
                }
                None => {
                    lowered.review(MigrationIssue::new(
                        codes::UNRESOLVED_VAR,
                        format!("`var('{name}')` list contains non-literal items"),
                    ));
                    raw.to_string()
                }
            }
        }
        Some(lit) => match lit.to_sql_literal() {
            Some(literal) if quoted => {
                let unquoted = match &lit {
                    Lit::Str(s) => s.clone(),
                    _ => literal,
                };
                lowered
                    .transformations
                    .push(format!("var('{name}') → {unquoted}"));
                unquoted
            }
            Some(literal) => {
                lowered
                    .transformations
                    .push(format!("var('{name}') → {literal}"));
                literal
            }
            None => {
                lowered.review(MigrationIssue::new(
                    codes::UNRESOLVED_VAR,
                    format!("`var('{name}')` value is not a SQL literal"),
                ));
                raw.to_string()
            }
        },
        None => {
            lowered.review(MigrationIssue::new(
                codes::UNRESOLVED_VAR,
                format!("`var('{name}')` has no value and no default"),
            ));
            raw.to_string()
        }
    }
}

/// Non-call `{{ expr }}` — variables, `this`, attribute access, filters.
fn translate_bare_expr(inner: &str, raw: &str, lowered: &mut Lowered) -> String {
    let name = inner.trim();
    match name {
        "this" => {
            lowered.review(MigrationIssue::new(
                codes::UNKNOWN_MACRO,
                "`{{ this }}` self-reference has no native equivalent",
            ));
        }
        _ if ENVIRONMENT_EXPRS.contains(&name)
            || name.starts_with("target.")
            || name.starts_with("adapter.")
            || name.starts_with("dbt.")
            || name.starts_with("graph.")
            || name.starts_with("model.")
            || name.starts_with("exceptions.") =>
        {
            lowered.review(MigrationIssue::new(
                codes::ENVIRONMENT_DEPENDENT,
                format!("`{{{{ {name} }}}}` depends on dbt runtime state"),
            ));
        }
        _ => {
            lowered.review(MigrationIssue::new(
                codes::UNKNOWN_MACRO,
                format!("expression `{{{{ {name} }}}}` cannot be evaluated statically"),
            ));
        }
    }
    raw.to_string()
}

/// Convert a `serde_yaml` scalar/compound into a literal where possible.
fn yaml_to_lit(value: &Value) -> Lit {
    match value {
        Value::Null => Lit::None,
        Value::Bool(b) => Lit::Bool(*b),
        Value::Number(n) => n
            .as_i64()
            .map(Lit::Int)
            .or_else(|| n.as_f64().map(Lit::Float))
            .unwrap_or(Lit::None),
        Value::String(s) => Lit::Str(s.clone()),
        Value::Sequence(items) => Lit::List(items.iter().map(yaml_to_lit).collect()),
        Value::Mapping(map) => Lit::Dict(
            map.iter()
                .map(|(k, v)| (yaml_key(k), yaml_to_lit(v)))
                .collect(),
        ),
        Value::Tagged(tagged) => yaml_to_lit(&tagged.value),
    }
}

fn yaml_key(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// Effective `(key, Lit)` config for a model: project-tree dir config, then
/// property-file `config:`, then in-file `{{ config() }}` — later wins.
fn merged_model_config(
    dir: &[String],
    property_config: Option<&Value>,
    file_config: &[(String, Lit)],
    project: &DbtProject,
) -> BTreeMap<String, Lit> {
    let mut merged: BTreeMap<String, Lit> = BTreeMap::new();
    for (key, value) in dir_config(&project.models_tree, &project.name, dir) {
        merged.insert(key, yaml_to_lit(&value));
    }
    if let Some(config) = property_config
        .and_then(|p| p.get("config"))
        .and_then(Value::as_mapping)
    {
        for (key, value) in config {
            merged.insert(yaml_key(key), yaml_to_lit(value));
        }
    }
    for (key, value) in file_config {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

/// Collect `+key`/`key` config for `models.<project>.<dir...>`.
fn dir_config(tree: &Value, project: &str, dir: &[String]) -> BTreeMap<String, Value> {
    let mut config = BTreeMap::new();
    // `models: +k` applies to every project; `models.<name>: +k` applies to
    // the whole project; each subsequent segment descends one directory.
    collect_config_keys(tree, &mut config);
    let mut node = tree.get(project);
    if let Some(current) = node {
        collect_config_keys(current, &mut config);
    }
    for segment in dir {
        node = node.and_then(|n| n.get(segment.as_str()));
        match node {
            Some(current) => collect_config_keys(current, &mut config),
            None => break,
        }
    }
    config
}

/// Config keys are `+name` (modern) or bare known config names.
fn collect_config_keys(node: &Value, into: &mut BTreeMap<String, Value>) {
    const KNOWN: &[&str] = &[
        "materialized",
        "schema",
        "database",
        "alias",
        "tags",
        "meta",
        "enabled",
        "unique_key",
        "incremental_strategy",
        "partition_by",
        "on_schema_change",
        "grants",
        "contract",
        "pre-hook",
        "post-hook",
    ];
    let Some(map) = node.as_mapping() else {
        return;
    };
    for (key, value) in map {
        let Some(key) = key.as_str() else {
            continue;
        };
        if let Some(name) = key.strip_prefix('+') {
            into.insert(name.to_string(), value.clone());
        } else if KNOWN.contains(&key) {
            into.insert(key.to_string(), value.clone());
        }
    }
}

fn config_str(config: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    config.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Models disabled via `enabled: false` in any config layer.
fn disabled_models(project: &DbtProject) -> Vec<String> {
    let mut disabled = Vec::new();
    for file in project.property_files.iter() {
        let Some(models) = file.root.get("models").and_then(Value::as_sequence) else {
            continue;
        };
        for entry in models {
            let enabled = entry
                .get("config")
                .and_then(|c| c.get("enabled"))
                .or_else(|| entry.get("enabled"))
                .and_then(Value::as_bool);
            if enabled == Some(false) {
                if let Some(name) = entry.get("name").and_then(Value::as_str) {
                    disabled.push(name.to_string());
                }
            }
        }
    }
    disabled
}

/// `(source, table)` → resolved physical relation.
fn collect_sources(project: &DbtProject) -> BTreeMap<(String, String), SourceInfo> {
    let mut sources = BTreeMap::new();
    for file in &project.property_files {
        let Some(list) = file.root.get("sources").and_then(Value::as_sequence) else {
            continue;
        };
        for entry in list {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            let database = get_str(entry, "database");
            let default_schema = get_str(entry, "schema").unwrap_or(name);
            let tables = entry.get("tables").and_then(Value::as_sequence);
            for table in tables.into_iter().flatten() {
                let Some(table_name) = table.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let identifier = get_str(table, "identifier").unwrap_or(table_name);
                let schema = get_str(table, "schema").unwrap_or(default_schema);
                let database = get_str(table, "database").or(database);
                let relation = match database {
                    Some(database) => format!("{database}.{schema}.{identifier}"),
                    None => format!("{schema}.{identifier}"),
                };
                sources.insert(
                    (name.to_string(), table_name.to_string()),
                    SourceInfo {
                        relation,
                        description: get_str(table, "description").map(str::to_string),
                        table: table.clone(),
                    },
                );
            }
        }
    }
    sources
}

fn get_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// dbt model name → the `models:` property entry that describes it.
fn collect_model_properties(project: &DbtProject) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    for file in &project.property_files {
        let Some(models) = file.root.get("models").and_then(Value::as_sequence) else {
            continue;
        };
        for entry in models {
            if let Some(name) = entry.get("name").and_then(Value::as_str) {
                map.insert(name.to_string(), entry.clone());
            }
        }
    }
    map
}

/// Translate one dbt model file into a native Phlo model file.
fn translate_model(
    model: &crate::project::DbtSqlFile,
    ctx: &mut Context,
    project_model_config: &BTreeMap<String, Value>,
    root_tomls: &mut BTreeMap<PathBuf, RootToml>,
) -> ModelOutcome {
    let segments = emitted_segments(&model.dir, &model.stem);
    let logical = logical_name(&segments, &ctx.project.name);
    let rel_output = emitted_path(&segments);
    let display_path = display(&model.rel_path);

    let mut outcome = ResourceOutcome {
        kind: ResourceKind::Model,
        name: format!("model.{}.{}", ctx.project.name, model.stem),
        source_path: Some(display_path.clone()),
        classification: Classification::Clean,
        emitted_path: Some(rel_output.clone()),
        transformations: Vec::new(),
        notes: Vec::new(),
        issues: Vec::new(),
        source_hash: Some(hash_text(&model.sql)),
    };

    if ctx.disabled_models.iter().any(|name| name == &model.stem) {
        outcome.classification = Classification::Unsupported;
        outcome.emitted_path = None;
        outcome.issues.push(MigrationIssue::new(
            codes::DISABLED,
            "model is disabled (`enabled: false`); Phlo has no disabled state",
        ));
        return (outcome, None);
    }

    if ctx.collided_names.iter().any(|name| name == &model.stem) {
        outcome.issues.push(MigrationIssue::new(
            codes::NAME_COLLISION,
            format!(
                "model name `{}` maps to the same emitted identity as another model",
                model.stem
            ),
        ));
    }

    let lowered = lower_sql(&model.sql, ctx);
    outcome
        .transformations
        .extend(lowered.transformations.clone());
    outcome.notes.extend(lowered.notes.clone());
    outcome.issues.extend(lowered.issues.clone());

    // Merge config: dir → property file → in-file.
    let properties = ctx.model_properties.get(&model.stem).cloned();
    let merged = merged_model_config(
        &model.dir,
        properties.as_ref(),
        &lowered.config,
        ctx.project,
    );

    // `-- @id` pins logical identity so files can move after migration.
    let mut directives: Vec<String> = Vec::new();
    let mut header_notes: Vec<String> = Vec::new();

    // Materialisation.
    let materialized = merged
        .get("materialized")
        .and_then(Lit::as_str)
        .unwrap_or("view")
        .to_ascii_lowercase();

    // Where does the materialisation come from? If the dbt `models:` tree set
    // it for this directory, emit it once in the namespace transform.toml
    // instead of on every model.
    let dir_only_config = dir_config(&ctx.project.models_tree, &ctx.project.name, &model.dir);
    let dir_materialized = config_str(&dir_only_config, "materialized");
    let inherited_materialized = dir_materialized
        .clone()
        .or_else(|| config_str(project_model_config, "materialized"))
        .unwrap_or_else(|| ctx.default_materialization.clone());

    if !model.dir.is_empty() {
        let namespace = &segments[0];
        let toml = root_tomls
            .entry(PathBuf::from("transforms").join(namespace))
            .or_default();
        if let Some(m) = &dir_materialized {
            if model.dir.len() == 1 {
                if toml.materialized.is_none() {
                    toml.materialized = Some(m.clone());
                }
            } else {
                let folder_key = model.dir.join("/");
                toml.set_folder(&folder_key, "materialized", &format!("\"{m}\""));
            }
        }
        // Directory-level tags from the models tree.
        if let Some(tags) = dir_only_config.get("tags").and_then(yaml_str_list) {
            if model.dir.len() == 1 {
                for tag in tags {
                    if !toml.tags.contains(&tag) {
                        toml.tags.push(tag);
                    }
                }
            }
        }
        if let Some(schema) = config_str(&dir_only_config, "schema") {
            // Phlo's folder schema keys are relative to the transform root,
            // including the namespace segment.
            let folder_key = model.dir.join("/");
            toml.set_folder(&folder_key, "schema", &format!("\"{schema}\""));
        }
    }

    match materialized.as_str() {
        "view" | "table" | "incremental" => {}
        "ephemeral" => {
            outcome.issues.push(MigrationIssue::new(
                codes::MATERIALIZATION,
                "`ephemeral` materialisation has no native equivalent; emitted as a view",
            ));
        }
        "snapshot" => {
            outcome.classification = Classification::Unsupported;
            outcome.emitted_path = None;
            outcome.issues.push(MigrationIssue::new(
                codes::MATERIALIZATION,
                "`snapshot` materialisation is not supported",
            ));
            return (outcome, None);
        }
        other => {
            outcome.issues.push(MigrationIssue::new(
                codes::MATERIALIZATION,
                format!("custom materialisation `{other}` cannot be verified"),
            ));
        }
    }

    let effective = if materialized == "ephemeral" {
        "view".to_string()
    } else if materialized == "incremental" || materialized == "view" || materialized == "table" {
        materialized.clone()
    } else {
        "table".to_string()
    };

    // Incremental intent.
    let unique_key: Vec<String> = merged
        .get("unique_key")
        .and_then(Lit::as_str_list)
        .unwrap_or_default();
    let incremental_strategy = merged
        .get("incremental_strategy")
        .and_then(Lit::as_str)
        .map(str::to_ascii_lowercase);
    let partition_by: Vec<String> = merged
        .get("partition_by")
        .map(|value| match value {
            Lit::Dict(items) => items
                .iter()
                .find(|(k, _)| k == "field")
                .map(|(_, v)| v.as_str_list().unwrap_or_default())
                .unwrap_or_default(),
            other => other.as_str_list().unwrap_or_default(),
        })
        .unwrap_or_default();

    let mut incremental_directive: Option<String> = None;
    if materialized == "incremental" && !lowered.incremental_reduced {
        let strategy = incremental_strategy.as_deref().unwrap_or("default");
        incremental_directive = match (strategy, !unique_key.is_empty(), !partition_by.is_empty()) {
            ("merge" | "delete+insert" | "default" | "append", true, _) => {
                Some(format!("-- @incremental key={}", unique_key.join(",")))
            }
            ("insert_overwrite", _, true) => Some(format!(
                "-- @incremental partition={}",
                partition_by.join(",")
            )),
            ("append" | "insert_overwrite", false, false) => lowered
                .window_column
                .as_ref()
                .map(|column| format!("-- @incremental window={column}"))
                .or(Some("-- @incremental append".to_string())),
            ("default", false, _) => lowered
                .window_column
                .as_ref()
                .map(|column| format!("-- @incremental window={column}"))
                .or(Some("-- @incremental append".to_string())),
            (other, _, _) => {
                outcome.issues.push(MigrationIssue::new(
                    codes::INCREMENTAL_PATTERN,
                    format!("incremental strategy `{other}` has no native equivalent"),
                ));
                None
            }
        };
        if incremental_directive.is_none() {
            // Unmappable strategy: degrade to a full-refresh table, which is
            // always correct though slower.
            directives.push("-- @table".to_string());
        }
    } else if materialized == "incremental" && lowered.incremental_reduced {
        if !unique_key.is_empty() {
            // Merge-by-key over a full scan is still correct.
            incremental_directive = Some(format!("-- @incremental key={}", unique_key.join(",")));
        } else {
            directives.push("-- @table".to_string());
            outcome
                .notes
                .push("incremental reduced to a full-refresh table".to_string());
        }
    }

    if let Some(directive) = incremental_directive {
        directives.push(directive);
    } else if materialized == "table" && effective != inherited_materialized {
        directives.push("-- @table".to_string());
    } else if (materialized == "view" || materialized == "ephemeral")
        && effective != inherited_materialized
    {
        directives.push("-- @view".to_string());
    }

    // `unique_key` on a non-incremental model still conveys record identity.
    if materialized != "incremental" && !unique_key.is_empty() {
        directives.push(format!("-- @key {}", unique_key.join(",")));
        outcome
            .transformations
            .push(format!("unique_key → @key {}", unique_key.join(",")));
    }

    // Tags and owner.
    let mut tags: Vec<String> = merged
        .get("tags")
        .and_then(Lit::as_str_list)
        .unwrap_or_default();
    if let Some(entry) = properties.as_ref() {
        if let Some(extra) = entry.get("tags").and_then(yaml_str_list) {
            tags.extend(extra);
        }
    }
    tags.sort();
    tags.dedup();
    if !tags.is_empty() {
        directives.push(format!("-- @tags {}", tags.join(",")));
    }
    let owner = merged
        .get("meta")
        .and_then(|meta| match meta {
            Lit::Dict(items) => items
                .iter()
                .find(|(k, _)| k == "owner")
                .and_then(|(_, v)| v.as_str().map(str::to_string)),
            _ => None,
        })
        .or_else(|| {
            properties.as_ref().and_then(|entry| {
                entry
                    .get("meta")
                    .and_then(|meta| meta.get("owner"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        });
    if let Some(owner) = owner {
        directives.push(format!("-- @owner {owner}"));
    }

    // Review-worthy and ignored config keys. Keys already translated at a
    // higher level (project `+database` → `default_catalog`, dir `+schema` →
    // folder config) are excluded.
    let schema_translated =
        dir_only_config.contains_key("schema") || project_model_config.contains_key("schema");
    let database_translated = project_model_config.contains_key("database");
    for key in merged.keys() {
        let translated = (key == "database" && database_translated)
            || (key == "schema" && schema_translated && !model.dir.is_empty());
        if REVIEW_CONFIG_KEYS.contains(&key.as_str()) && !translated {
            outcome.issues.push(MigrationIssue::new(
                codes::UNTRANSLATED_CONFIG,
                format!("config `{key}` has no native equivalent and was dropped"),
            ));
        }
    }
    if merged.contains_key("schema") && !schema_translated {
        outcome.issues.push(MigrationIssue::new(
            codes::UNTRANSLATED_CONFIG,
            "per-model `schema` is not representable; Phlo derives the target schema from logical identity",
        ));
    }
    if merged.contains_key("alias") {
        outcome
            .notes
            .push("`alias` dropped; Phlo derives target names from logical identity".to_string());
    }
    let ignored: Vec<String> = merged
        .keys()
        .filter(|key| {
            IGNORED_CONFIG_KEYS.contains(&key.as_str())
                && !REVIEW_CONFIG_KEYS.contains(&key.as_str())
        })
        .cloned()
        .collect();
    if !ignored.is_empty() {
        outcome.notes.push(format!(
            "dropped cosmetic/runtime config: {}",
            ignored.join(", ")
        ));
    }
    let unrecognised: Vec<String> = merged
        .keys()
        .filter(|key| {
            !IGNORED_CONFIG_KEYS.contains(&key.as_str())
                && !REVIEW_CONFIG_KEYS.contains(&key.as_str())
                && !matches!(
                    key.as_str(),
                    "materialized"
                        | "unique_key"
                        | "incremental_strategy"
                        | "partition_by"
                        | "tags"
                        | "meta"
                        | "contract"
                        | "enabled"
                        | "alias"
                        | "schema"
                        | "on_schema_change"
                )
        })
        .cloned()
        .collect();
    if !unrecognised.is_empty() {
        outcome.notes.push(format!(
            "unrecognised config dropped: {}",
            unrecognised.join(", ")
        ));
    }
    if merged.contains_key("on_schema_change") {
        outcome.notes.push(
            "on_schema_change dropped; Phlo classifies schema changes at plan time".to_string(),
        );
    }

    // Column-level property tests and contracts.
    if let Some(entry) = properties.as_ref() {
        directives.extend(emit_property_tests(
            entry,
            &logical,
            &unique_key,
            ctx,
            &mut outcome,
        ));
        if let Some(desc) = entry.get("description").and_then(Value::as_str) {
            for line in desc.trim().lines() {
                header_notes.push(format!("-- {line}"));
            }
        }
        if entry.get("versions").is_some() || entry.get("latest_version").is_some() {
            outcome.issues.push(MigrationIssue::new(
                codes::UNTRANSLATED_CONFIG,
                "model versions have no native equivalent",
            ));
        }
    }

    // Description from config() too.
    if let Some(Lit::Str(desc)) = merged.get("description") {
        for line in desc.trim().lines() {
            header_notes.push(format!("-- {line}"));
        }
    }

    // Contract enforcement → generated phlo.toml sections (queued on ctx).
    let contract_enforced = merged
        .get("contract")
        .map(|value| match value {
            Lit::Dict(items) => items
                .iter()
                .find(|(k, _)| k == "enforced")
                .map(|(_, v)| *v == Lit::Bool(true))
                .unwrap_or(false),
            _ => false,
        })
        .unwrap_or(false);
    if contract_enforced {
        let mut section = format!("[model.\"{logical}\".contract]\nenforced = true\n");
        if let Some(entry) = properties.as_ref() {
            if let Some(columns) = entry.get("columns").and_then(Value::as_sequence) {
                for column in columns {
                    let Some(name) = column.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let data_type = column.get("data_type").and_then(Value::as_str);
                    let nullable =
                        column
                            .get("constraints")
                            .and_then(Value::as_sequence)
                            .map(|constraints| {
                                !constraints.iter().any(|c| {
                                    c.get("type").and_then(Value::as_str) == Some("not_null")
                                })
                            });
                    if data_type.is_some() || nullable.is_some() {
                        section.push_str(&format!("\n[model.\"{logical}\".columns.{name}]\n"));
                        if let Some(data_type) = data_type {
                            section.push_str(&format!("type = \"{data_type}\"\n"));
                        }
                        if let Some(nullable) = nullable {
                            section.push_str(&format!("nullable = {nullable}\n"));
                        }
                    }
                }
            }
        }
        ctx.contract_sections.push(section);
        outcome
            .transformations
            .push("contract emitted to phlo.toml".to_string());
    }

    if !outcome.issues.is_empty() {
        outcome.classification = Classification::Review;
    }

    // Compose the emitted file.
    let mut file = String::new();
    file.push_str(&format!(
        "-- translated from dbt model `{}` ({})\n",
        model.stem, display_path
    ));
    for note in &header_notes {
        file.push_str(note);
        file.push('\n');
    }
    for directive in &directives {
        file.push_str(directive);
        file.push('\n');
    }
    file.push('\n');
    file.push_str(lowered.sql.trim_start_matches(['\n', ' ', '\t']));
    if !file.ends_with('\n') {
        file.push('\n');
    }
    (outcome, Some((rel_output, file)))
}

/// Emit native tests for property-file column/model tests. `key_columns`
/// are the model's `@key`s: `unique`/`not_null` on them are implied and
/// folded away. Returns extra directives to add to the model header.
fn emit_property_tests(
    entry: &Value,
    logical: &str,
    key_columns: &[String],
    ctx: &mut Context,
    outcome: &mut ResourceOutcome,
) -> Vec<String> {
    let mut directives = Vec::new();
    let columns = entry.get("columns").and_then(Value::as_sequence);
    if let Some(columns) = columns {
        for column in columns {
            let Some(name) = column.get("name").and_then(Value::as_str) else {
                continue;
            };
            let tests = column.get("tests").or_else(|| column.get("data_tests"));
            for test in tests.and_then(Value::as_sequence).into_iter().flatten() {
                translate_generic_test(
                    test,
                    logical,
                    Some(name),
                    key_columns,
                    ctx,
                    outcome,
                    &mut directives,
                );
            }
        }
    }
    let tests = entry.get("tests").or_else(|| entry.get("data_tests"));
    for test in tests.and_then(Value::as_sequence).into_iter().flatten() {
        translate_generic_test(
            test,
            logical,
            None,
            key_columns,
            ctx,
            outcome,
            &mut directives,
        );
    }
    directives
}

/// Convert one generic test entry (string or `{name: {args}}`) into either a
/// native directive, a generated `tests/` file, or a review issue.
fn translate_generic_test(
    test: &Value,
    model_logical: &str,
    column: Option<&str>,
    key_columns: &[String],
    ctx: &mut Context,
    outcome: &mut ResourceOutcome,
    directives: &mut Vec<String>,
) {
    let (name, args) = match test {
        Value::String(name) => (name.clone(), Mapping::new()),
        Value::Mapping(map) if map.len() == 1 => {
            let (key, value) = map.iter().next().expect("one entry");
            let args = value.as_mapping().cloned().unwrap_or_default();
            (yaml_key(key), args)
        }
        _ => {
            outcome.issues.push(MigrationIssue::new(
                codes::UNSUPPORTED_TEST,
                "unrecognised test definition",
            ));
            return;
        }
    };

    let where_clause = args
        .get(Value::String("where".into()))
        .or_else(|| {
            args.get(Value::String("config".into()))
                .and_then(|config| config.get(Value::String("where".into())))
        })
        .and_then(Value::as_str);

    let test_file = |ctx: &mut Context, outcome: &mut ResourceOutcome, kind: &str, sql: String| {
        let mut body = sql;
        if let Some(filter) = where_clause {
            body = format!("select * from ({body}) as __phlo_t where {filter}");
        }
        let file_name = format!(
            "tests/generated/{}__{}__{}.sql",
            model_logical.replace('.', "__"),
            column.unwrap_or("model"),
            kind
        );
        ctx.generated_tests.push(EmittedFile {
            rel_path: file_name,
            contents: format!("{body}\n"),
        });
        outcome.transformations.push(format!(
            "{kind} test on `{}` emitted as a native test",
            column.unwrap_or(model_logical)
        ));
    };

    match name.as_str() {
        "unique" => {
            let Some(column) = column else {
                outcome.issues.push(MigrationIssue::new(
                    codes::UNSUPPORTED_TEST,
                    "model-level `unique` test is ambiguous",
                ));
                return;
            };
            if key_columns.iter().any(|key| key == column) {
                outcome
                    .transformations
                    .push(format!("unique test on `{column}` folded into @key"));
                return;
            }
            test_file(ctx, outcome,
                "unique",
                format!(
                    "select \"{column}\", count(*) as n from {model_logical} group by \"{column}\" having count(*) > 1"
                ),
            );
        }
        "not_null" => {
            let Some(column) = column else {
                outcome.issues.push(MigrationIssue::new(
                    codes::UNSUPPORTED_TEST,
                    "model-level `not_null` test is ambiguous",
                ));
                return;
            };
            if key_columns.iter().any(|key| key == column) {
                outcome
                    .transformations
                    .push(format!("not_null test on `{column}` folded into @key"));
                return;
            }
            directives.push(format!("-- @not-null {column}"));
            outcome
                .transformations
                .push(format!("not_null test on `{column}` became @not-null"));
        }
        "accepted_values" => {
            let values = args
                .get(Value::String("values".into()))
                .and_then(Value::as_sequence)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| match yaml_to_lit(item).to_sql_literal() {
                            Some(literal) => literal,
                            None => "NULL".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                });
            match (column, values) {
                (Some(column), Some(values)) => test_file(
                    ctx,
                    outcome,
                    "accepted_values",
                    format!("select * from {model_logical} where \"{column}\" not in ({values})"),
                ),
                _ => outcome.issues.push(MigrationIssue::new(
                    codes::UNSUPPORTED_TEST,
                    "`accepted_values` test lacks a column or values",
                )),
            }
        }
        "relationships" => {
            let to = args.get(Value::String("to".into())).and_then(Value::as_str);
            let field = args
                .get(Value::String("field".into()))
                .and_then(Value::as_str)
                .unwrap_or("id");
            let Some(column) = column else {
                outcome.issues.push(MigrationIssue::new(
                    codes::UNSUPPORTED_TEST,
                    "model-level `relationships` test is ambiguous",
                ));
                return;
            };
            let Some(target) = to.and_then(|to| resolve_test_target(to, ctx)) else {
                outcome.issues.push(MigrationIssue::new(
                    codes::UNSUPPORTED_TEST,
                    "`relationships` test target could not be resolved",
                ));
                return;
            };
            test_file(ctx, outcome,
                "relationships",
                format!(
                    "select child.* from {model_logical} child left join {target} parent on child.\"{column}\" = parent.\"{field}\" where child.\"{column}\" is not null and parent.\"{field}\" is null"
                ),
            );
        }
        "unique_combination_of_columns" | "dbt_utils.unique_combination_of_columns" => {
            let columns = args
                .get(Value::String("combination_of_columns".into()))
                .and_then(Value::as_sequence)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|c| format!("\"{c}\""))
                        .collect::<Vec<_>>()
                });
            match columns {
                Some(columns) if !columns.is_empty() => test_file(ctx, outcome,
                    "unique_combination",
                    format!(
                        "select {}, count(*) as n from {model_logical} group by {} having count(*) > 1",
                        columns.join(", "),
                        columns.join(", ")
                    ),
                ),
                _ => outcome.issues.push(MigrationIssue::new(
                    codes::UNSUPPORTED_TEST,
                    "`unique_combination_of_columns` lacks `combination_of_columns`",
                )),
            }
        }
        other => {
            outcome.issues.push(MigrationIssue::new(
                codes::UNSUPPORTED_TEST,
                format!("generic test `{other}` has no native conversion"),
            ));
        }
    }
}

/// Emit native tests declared on a source table (`tests:`/`data_tests:` and
/// per-column tests) against the resolved physical relation.
fn emit_source_tests(
    info: &SourceInfo,
    source: &str,
    table: &str,
    ctx: &mut Context,
    outcome: &mut ResourceOutcome,
) {
    let relation = info.relation.clone();
    let name = format!("{source}__{table}");
    let table_value = info.table.clone();

    let emit = |ctx: &mut Context, kind: &str, sql: String| {
        ctx.generated_tests.push(EmittedFile {
            rel_path: format!("tests/generated/{name}__{kind}.sql"),
            contents: format!("{sql}\n"),
        });
    };

    if let Some(columns) = table_value.get("columns").and_then(Value::as_sequence) {
        for column in columns {
            let Some(column_name) = column.get("name").and_then(Value::as_str) else {
                continue;
            };
            let tests = column.get("tests").or_else(|| column.get("data_tests"));
            for test in tests.and_then(Value::as_sequence).into_iter().flatten() {
                let test_name = match test {
                    Value::String(name) => name.clone(),
                    Value::Mapping(map) if map.len() == 1 => {
                        yaml_key(map.iter().next().expect("one entry").0)
                    }
                    _ => continue,
                };
                let args = match test {
                    Value::Mapping(map) if map.len() == 1 => map
                        .iter()
                        .next()
                        .map(|(_, v)| v.as_mapping().cloned().unwrap_or_default())
                        .unwrap_or_default(),
                    _ => Mapping::new(),
                };
                match test_name.as_str() {
                    "unique" => emit(
                        ctx,
                        &format!("{column_name}__unique"),
                        format!(
                            "select \"{column_name}\", count(*) as n from {relation} group by \"{column_name}\" having count(*) > 1"
                        ),
                    ),
                    "not_null" => emit(
                        ctx,
                        &format!("{column_name}__not_null"),
                        format!(
                            "select * from {relation} where \"{column_name}\" is null"
                        ),
                    ),
                    "accepted_values" => {
                        let values = args
                            .get(Value::String("values".into()))
                            .and_then(Value::as_sequence)
                            .map(|items| {
                                items
                                    .iter()
                                    .map(|item| {
                                        yaml_to_lit(item).to_sql_literal().unwrap_or_else(|| "NULL".into())
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            });
                        match values {
                            Some(values) => emit(
                                ctx,
                                &format!("{column_name}__accepted_values"),
                                format!(
                                    "select * from {relation} where \"{column_name}\" not in ({values})"
                                ),
                            ),
                            None => outcome.issues.push(MigrationIssue::new(
                                codes::UNSUPPORTED_TEST,
                                "`accepted_values` source test lacks `values`",
                            )),
                        }
                    }
                    other => outcome.issues.push(MigrationIssue::new(
                        codes::UNSUPPORTED_TEST,
                        format!("source column test `{other}` has no native conversion"),
                    )),
                }
            }
        }
    }
    // Table-level tests (e.g. `freshness` is metadata — note it).
    if table_value.get("freshness").is_some() {
        outcome
            .notes
            .push("freshness metadata recorded in manifest; Phlo models source freshness via source state".to_string());
    }
    if table_value
        .get("tests")
        .or_else(|| table_value.get("data_tests"))
        .and_then(Value::as_sequence)
        .is_some_and(|tests| !tests.is_empty())
    {
        outcome.issues.push(MigrationIssue::new(
            codes::UNSUPPORTED_TEST,
            "table-level source tests are not translated",
        ));
    }
}

/// Resolve a `ref('x')`/`source('a','b')` string appearing inside test args.
fn resolve_test_target(spec: &str, ctx: &Context) -> Option<String> {
    let inner = spec
        .trim()
        .trim_start_matches("{{")
        .trim_end_matches("}}")
        .trim();
    let call = jinja::parse_call(inner)?;
    match call.name.as_str() {
        "ref" => ctx.model_targets.get(call.arg(0)?).cloned(),
        "source" => ctx
            .sources
            .get(&(call.arg(0)?.to_string(), call.arg(1)?.to_string()))
            .map(|info| info.relation.clone()),
        _ => None,
    }
}

/// Translate a singular test file.
fn translate_singular_test(test: &crate::project::DbtSqlFile, ctx: &Context) -> ModelOutcome {
    let mut rel = String::from("tests");
    for segment in &test.dir {
        rel.push('/');
        rel.push_str(&sanitize_segment(segment));
    }
    rel.push('/');
    rel.push_str(&sanitize_segment(&test.stem));
    rel.push_str(".sql");

    let mut outcome = ResourceOutcome {
        kind: ResourceKind::SingularTest,
        name: test.stem.clone(),
        source_path: Some(display(&test.rel_path)),
        classification: Classification::Clean,
        emitted_path: Some(rel.clone()),
        transformations: Vec::new(),
        notes: Vec::new(),
        issues: Vec::new(),
        source_hash: Some(hash_text(&test.sql)),
    };

    let lowered = lower_sql(&test.sql, ctx);
    outcome.transformations.extend(lowered.transformations);
    outcome.notes.extend(lowered.notes);
    outcome.issues.extend(lowered.issues);
    if !outcome.issues.is_empty() {
        outcome.classification = Classification::Review;
    }
    if !lowered.config.is_empty() {
        outcome.notes.push(format!(
            "dropped test config: {}",
            lowered
                .config
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut file = String::new();
    file.push_str(&format!(
        "-- translated from dbt test `{}` ({})\n\n",
        test.stem,
        display(&test.rel_path)
    ));
    file.push_str(lowered.sql.trim_start_matches(['\n', ' ', '\t']));
    if !file.ends_with('\n') {
        file.push('\n');
    }
    (outcome, Some((rel, file)))
}

/// Outcomes for resources that exist only in property files (sources,
/// exposures, metrics, semantic models, yaml-declared snapshots/seeds).
fn collect_property_resources(project: &DbtProject, outcomes: &mut Vec<ResourceOutcome>) {
    for file in &project.property_files {
        let path = display(&file.rel_path);
        for (key, kind, class, message) in [
            (
                "exposures",
                ResourceKind::Exposure,
                Classification::Unsupported,
                "dbt exposures have no Phlo equivalent",
            ),
            (
                "metrics",
                ResourceKind::Exposure,
                Classification::Unsupported,
                "dbt metrics belong to a semantic layer, which Phlo does not implement",
            ),
            (
                "semantic_models",
                ResourceKind::Exposure,
                Classification::Unsupported,
                "dbt semantic models belong to a semantic layer, which Phlo does not implement",
            ),
            (
                "snapshots",
                ResourceKind::Snapshot,
                Classification::Unsupported,
                "dbt snapshots have no Phlo equivalent",
            ),
            (
                "seeds",
                ResourceKind::Seed,
                Classification::Review,
                "dbt seed declarations have no native static-data representation",
            ),
        ] {
            if let Some(items) = file.root.get(key).and_then(Value::as_sequence) {
                for item in items {
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("(unnamed)")
                        .to_string();
                    outcomes.push(ResourceOutcome {
                        kind,
                        name,
                        source_path: Some(path.clone()),
                        classification: class,
                        emitted_path: None,
                        transformations: Vec::new(),
                        notes: Vec::new(),
                        issues: vec![MigrationIssue::new(codes::UNSUPPORTED_KIND, message)],
                        source_hash: None,
                    });
                }
            }
        }
    }
}

/// Assemble the summary report from per-resource outcomes.
fn build_report(project: &DbtProject, outcomes: &[ResourceOutcome]) -> MigrationReport {
    let mut summary: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for outcome in outcomes {
        *summary
            .entry(outcome.kind.label().to_string())
            .or_default()
            .entry(outcome.classification.label().to_string())
            .or_default() += 1;
    }
    let models: Vec<&ResourceOutcome> = outcomes
        .iter()
        .filter(|outcome| outcome.kind == ResourceKind::Model)
        .collect();
    let clean_models = models
        .iter()
        .filter(|outcome| outcome.classification == Classification::Clean)
        .count();
    let coverage = if models.is_empty() {
        1.0
    } else {
        clean_models as f64 / models.len() as f64
    };
    MigrationReport {
        translator_version: TRANSLATOR_VERSION.to_string(),
        source_root: display(&project.root),
        project_name: project.name.clone(),
        summary,
        model_coverage: coverage,
        load_warnings: project.load_warnings.clone(),
        resources: outcomes.to_vec(),
    }
}

fn yaml_str_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Sequence(items) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

/// Write emitted files plus the migration manifest under `output`.
///
/// Never overwrites existing files — returns the conflicting path instead.
pub fn write_translation(output: &Path, translation: &Translation) -> io::Result<()> {
    for file in &translation.files {
        let path = output.join(&file.rel_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &file.contents)?;
    }
    let manifest_dir = output.join(".phlo").join("migration");
    std::fs::create_dir_all(&manifest_dir)?;
    let manifest = serde_json::to_string_pretty(&translation.manifest).map_err(io::Error::other)?;
    std::fs::write(manifest_dir.join("dbt-translation.json"), manifest)?;
    Ok(())
}
