//! The native Phlo filesystem frontend.
//!
//! Discovers `.sql` transforms across the conventional roots
//! (`transforms/**` and `workflows/*/transforms/**`) plus any configured
//! includes, derives stable logical identities, and lowers everything into a
//! frontend-agnostic [`SemanticProject`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use phlo_transform_sql::{parse_directives, IncrementalStrategy, Materialization};
use walkdir::{DirEntry, WalkDir};

use crate::config::{
    read_phlo_config, read_root_config, FolderConfig, ModelContractConfig, PhloConfig,
    TransformRootConfig,
};
use crate::diagnostics::{codes, Diagnostic};
use crate::identity::{IdentityError, ModelId, Namespace};
use crate::model::{
    FrontendKind, ModelConfig, ModelOrigin, RootKind, RootNamespaceStrategy, RootRef,
    SemanticModel, SemanticProject, SemanticTest, TestId, TransformRoot, TransformRootId,
    WorkspaceDefaults,
};
use crate::semantic::{ColumnContract, ColumnTolerance, DataType, DiffPolicySpec, ModelContract};

const DEFAULT_INCLUDES: &[&str] = &["transforms/**", "workflows/*/transforms/**"];
const DEFAULT_EXCLUDES: &[&str] = &[
    "**/target/**",
    "**/.phlo/**",
    "**/.git/**",
    "**/node_modules/**",
];

