//! dbt project discovery.
//!
//! Reads `dbt_project.yml`, model/test/macro/seed/snapshot directories and
//! `*.yml` property files into a loosely-typed in-memory project. Everything
//! dbt-shaped stays here; translation lives in [`crate::translate`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

/// A discovered `.sql` resource.
#[derive(Clone, Debug)]
pub struct DbtSqlFile {
    /// Path relative to the project root.
    pub rel_path: PathBuf,
    /// Path segments below the resource root (directories only).
    pub dir: Vec<String>,
    /// File stem, e.g. `stg_users`.
    pub stem: String,
    pub sql: String,
}

/// A discovered CSV seed file.
#[derive(Clone, Debug)]
pub struct DbtSeed {
    /// Absolute path to the CSV.
    pub path: PathBuf,
    /// Path relative to the project root.
    pub rel_path: PathBuf,
    /// Directory segments below the seed root (for `seeds:` tree config).
    pub dir: Vec<String>,
    /// File stem — the dbt seed name.
    pub name: String,
}

/// A parsed `*.yml`/`*.yaml` properties file.
#[derive(Clone, Debug)]
pub struct DbtPropertyFile {
    pub rel_path: PathBuf,
    pub root: Value,
}

/// How a dbt package dependency was declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageKind {
    /// dbt Hub: `package: org/name` with a `version:` range.
    Hub,
    /// `git: <url>` with an optional `revision:`.
    Git,
    /// `local: <path>` — resolved relative to the project root.
    Local,
    /// `projects:` entry in `dependencies.yml` — a dbt mesh cross-project
    /// dependency. Source is never installed under `dbt_packages/` by
    /// `dbt deps`, so it stays unresolved.
    Project,
    /// `tarball:` URL dependency — not resolved from disk.
    Tarball,
    /// Present under the package install path without a matching
    /// declaration — a transitive package or a stale install.
    Vendored,
}

/// A dbt package dependency plus its resolved on-disk source, when present.
///
/// Package source is loaded for static analysis only — macro definitions are
/// inlined through the same provable-subset machinery as project macros, and
/// no Jinja is ever executed. Nothing here creates a dbt package runtime.
#[derive(Clone, Debug)]
pub struct DbtPackage {
    /// The declared identifier: `org/name`, git URL, local path, or project
    /// name. For vendored packages, the install-path-relative directory.
    pub spec: String,
    pub kind: PackageKind,
    /// Macro namespace — the package's own `dbt_project.yml` `name:` when the
    /// source resolved, else derived from the spec.
    pub name: String,
    /// Declared version range (Hub) or revision (git), when present.
    pub requested: Option<String>,
    /// Exact resolved version/commit recorded in `package-lock.yml`.
    pub locked: Option<String>,
    /// Package source root, when available for static analysis.
    pub root: Option<PathBuf>,
    /// `.sql` files under the package's `macro-paths`.
    pub macro_files: Vec<DbtSqlFile>,
    /// Model file stems under the package's `model-paths`. Used only to make
    /// `ref('pkg', 'model')` diagnostics precise — package models are not
    /// translated.
    pub model_names: Vec<String>,
}

impl DbtPackage {
    fn declared(spec: &str, kind: PackageKind) -> Self {
        DbtPackage {
            spec: spec.to_string(),
            kind,
            name: spec_short_name(spec),
            requested: None,
            locked: None,
            root: None,
            macro_files: Vec::new(),
            model_names: Vec::new(),
        }
    }

    /// The package source root relative to the project root, for reports.
    pub fn display_root(&self, project_root: &Path) -> Option<String> {
        self.root
            .as_ref()
            .map(|root| display(root.strip_prefix(project_root).unwrap_or(root.as_path())))
    }
}