/// Load and lower a native Phlo workspace.
///
/// Fatal setup problems (a missing workspace root or unreadable config) are
/// returned as `Err`. Per-file problems are attached to
/// [`SemanticProject::diagnostics`] so that `check` can report them all.
pub fn load_project(workspace_root: &Path) -> Result<SemanticProject, Vec<Diagnostic>> {
    if !workspace_root.is_dir() {
        return Err(vec![Diagnostic::error(
            codes::PROJECT_WORKSPACE_NOT_FOUND,
            format!(
                "workspace root `{}` does not exist or is not a directory",
                workspace_root.to_string_lossy()
            ),
        )
        .with_path(workspace_root.to_string_lossy().replace('\\', "/"))]);
    }

    let config = match read_phlo_config(workspace_root) {
        Ok(config) => config,
        Err(diagnostic) => return Err(vec![diagnostic]),
    };

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let include = match build_globset(&combined_includes(&config)) {
        Ok(globset) => globset,
        Err(diagnostic) => return Err(vec![diagnostic]),
    };
    let exclude = match build_globset(&combined_excludes(&config)) {
        Ok(globset) => globset,
        Err(diagnostic) => return Err(vec![diagnostic]),
    };

    let files = walk_sql_files(workspace_root, &include, &exclude, &mut diagnostics);

    // First pass: derive identity and root membership for each file.
    let mut derived: Vec<DerivedFile> = Vec::with_capacity(files.len());
    for relative_path in files {
        match derive_identity(workspace_root, &relative_path, &config) {
            Ok(identity) => derived.push(DerivedFile {
                relative_path,
                identity,
            }),
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }

    // Assign deterministic root ids.
    let mut root_infos: BTreeMap<PathBuf, RootInfo> = BTreeMap::new();
    for file in &derived {
        root_infos
            .entry(file.identity.root_dir.clone())
            .or_insert_with(|| RootInfo {
                strategy: file.identity.root_strategy.clone(),
                kind: file.identity.kind,
            });
    }
    let root_ids: BTreeMap<PathBuf, TransformRootId> = root_infos
        .keys()
        .enumerate()
        .map(|(position, path)| (path.clone(), TransformRootId(position as u32)))
        .collect();

    detect_duplicate_namespaces(&root_infos, &mut diagnostics);

    let roots: Vec<TransformRoot> = root_infos
        .iter()
        .map(|(path, info)| TransformRoot {
            id: root_ids[path],
            path: path.clone(),
            strategy: info.strategy.clone(),
            kind: info.kind,
        })
        .collect();

    let defaults = workspace_defaults(&config, &mut diagnostics);

    // Second pass: read SQL, lower directives and build semantic models.
    let mut models: Vec<SemanticModel> = Vec::with_capacity(derived.len());
    for file in &derived {
        match lower_model(workspace_root, file, &root_ids, &defaults) {
            Ok(Some(model)) => models.push(model),
            Ok(None) => {}
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }

    models.sort_by(|left, right| left.id.cmp(&right.id));

    let contracts = normalized_sections(&config);
    for model in &mut models {
        if let Some(section) = contracts.get(&model.id.logical_name()) {
            model.contract = contract_from_section(section);
            apply_incremental_config(&mut model.config, section);
            apply_diff_config(&mut model.config, section);
        }
    }

    let tests = load_tests(workspace_root, &mut diagnostics);

    Ok(SemanticProject {
        workspace_root: Some(workspace_root.to_path_buf()),
        roots,
        models,
        tests,
        defaults,
        cross_workflow: config.dependencies.cross_workflow,
        diagnostics,
    })
}

fn workspace_defaults(config: &PhloConfig, diagnostics: &mut Vec<Diagnostic>) -> WorkspaceDefaults {
    let materialization = match config.transform.default_materialization.as_deref() {
        Some(value) => Materialization::parse(value).unwrap_or_else(|| {
            diagnostics.push(
                Diagnostic::error(
                    codes::CONFIG_INVALID,
                    format!("unknown default_materialization `{value}`"),
                )
                .with_path("phlo.toml")
                .with_help("expected `view` or `table`"),
            );
            Materialization::View
        }),
        None => Materialization::View,
    };

    WorkspaceDefaults {
        materialization,
        catalog: config.transform.default_catalog.clone(),
        schema: config.transform.default_schema.clone(),
    }
}

/// Normalise `[model.<name>...]` sections, accepting dotted or underscored keys.
fn normalized_sections(config: &PhloConfig) -> BTreeMap<String, ModelContractConfig> {
    let mut sections = BTreeMap::new();
    for (key, section) in &config.model {
        sections.insert(key.clone(), section.clone());
        if key.contains('_') {
            sections.insert(key.replace('_', "."), section.clone());
        }
    }
    sections
}

fn contract_from_section(section: &ModelContractConfig) -> Option<ModelContract> {
    if !section.contract.enforced && section.columns.is_empty() {
        return None;
    }
    Some(ModelContract {
        enforced: section.contract.enforced,
        columns: section
            .columns
            .iter()
            .map(|(name, column)| ColumnContract {
                name: name.clone(),
                data_type: column.data_type.as_deref().and_then(|text| {
                    let data_type = DataType::parse_trino(text);
                    data_type.is_known().then_some(data_type)
                }),
                nullable: column.nullable,
            })
            .collect(),
    })
}

/// Apply `[model.<name>.incremental]` to a model's effective config.
fn apply_incremental_config(config: &mut ModelConfig, section: &ModelContractConfig) {
    let incremental = &section.incremental;
    if config.incremental.is_none() {
        if let Some(strategy) = incremental
            .strategy
            .as_deref()
            .and_then(|strategy| build_incremental(strategy, incremental))
        {
            config.materialization = Materialization::Incremental;
            config.incremental = Some(strategy);
        }
    }
    if let Some(IncrementalStrategy::TimeWindow {
        overlap_seconds, ..
    }) = &mut config.incremental
    {
        if overlap_seconds.is_none() {
            *overlap_seconds = incremental
                .overlap
                .as_deref()
                .and_then(parse_overlap_seconds);
        }
    }
}

/// Apply `[model.<name>.diff]` to a model's effective config.
fn apply_diff_config(config: &mut ModelConfig, section: &ModelContractConfig) {
    let diff = &section.diff;
    let has_policy = diff.max_added_rows.is_some()
        || diff.max_removed_rows.is_some()
        || diff.max_modified_rows.is_some()
        || diff.max_changed_fraction.is_some()
        || diff.require_full_diff
        || diff.require_keyed_diff
        || !diff.columns.is_empty();
    if !has_policy {
        return;
    }
    let tolerances = diff
        .columns
        .iter()
        .map(|(name, tolerance)| {
            (
                name.clone(),
                ColumnTolerance {
                    absolute: tolerance.absolute_tolerance,
                    relative: tolerance.relative_tolerance,
                },
            )
        })
        .collect();
    config.diff = Some(DiffPolicySpec {
        max_added_rows: diff.max_added_rows,
        max_removed_rows: diff.max_removed_rows,
        max_modified_rows: diff.max_modified_rows,
        max_changed_fraction: diff.max_changed_fraction,
        require_full_diff: diff.require_full_diff,
        require_keyed_diff: diff.require_keyed_diff,
        tolerances,
    });
}

fn build_incremental(
    strategy: &str,
    config: &crate::config::IncrementalModelConfig,
) -> Option<IncrementalStrategy> {
    let columns = |value: &Option<String>| -> Vec<String> {
        value
            .as_deref()
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    match strategy.trim().to_ascii_lowercase().as_str() {
        "append" => Some(IncrementalStrategy::Append),
        "key" => {
            let columns = columns(&config.key);
            (!columns.is_empty()).then_some(IncrementalStrategy::Key { columns })
        }
        "partition" => {
            let columns = columns(&config.partition);
            (!columns.is_empty()).then_some(IncrementalStrategy::Partition { columns })
        }
        "time-window" | "window" => {
            config
                .column
                .as_ref()
                .map(|column| IncrementalStrategy::TimeWindow {
                    column: column.clone(),
                    overlap_seconds: None,
                })
        }
        _ => None,
    }
}

/// Parse a simple duration such as `30m`, `2h` or `1d` into seconds.
fn parse_overlap_seconds(value: &str) -> Option<u64> {
    let value = value.trim();
    let (number, unit) = value.split_at(value.find(|c: char| !c.is_ascii_digit())?);
    let number: u64 = number.parse().ok()?;
    match unit.trim() {
        "s" | "sec" | "secs" => Some(number),
        "m" | "min" | "mins" => Some(number * 60),
        "h" | "hr" | "hrs" => Some(number * 3600),
        "d" | "day" | "days" => Some(number * 86_400),
        _ => None,
    }
}

fn combined_includes(config: &PhloConfig) -> Vec<String> {
    let mut includes: Vec<String> = DEFAULT_INCLUDES.iter().map(|s| s.to_string()).collect();
    includes.extend(config.transform.discovery.include.iter().cloned());
    includes.sort();
    includes.dedup();
    includes
}

fn combined_excludes(config: &PhloConfig) -> Vec<String> {
    let mut excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
    excludes.extend(config.transform.discovery.exclude.iter().cloned());
    excludes.sort();
    excludes.dedup();
    excludes
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, Diagnostic> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| {
                Diagnostic::error(
                    codes::CONFIG_INVALID,
                    format!("invalid glob `{pattern}`: {error}"),
                )
                .with_path("phlo.toml")
            })?;
        builder.add(glob);
    }
    builder.build().map_err(|error| {
        Diagnostic::error(
            codes::CONFIG_INVALID,
            format!("invalid discovery globs: {error}"),
        )
        .with_path("phlo.toml")
    })
}

fn walk_sql_files(
    workspace_root: &Path,
    include: &GlobSet,
    exclude: &GlobSet,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(workspace_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_descend)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(Diagnostic::warning(
                    codes::PROJECT_FILE_READ,
                    format!("could not read a workspace entry: {error}"),
                ));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("sql") {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(workspace_root) else {
            continue;
        };
        let relative = relative.to_path_buf();
        let display = display_path(&relative);
        if !include.is_match(&display) || exclude.is_match(&display) {
            continue;
        }
        files.push(relative);
    }
    files.sort();
    files
}

fn should_descend(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | "target" | ".phlo" | "node_modules")
    )
}

/// Discover custom SQL tests from the conventional `tests/**/*.sql` location.
fn load_tests(workspace_root: &Path, diagnostics: &mut Vec<Diagnostic>) -> Vec<SemanticTest> {
    let include = match build_globset(&["tests/**".to_string()]) {
        Ok(globset) => globset,
        Err(diagnostic) => {
            diagnostics.push(diagnostic);
            return Vec::new();
        }
    };
    let exclude = match build_globset(
        &DEFAULT_EXCLUDES
            .iter()
            .map(|pattern| pattern.to_string())
            .collect::<Vec<_>>(),
    ) {
        Ok(globset) => globset,
        Err(diagnostic) => {
            diagnostics.push(diagnostic);
            return Vec::new();
        }
    };

    let files = walk_sql_files(workspace_root, &include, &exclude, diagnostics);
    let mut tests = Vec::with_capacity(files.len());
    for relative_path in files {
        let display = display_path(&relative_path);
        let full_path = workspace_root.join(&relative_path);
        let sql = match std::fs::read_to_string(&full_path) {
            Ok(sql) => sql,
            Err(error) => {
                diagnostics.push(
                    Diagnostic::error(
                        codes::PROJECT_FILE_READ,
                        format!("could not read test: {error}"),
                    )
                    .with_path(display),
                );
                continue;
            }
        };
        let Some(name) = test_name(&relative_path) else {
            diagnostics.push(
                Diagnostic::error(
                    codes::PROJECT_UNRESOLVABLE_PATH,
                    "could not derive a test name from the path",
                )
                .with_path(display),
            );
            continue;
        };
        tests.push(SemanticTest {
            id: TestId::new(name),
            sql,
            origin: ModelOrigin {
                frontend: FrontendKind::Native,
                path: Some(relative_path),
            },
        });
    }
    tests.sort_by(|left, right| left.id.cmp(&right.id));
    tests
}