/// The package name derivable from a declaration: the final `org/name`
/// segment for Hub, the repository basename for git, the last path
/// component for `local:`. Normalised to a valid namespace (`-` → `_`).
pub(crate) fn spec_short_name(spec: &str) -> String {
    spec.trim_end_matches('/')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(spec)
        .trim_end_matches(".git")
        .replace('-', "_")
}

/// Non-secret target fields read from `profiles.yml`, when present.
#[derive(Clone, Debug, Default)]
pub struct ProfileTarget {
    /// The selected target's name (`target: dev` → `dev`); `target.name` in Jinja.
    pub name: Option<String>,
    /// Every declared output: name → schema. Used to evaluate
    /// `generate_schema_name` across all targets, not just the selected one.
    pub outputs: BTreeMap<String, Option<String>>,
    /// Every declared output: name → non-secret scalar fields
    /// (`type`, `schema`, `database`, ...). Used to evaluate `target.*`
    /// expressions statically; a field is only static when every declared
    /// output agrees on it.
    pub output_fields: BTreeMap<String, BTreeMap<String, String>>,
    pub adapter_type: Option<String>,
    pub database: Option<String>,
    pub catalog: Option<String>,
    pub schema: Option<String>,
}

/// A loaded dbt project.
#[derive(Clone, Debug)]
pub struct DbtProject {
    pub root: PathBuf,
    pub name: String,
    pub model_paths: Vec<PathBuf>,
    pub test_paths: Vec<PathBuf>,
    pub seed_paths: Vec<PathBuf>,
    pub macro_paths: Vec<PathBuf>,
    pub snapshot_paths: Vec<PathBuf>,
    pub analysis_paths: Vec<PathBuf>,
    /// Top-level `vars:` mapping (values kept as YAML).
    pub vars: Mapping,
    /// The `models:` hierarchy from `dbt_project.yml` (empty when absent).
    pub models_tree: Value,
    /// The `seeds:` hierarchy from `dbt_project.yml` (for `+schema` etc.).
    pub seeds_tree: Value,
    pub models: Vec<DbtSqlFile>,
    pub property_files: Vec<DbtPropertyFile>,
    pub singular_tests: Vec<DbtSqlFile>,
    /// Macro names → the file that defined them.
    pub macros: BTreeMap<String, PathBuf>,
    pub macro_files: Vec<DbtSqlFile>,
    pub seeds: Vec<DbtSeed>,
    pub snapshots: Vec<DbtSqlFile>,
    pub analyses: Vec<DbtSqlFile>,
    /// Declared and vendored package dependencies.
    pub packages: Vec<DbtPackage>,
    pub profile: Option<ProfileTarget>,
    /// Files that could not be read or parsed; surfaced as review issues.
    pub load_warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("`{0}` does not look like a dbt project (no dbt_project.yml)")]
    NotAProject(String),
    #[error("could not read `{0}`: {1}")]
    Read(String, String),
    #[error("invalid dbt_project.yml: {0}")]
    InvalidProjectFile(String),
}