fn test_name(relative_path: &Path) -> Option<String> {
    let without_extension = relative_path.with_extension("");
    let mut components: Vec<String> = without_extension
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    if components.first().map(String::as_str) == Some("tests") {
        components.remove(0);
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("."))
}

struct RootInfo {
    strategy: RootNamespaceStrategy,
    kind: RootKind,
}

struct DerivedFile {
    relative_path: PathBuf,
    identity: DerivedIdentity,
}

#[derive(Debug)]
struct DerivedIdentity {
    namespace: Namespace,
    /// Namespace-relative logical path.
    path: Vec<String>,
    root_dir: PathBuf,
    root_strategy: RootNamespaceStrategy,
    root_relative: Vec<String>,
    kind: RootKind,
    root_config: TransformRootConfig,
}

fn derive_identity(
    workspace_root: &Path,
    relative_path: &Path,
    config: &PhloConfig,
) -> Result<DerivedIdentity, Diagnostic> {
    let mut components: Vec<String> = relative_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    let filename = components
        .pop()
        .ok_or_else(|| unresolvable(relative_path, "empty path"))?;
    let stem = Path::new(&filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| unresolvable(relative_path, "file name has no stem"))?
        .to_string();
    let directories = components;

    let transforms_index = directories
        .iter()
        .position(|segment| segment == "transforms");
    let derived = match transforms_index {
        Some(0) => derive_from_global_root(relative_path, &directories, &stem, config)?,
        Some(index) => derive_from_named_root(relative_path, &directories, index, &stem)?,
        None => derive_from_configured_root(workspace_root, relative_path, &directories, &stem)?,
    };

    // A local transform.toml may override the namespace for the subtree and
    // supplies root/folder configuration for materialisation metadata.
    let root_config = read_root_config(workspace_root, &derived.namespace_dir)?;
    let mut namespace = derived.namespace;
    if let Some(override_namespace) = &root_config.namespace {
        namespace = validate_namespace(override_namespace, relative_path)?;
    }

    let root_strategy = match &derived.root_strategy {
        // A fixed root's namespace is whatever applies to it after override.
        RootNamespaceStrategy::Fixed(_) => RootNamespaceStrategy::Fixed(namespace.clone()),
        RootNamespaceStrategy::FirstSegment => RootNamespaceStrategy::FirstSegment,
    };

    Ok(DerivedIdentity {
        namespace,
        path: derived.path,
        root_dir: derived.root_dir,
        root_strategy,
        root_relative: derived.root_relative,
        kind: derived.kind,
        root_config,
    })
}

struct RawDerived {
    namespace: Namespace,
    path: Vec<String>,
    root_dir: PathBuf,
    root_strategy: RootNamespaceStrategy,
    root_relative: Vec<String>,
    namespace_dir: PathBuf,
    kind: RootKind,
}

fn derive_from_global_root(
    relative_path: &Path,
    directories: &[String],
    stem: &str,
    config: &PhloConfig,
) -> Result<RawDerived, Diagnostic> {
    let root_dir = PathBuf::from("transforms");
    if directories.len() >= 2 {
        let namespace = validate_namespace(&directories[1], relative_path)?;
        let mut path = directories[2..].to_vec();
        path.push(stem.to_string());
        let mut root_relative = directories[1..].to_vec();
        root_relative.push(stem.to_string());
        Ok(RawDerived {
            namespace,
            path,
            root_dir: root_dir.clone(),
            root_strategy: RootNamespaceStrategy::FirstSegment,
            root_relative,
            namespace_dir: root_dir.join(&directories[1]),
            kind: RootKind::GlobalTransforms,
        })
    } else {
        // Files directly inside `transforms/` use the configured default
        // namespace, or `default` when none is set.
        let namespace = config
            .transform
            .default_namespace
            .as_deref()
            .unwrap_or("default");
        let namespace = validate_namespace(namespace, relative_path)?;
        Ok(RawDerived {
            namespace,
            path: vec![stem.to_string()],
            root_dir: root_dir.clone(),
            root_strategy: RootNamespaceStrategy::FirstSegment,
            root_relative: vec![stem.to_string()],
            namespace_dir: root_dir,
            kind: RootKind::GlobalTransforms,
        })
    }
}

fn derive_from_named_root(
    relative_path: &Path,
    directories: &[String],
    transforms_index: usize,
    stem: &str,
) -> Result<RawDerived, Diagnostic> {
    let namespace = validate_namespace(&directories[transforms_index - 1], relative_path)?;
    let root_dir = PathBuf::from(directories[..=transforms_index].join("/"));
    let kind = if root_dir.starts_with("workflows") {
        RootKind::Workflow
    } else {
        RootKind::Custom
    };
    let mut path = directories[transforms_index + 1..].to_vec();
    path.push(stem.to_string());
    Ok(RawDerived {
        namespace: namespace.clone(),
        path: path.clone(),
        root_dir: root_dir.clone(),
        root_strategy: RootNamespaceStrategy::Fixed(namespace),
        root_relative: path,
        namespace_dir: root_dir,
        kind,
    })
}

/// Fallback for configured roots that do not contain a `transforms` segment.
/// The nearest ancestor directory containing a `transform.toml` defines the
/// root and namespace.
fn derive_from_configured_root(
    workspace_root: &Path,
    relative_path: &Path,
    directories: &[String],
    stem: &str,
) -> Result<RawDerived, Diagnostic> {
    for index in (0..directories.len()).rev() {
        let candidate: PathBuf = directories[..=index].iter().collect();
        if workspace_root
            .join(&candidate)
            .join("transform.toml")
            .is_file()
        {
            let configured = read_root_config(workspace_root, &candidate)
                .map_err(|_| unresolvable(relative_path, "invalid transform.toml"))?;
            let namespace = configured
                .namespace
                .as_deref()
                .unwrap_or(&directories[index]);
            let namespace = validate_namespace(namespace, relative_path)?;
            let mut path = directories[index + 1..].to_vec();
            path.push(stem.to_string());
            return Ok(RawDerived {
                namespace: namespace.clone(),
                path: path.clone(),
                root_dir: candidate.clone(),
                root_strategy: RootNamespaceStrategy::Fixed(namespace),
                root_relative: path,
                namespace_dir: candidate,
                kind: RootKind::Custom,
            });
        }
    }
    Err(unresolvable(
        relative_path,
        "no transform root could be derived from the path",
    ))
}