/// Load a dbt project rooted at `root`.
pub fn load(root: &Path) -> Result<DbtProject, ProjectError> {
    let project_file = root.join("dbt_project.yml");
    if !project_file.is_file() {
        return Err(ProjectError::NotAProject(root.display().to_string()));
    }
    let text = std::fs::read_to_string(&project_file)
        .map_err(|error| ProjectError::Read("dbt_project.yml".into(), error.to_string()))?;
    let project_yaml: Value = serde_yaml::from_str(&text)
        .map_err(|error| ProjectError::InvalidProjectFile(error.to_string()))?;

    let name = get_str(&project_yaml, "name")
        .map(str::to_string)
        .unwrap_or_else(|| "dbt".to_string());
    let mut vars = project_yaml
        .get("vars")
        .and_then(Value::as_mapping)
        .cloned()
        .unwrap_or_default();
    // dbt resolves `var('x')` against the invoking node's package namespace
    // (`vars.<package>.x`) before the root `vars.x`. Every node we translate
    // belongs to this project, so the project-named block merges on top.
    // (YAML duplicate keys keep only the last block — a serde_yaml limit —
    // so a `vars:` that repeats the package name loses earlier blocks.)
    if let Some(namespaced) = vars
        .get(Value::String(name.clone()))
        .and_then(Value::as_mapping)
        .cloned()
    {
        for (key, value) in namespaced {
            vars.insert(key, value);
        }
    }
    let models_tree = project_yaml.get("models").cloned().unwrap_or(Value::Null);

    let model_paths = paths(&project_yaml, "model-paths", &["models"]);
    let test_paths = paths(&project_yaml, "test-paths", &["tests"]);
    let seed_paths = paths(&project_yaml, "seed-paths", &["data"]);
    let macro_paths = paths(&project_yaml, "macro-paths", &["macros"]);
    let snapshot_paths = paths(&project_yaml, "snapshot-paths", &["snapshots"]);
    let analysis_paths = paths(&project_yaml, "analysis-paths", &["analyses", "analysis"]);

    let mut project = DbtProject {
        root: root.to_path_buf(),
        name,
        model_paths: model_paths.clone(),
        test_paths: test_paths.clone(),
        seed_paths: seed_paths.clone(),
        macro_paths: macro_paths.clone(),
        snapshot_paths: snapshot_paths.clone(),
        analysis_paths: analysis_paths.clone(),
        vars,
        models_tree,
        seeds_tree: project_yaml.get("seeds").cloned().unwrap_or(Value::Null),
        models: Vec::new(),
        property_files: Vec::new(),
        singular_tests: Vec::new(),
        macros: BTreeMap::new(),
        macro_files: Vec::new(),
        seeds: Vec::new(),
        snapshots: Vec::new(),
        analyses: Vec::new(),
        packages: Vec::new(),
        profile: None,
        load_warnings: Vec::new(),
    };

    let mut warnings = Vec::new();
    for dir in &model_paths {
        for file in walk(root, dir, &mut warnings) {
            match extension(&file) {
                Some("sql") => {
                    if let Some(sql) = read_sql(root, &file, &mut warnings) {
                        project.models.push(sql_file(root, dir, &file, sql));
                    }
                }
                Some("yml" | "yaml") => {
                    if let Some(property) = read_yaml(root, &file, &mut warnings) {
                        project.property_files.push(property);
                    }
                }
                _ => {}
            }
        }
    }
    for dir in &test_paths {
        for file in walk(root, dir, &mut warnings) {
            if extension(&file) == Some("sql") {
                if let Some(sql) = read_sql(root, &file, &mut warnings) {
                    project.singular_tests.push(sql_file(root, dir, &file, sql));
                }
            }
        }
    }
    for dir in &macro_paths {
        for file in walk(root, dir, &mut warnings) {
            if extension(&file) == Some("sql") {
                if let Some(sql) = read_sql(root, &file, &mut warnings) {
                    for name in macro_names(&sql) {
                        project.macros.insert(name, file.clone());
                    }
                    project.macro_files.push(sql_file(root, dir, &file, sql));
                }
            }
        }
    }
    for dir in &seed_paths {
        for file in walk(root, dir, &mut warnings) {
            if extension(&file) == Some("csv") {
                project.seeds.push(DbtSeed {
                    rel_path: file.strip_prefix(root).unwrap_or(&file).to_path_buf(),
                    dir: dir_segments(root, dir, &file),
                    name: file
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or_default()
                        .to_string(),
                    path: file.clone(),
                });
            }
        }
    }
    for dir in &snapshot_paths {
        for file in walk(root, dir, &mut warnings) {
            if extension(&file) == Some("sql") {
                if let Some(sql) = read_sql(root, &file, &mut warnings) {
                    project.snapshots.push(sql_file(root, dir, &file, sql));
                }
            }
        }
    }
    for dir in &analysis_paths {
        for file in walk(root, dir, &mut warnings) {
            if extension(&file) == Some("sql") {
                if let Some(sql) = read_sql(root, &file, &mut warnings) {
                    project.analyses.push(sql_file(root, dir, &file, sql));
                }
            }
        }
    }
    project.packages = load_packages(root, &project_yaml, &mut warnings);
    let profile_name = get_str(&project_yaml, "profile").map(str::to_string);
    project.profile = read_profile(root, profile_name.as_deref());
    project.load_warnings = warnings;
    Ok(project)
}

fn paths(project: &Value, key: &str, defaults: &[&str]) -> Vec<PathBuf> {
    project
        .get(key)
        .and_then(Value::as_sequence)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .filter(|paths: &Vec<PathBuf>| !paths.is_empty())
        .unwrap_or_else(|| defaults.iter().map(PathBuf::from).collect())
}

/// Recursively list files under `root/dir`, sorted for determinism.
fn walk(root: &Path, dir: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let base = root.join(dir);
    if !base.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&base).follow_links(false) {
        match entry {
            Ok(entry) if entry.file_type().is_file() => files.push(entry.path().to_path_buf()),
            Ok(_) => {}
            Err(error) => warnings.push(format!("could not walk {}: {error}", base.display())),
        }
    }
    files.sort();
    files
}

fn extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|ext| ext.to_str())
}

fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn read_sql(_root: &Path, path: &Path, warnings: &mut Vec<String>) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(sql) => Some(sql),
        Err(error) => {
            warnings.push(format!("could not read {}: {error}", display(path)));
            None
        }
    }
}

fn read_yaml(root: &Path, path: &Path, warnings: &mut Vec<String>) -> Option<DbtPropertyFile> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            warnings.push(format!("could not read {}: {error}", display(path)));
            return None;
        }
    };
    match serde_yaml::from_str::<Value>(&text) {
        Ok(root_value) => Some(DbtPropertyFile {
            rel_path: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
            root: root_value,
        }),
        Err(error) => {
            warnings.push(format!("invalid YAML in {}: {error}", display(path)));
            None
        }
    }
}

fn sql_file(root: &Path, dir: &Path, path: &Path, sql: String) -> DbtSqlFile {
    let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    let dir_segments = dir_segments(root, dir, path);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    DbtSqlFile {
        rel_path,
        dir: dir_segments,
        stem,
        sql,
    }
}