fn validate_namespace(value: &str, relative_path: &Path) -> Result<Namespace, Diagnostic> {
    let valid = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(Namespace::new(value))
    } else {
        Err(Diagnostic::error(
            codes::CONFIG_INVALID,
            format!("`{value}` is not a valid namespace"),
        )
        .with_path(display_path(relative_path)))
    }
}

fn detect_duplicate_namespaces(
    roots: &BTreeMap<PathBuf, RootInfo>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut by_namespace: BTreeMap<String, Vec<&PathBuf>> = BTreeMap::new();
    for (path, info) in roots {
        if let RootNamespaceStrategy::Fixed(namespace) = &info.strategy {
            by_namespace
                .entry(namespace.to_string())
                .or_default()
                .push(path);
        }
    }
    for (namespace, paths) in by_namespace {
        if paths.len() > 1 {
            diagnostics.push(
                Diagnostic::error(
                    codes::PROJECT_DUPLICATE_NAMESPACE,
                    format!("duplicate transform namespace `{namespace}`"),
                )
                .with_labels(paths.iter().map(|path| display_path(path)))
                .with_help("give each transform root a distinct namespace or remove the override"),
            );
        }
    }
}

fn lower_model(
    workspace_root: &Path,
    file: &DerivedFile,
    root_ids: &BTreeMap<PathBuf, TransformRootId>,
    defaults: &WorkspaceDefaults,
) -> Result<Option<SemanticModel>, Diagnostic> {
    let display = display_path(&file.relative_path);
    let full_path = workspace_root.join(&file.relative_path);
    let sql = std::fs::read_to_string(&full_path).map_err(|error| {
        Diagnostic::error(
            codes::PROJECT_FILE_READ,
            format!("could not read model: {error}"),
        )
        .with_path(display.clone())
    })?;

    let directives = parse_directives(&sql);
    let config = effective_config(&file.identity, &directives, defaults);

    let derived_id = ModelId::new(file.identity.namespace.clone(), file.identity.path.clone());
    let id = match directives.pinned_id.as_deref() {
        Some(raw) => match ModelId::parse(raw) {
            Ok(pinned) => pinned,
            Err(_) => match derived_id {
                Ok(id) => id,
                Err(error) => {
                    return Err(invalid_identity(&display, &error));
                }
            },
        },
        None => match derived_id {
            Ok(id) => id,
            Err(error) => return Err(invalid_identity(&display, &error)),
        },
    };

    let root_id = root_ids[&file.identity.root_dir];
    Ok(Some(SemanticModel {
        id,
        namespace: file.identity.namespace.clone(),
        path: file.identity.path.clone(),
        root: Some(RootRef {
            id: root_id,
            relative_path: file.identity.root_relative.clone(),
        }),
        sql,
        directives,
        config,
        workflow: (file.identity.kind == RootKind::Workflow)
            .then(|| file.identity.namespace.to_string()),
        contract: None,
        origin: ModelOrigin {
            frontend: FrontendKind::Native,
            path: Some(file.relative_path.clone()),
        },
    }))
}

/// Apply workspace → root → folder → model precedence to produce the
/// effective configuration.
fn effective_config(
    identity: &DerivedIdentity,
    directives: &phlo_transform_sql::Directives,
    defaults: &WorkspaceDefaults,
) -> ModelConfig {
    let folder = folder_config_for(&identity.root_config, &identity.root_relative);

    let mut materialization = defaults.materialization;
    if let Some(value) = identity.root_config.materialized.as_deref() {
        if let Some(parsed) = Materialization::parse(value) {
            materialization = parsed;
        }
    }
    if let Some(value) = folder.and_then(|folder| folder.materialized.as_deref()) {
        if let Some(parsed) = Materialization::parse(value) {
            materialization = parsed;
        }
    }
    if let Some(parsed) = directives.materialization {
        materialization = parsed;
    }

    let mut tags = identity.root_config.tags.clone();
    if let Some(folder) = folder {
        tags.extend(folder.tags.iter().cloned());
    }
    tags.extend(directives.tags.iter().cloned());
    tags.sort();
    tags.dedup();

    let owner = directives
        .owner
        .clone()
        .or_else(|| folder.and_then(|folder| folder.owner.clone()))
        .or_else(|| identity.root_config.owner.clone());

    let schema = folder.and_then(|folder| folder.schema.clone());

    ModelConfig {
        materialization,
        tags,
        owner,
        schema,
        incremental: directives.incremental.clone(),
        diff: None,
    }
}