/// Directory segments of `path` below `root/dir`.
fn dir_segments(root: &Path, dir: &Path, path: &Path) -> Vec<String> {
    let within = path.strip_prefix(root.join(dir)).unwrap_or(path);
    let mut segments: Vec<String> = within
        .parent()
        .map(|parent| {
            parent
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    segments.retain(|segment| !segment.is_empty());
    segments
}

/// Extract `{% macro name(` and `{% materialization name %}` definitions.
fn macro_names(sql: &str) -> Vec<String> {
    let mut names = Vec::new();
    for segment in crate::jinja::scan(sql) {
        if let crate::jinja::Segment::Stmt { inner, .. } = &segment {
            let rest = inner
                .strip_prefix("macro ")
                .or_else(|| inner.strip_prefix("materialization "));
            if let Some(rest) = rest {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
                    .collect();
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
    }
    names
}

/// A package directory found under the install path.
struct VendoredPackage {
    dir_name: String,
    /// The package's own `dbt_project.yml` `name:`, when readable.
    name: Option<String>,
    path: PathBuf,
}

/// A `package-lock.yml` entry: the declared spec and the exact resolution
/// dbt recorded (hub version or git commit).
struct LockedPackage {
    spec: String,
    name: Option<String>,
    resolution: String,
}

/// Resolve every declared and vendored package to on-disk source.
///
/// Resolution is read-only: `local:` entries resolve against the project
/// root; Hub and git entries match directories already installed under
/// `dbt_packages/` (or `packages-install-path:`/`deps/`). Nothing is
/// fetched and nothing is executed — the translator only ever *inspects*
/// package source, and an uninstalled package simply stays unresolved.
fn load_packages(root: &Path, project: &Value, warnings: &mut Vec<String>) -> Vec<DbtPackage> {
    let declared = declared_packages(root, warnings);
    let locked = read_package_lock(root, warnings);
    let vendored = vendored_packages(root, project, warnings);

    let mut matched = vec![false; vendored.len()];
    let mut packages = Vec::new();
    for mut package in declared {
        if packages
            .iter()
            .any(|seen: &DbtPackage| seen.spec == package.spec)
        {
            warnings.push(format!("duplicate package declaration `{}`", package.spec));
            continue;
        }
        package.locked = locked
            .iter()
            .find(|entry| {
                entry.spec == package.spec
                    || entry
                        .name
                        .as_deref()
                        .is_some_and(|name| name == package.name)
            })
            .map(|entry| entry.resolution.clone());
        package.root = match package.kind {
            PackageKind::Local => {
                let path = root.join(&package.spec);
                if path.join("dbt_project.yml").is_file() {
                    Some(path)
                } else {
                    warnings.push(format!(
                        "local package `{}` has no dbt_project.yml at {}",
                        package.spec,
                        display(&path)
                    ));
                    None
                }
            }
            PackageKind::Hub | PackageKind::Git => {
                match_vendored(&package, &vendored, &mut matched)
            }
            PackageKind::Project | PackageKind::Tarball | PackageKind::Vendored => None,
        };
        packages.push(package);
    }

    // Vendored directories no declaration claimed are still inspectable —
    // transitive installs land here.
    for (index, dir) in vendored.iter().enumerate() {
        if matched[index] {
            continue;
        }
        let mut package = DbtPackage::declared(&dir.dir_name, PackageKind::Vendored);
        package.spec = format!(
            "{}/{}",
            dir.path
                .parent()
                .and_then(|base| base.file_name())
                .map(|name| name.to_string_lossy())
                .unwrap_or_default(),
            dir.dir_name
        );
        if let Some(name) = &dir.name {
            package.name = name.clone();
        }
        package.root = Some(dir.path.clone());
        packages.push(package);
    }

    let mut namespaces = std::collections::BTreeSet::new();
    for package in &mut packages {
        load_package_source(package, root, warnings);
        if package.root.is_some() && !namespaces.insert(package.name.clone()) {
            warnings.push(format!(
                "multiple packages provide the `{}` macro namespace",
                package.name
            ));
        }
    }
    packages
}

/// `packages:` and `projects:` entries across `packages.yml` and
/// `dependencies.yml` (dbt reads both).
fn declared_packages(root: &Path, warnings: &mut Vec<String>) -> Vec<DbtPackage> {
    let mut packages = Vec::new();
    for candidate in ["packages.yml", "dependencies.yml"] {
        let path = root.join(candidate);
        if !path.is_file() {
            continue;
        }
        let Some(file) = read_yaml(root, &path, warnings) else {
            continue;
        };
        for item in file
            .root
            .get("packages")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
        {
            if let Some(package) = package_entry(item, warnings) {
                packages.push(package);
            }
        }
        for item in file
            .root
            .get("projects")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
        {
            // `projects: [{name: foo}]` or the bare `projects: [foo]`.
            if let Some(name) = item
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| item.as_str())
            {
                packages.push(DbtPackage::declared(name, PackageKind::Project));
            }
        }
    }
    packages
}

/// One `packages:` list entry: `- package: org/name`, `- git: <url>` or
/// `- local: <path>`.
fn package_entry(item: &Value, warnings: &mut Vec<String>) -> Option<DbtPackage> {
    let get = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
    if let Some(spec) = get("package") {
        let mut package = DbtPackage::declared(&spec, PackageKind::Hub);
        package.requested = item.get("version").and_then(package_version);
        return Some(package);
    }
    if let Some(spec) = get("git") {
        let mut package = DbtPackage::declared(&spec, PackageKind::Git);
        package.requested = item.get("revision").and_then(package_version);
        return Some(package);
    }
    if let Some(spec) = get("local") {
        return Some(DbtPackage::declared(&spec, PackageKind::Local));
    }
    if let Some(spec) = get("tarball") {
        return Some(DbtPackage::declared(&spec, PackageKind::Tarball));
    }
    warnings.push(format!("unrecognised package declaration `{item:?}`"));
    None
}

/// A `version:`/`revision:` field — a scalar or a list of range bounds.
fn package_version(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Sequence(items) => Some(
            items
                .iter()
                .filter_map(package_version)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        _ => None,
    }
}

/// Exact resolved versions/revisions from `package-lock.yml`.
fn read_package_lock(root: &Path, warnings: &mut Vec<String>) -> Vec<LockedPackage> {
    let path = root.join("package-lock.yml");
    if !path.is_file() {
        return Vec::new();
    }
    let mut locked = Vec::new();
    let Some(file) = read_yaml(root, &path, warnings) else {
        return locked;
    };
    for item in file
        .root
        .get("packages")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
    {
        let spec = item
            .get("package")
            .or_else(|| item.get("git"))
            .or_else(|| item.get("local"))
            .and_then(Value::as_str);
        let resolution = item
            .get("version")
            .or_else(|| item.get("revision"))
            .and_then(package_version);
        if let (Some(spec), Some(resolution)) = (spec, resolution) {
            locked.push(LockedPackage {
                spec: spec.to_string(),
                name: item.get("name").and_then(Value::as_str).map(str::to_string),
                resolution,
            });
        }
    }
    locked
}

/// Directories already installed under `packages-install-path` (default
/// `dbt_packages`) or `deps`, each with the package's own `name:`.
fn vendored_packages(
    root: &Path,
    project: &Value,
    warnings: &mut Vec<String>,
) -> Vec<VendoredPackage> {
    let install = get_str(project, "packages-install-path").unwrap_or("dbt_packages");
    let mut bases = vec![root.join(install)];
    let deps = root.join("deps");
    if deps != bases[0] {
        bases.push(deps);
    }
    let mut vendored = Vec::new();
    for base in bases {
        let mut paths: Vec<PathBuf> = match std::fs::read_dir(&base) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.is_dir())
                .collect(),
            Err(_) => continue,
        };
        paths.sort();
        for path in paths {
            let dir_name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            if dir_name.starts_with('.') {
                continue;
            }
            vendored.push(VendoredPackage {
                name: package_yaml(&path)
                    .as_ref()
                    .and_then(|yaml| get_str(yaml, "name"))
                    .map(str::to_string),
                dir_name,
                path,
            });
        }
    }
    let _ = warnings;
    vendored
}

/// Match a declared Hub/git package to an installed directory, by directory
/// name or by the package's own `name:`.
fn match_vendored(
    package: &DbtPackage,
    vendored: &[VendoredPackage],
    matched: &mut [bool],
) -> Option<PathBuf> {
    let wanted = spec_short_name(&package.spec);
    let position = vendored.iter().enumerate().find(|(index, dir)| {
        !matched[*index]
            && (names_match(&dir.dir_name, &wanted)
                || dir
                    .name
                    .as_deref()
                    .is_some_and(|name| names_match(name, &wanted)))
    });
    position.map(|(index, dir)| {
        matched[index] = true;
        dir.path.clone()
    })
}

fn names_match(left: &str, right: &str) -> bool {
    left == right || left.replace('-', "_") == right.replace('-', "_")
}

/// Parse a package's `dbt_project.yml`, when present.
fn package_yaml(package_root: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(package_root.join("dbt_project.yml")).ok()?;
    serde_yaml::from_str(&text).ok()
}

/// Load a resolved package's macro files and model stems for static
/// analysis.
fn load_package_source(package: &mut DbtPackage, project_root: &Path, warnings: &mut Vec<String>) {
    let Some(pkg_root) = package.root.clone() else {
        return;
    };
    let yaml = package_yaml(&pkg_root);
    if let Some(name) = yaml.as_ref().and_then(|yaml| get_str(yaml, "name")) {
        package.name = name.to_string();
    }
    let empty = Value::Null;
    let yaml = yaml.as_ref().unwrap_or(&empty);
    for dir in paths(yaml, "macro-paths", &["macros"]) {
        for file in walk(&pkg_root, &dir, warnings) {
            if extension(&file) != Some("sql") {
                continue;
            }
            if let Some(sql) = read_sql(&pkg_root, &file, warnings) {
                package.macro_files.push(DbtSqlFile {
                    rel_path: file
                        .strip_prefix(project_root)
                        .unwrap_or(&file)
                        .to_path_buf(),
                    dir: dir_segments(&pkg_root, &dir, &file),
                    stem: file
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or_default()
                        .to_string(),
                    sql,
                });
            }
        }
    }
    for dir in paths(yaml, "model-paths", &["models"]) {
        for file in walk(&pkg_root, &dir, warnings) {
            if extension(&file) == Some("sql") {
                package.model_names.push(
                    file.file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
    }
    package.model_names.sort();
    package.model_names.dedup();
}

/// Read non-secret target fields from a project-local `profiles.yml`.
fn read_profile(root: &Path, profile_name: Option<&str>) -> Option<ProfileTarget> {
    let path = root.join("profiles.yml");
    if !path.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let yaml: Value = serde_yaml::from_str(&text).ok()?;
    let entry = match profile_name {
        Some(name) => yaml.get(name),
        None => yaml.as_mapping().and_then(|map| map.values().next()),
    }?;
    let outputs = entry.get("outputs")?.as_mapping()?;
    let target_name = entry.get("target").and_then(Value::as_str);
    let target = match target_name {
        Some(name) => outputs.get(Value::String(name.to_string())),
        None => outputs.values().next(),
    }?;
    Some(ProfileTarget {
        // `target.name` is the selected target key (`target: dev` → `dev`),
        // or the single output's key when no selector is present.
        name: target_name.map(str::to_string).or_else(|| {
            outputs
                .keys()
                .next()
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        outputs: outputs
            .iter()
            .filter_map(|(name, output)| {
                name.as_str().map(|name| {
                    (
                        name.to_string(),
                        get_str(output, "schema").map(str::to_string),
                    )
                })
            })
            .collect(),
        output_fields: outputs
            .iter()
            .filter_map(|(name, output)| {
                name.as_str()
                    .map(|name| (name.to_string(), scalar_fields(output)))
            })
            .collect(),
        adapter_type: get_str(target, "type").map(str::to_string),
        database: get_str(target, "database").map(str::to_string),
        catalog: get_str(target, "catalog").map(str::to_string),
        schema: get_str(target, "schema").map(str::to_string),
    })
}

/// Non-secret scalar fields of one `outputs:` entry. Credentials and other
/// secret-bearing keys are never read.
fn scalar_fields(output: &Value) -> BTreeMap<String, String> {
    const SECRET: &[&str] = &["pass", "secret", "token", "key", "cred", "auth"];
    let mut fields = BTreeMap::new();
    let Some(map) = output.as_mapping() else {
        return fields;
    };
    for (key, value) in map {
        let Some(key) = key.as_str() else { continue };
        if SECRET.iter().any(|needle| key.contains(needle)) {
            continue;
        }
        let rendered = match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        };
        if let Some(rendered) = rendered {
            fields.insert(key.to_string(), rendered);
        }
    }
    fields
}

fn get_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