/// Find the deepest configured folder that contains the model path.
fn folder_config_for<'a>(
    root_config: &'a TransformRootConfig,
    root_relative: &[String],
) -> Option<&'a FolderConfig> {
    let directories = &root_relative[..root_relative.len().saturating_sub(1)];
    let directory_parts: Vec<&str> = directories.iter().map(String::as_str).collect();
    let mut best: Option<(usize, &'a FolderConfig)> = None;
    for (key, config) in &root_config.folder {
        let parts: Vec<&str> = key.split('/').filter(|part| !part.is_empty()).collect();
        if directory_parts.starts_with(&parts)
            && best
                .map(|(best_len, _)| parts.len() > best_len)
                .unwrap_or(true)
        {
            best = Some((parts.len(), config));
        }
    }
    best.map(|(_, config)| config)
}

fn invalid_identity(path: &str, error: &IdentityError) -> Diagnostic {
    Diagnostic::error(
        codes::PROJECT_INVALID_MODEL_ID,
        format!("could not derive a valid model id: {error}"),
    )
    .with_path(path.to_string())
}

fn unresolvable(relative_path: &Path, reason: &str) -> Diagnostic {
    Diagnostic::error(
        codes::PROJECT_UNRESOLVABLE_PATH,
        format!("could not derive a transform root: {reason}"),
    )
    .with_path(display_path(relative_path))
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(path: &str) -> DerivedIdentity {
        derive_identity(
            Path::new("/does-not-exist"),
            Path::new(path),
            &PhloConfig::default(),
        )
        .unwrap_or_else(|_| panic!("{path} should derive"))
    }

    fn namespace_and_path(path: &str) -> (String, Vec<String>) {
        let identity = identity(path);
        (identity.namespace.to_string(), identity.path)
    }

    #[test]
    fn global_root_uses_first_segment_namespace() {
        assert_eq!(
            namespace_and_path("transforms/shared/dimensions/date.sql"),
            (
                "shared".to_string(),
                vec!["dimensions".to_string(), "date".to_string()]
            )
        );
    }

    #[test]
    fn workflow_root_uses_workflow_name() {
        let identity = identity("workflows/assay/transforms/staging/raw.sql");
        assert_eq!(identity.namespace.to_string(), "assay");
        assert_eq!(identity.path, vec!["staging", "raw"]);
        assert_eq!(
            identity.root_dir,
            PathBuf::from("workflows/assay/transforms")
        );
        assert_eq!(identity.kind, RootKind::Workflow);
        assert_eq!(
            identity.root_strategy,
            RootNamespaceStrategy::Fixed(Namespace::from("assay"))
        );
    }

    #[test]
    fn custom_root_with_transforms_is_supported() {
        let identity = identity("domains/assay/transforms/raw.sql");
        assert_eq!(identity.namespace.to_string(), "assay");
        assert_eq!(identity.path, vec!["raw"]);
        assert_eq!(identity.kind, RootKind::Custom);
    }

    #[test]
    fn top_level_file_uses_default_namespace() {
        assert_eq!(
            namespace_and_path("transforms/top.sql"),
            ("default".to_string(), vec!["top".to_string()])
        );
    }

    #[test]
    fn path_without_a_transform_root_is_unresolvable() {
        let error = derive_identity(
            Path::new("/does-not-exist"),
            Path::new("somewhere/random.sql"),
            &PhloConfig::default(),
        )
        .expect_err("no root");
        assert_eq!(error.code, codes::PROJECT_UNRESOLVABLE_PATH);
    }
}
