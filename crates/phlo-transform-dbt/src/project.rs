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

/// A parsed `*.yml`/`*.yaml` properties file.
#[derive(Clone, Debug)]
pub struct DbtPropertyFile {
    pub rel_path: PathBuf,
    pub root: Value,
}

/// Non-secret target fields read from `profiles.yml`, when present.
#[derive(Clone, Debug, Default)]
pub struct ProfileTarget {
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
    pub models: Vec<DbtSqlFile>,
    pub property_files: Vec<DbtPropertyFile>,
    pub singular_tests: Vec<DbtSqlFile>,
    /// Macro names → the file that defined them.
    pub macros: BTreeMap<String, PathBuf>,
    pub macro_files: Vec<DbtSqlFile>,
    pub seeds: Vec<PathBuf>,
    pub snapshots: Vec<DbtSqlFile>,
    pub analyses: Vec<DbtSqlFile>,
    pub packages: Vec<String>,
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
    let vars = project_yaml
        .get("vars")
        .and_then(Value::as_mapping)
        .cloned()
        .unwrap_or_default();
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
                project.seeds.push(file.clone());
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
    project.packages = read_packages(root, &mut warnings);
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
    let within = path.strip_prefix(root.join(dir)).unwrap_or(path);
    let mut dir_segments: Vec<String> = within
        .parent()
        .map(|parent| {
            parent
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    dir_segments.retain(|segment| !segment.is_empty());
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

fn read_packages(root: &Path, warnings: &mut Vec<String>) -> Vec<String> {
    for candidate in ["packages.yml", "dependencies.yml"] {
        let path = root.join(candidate);
        if !path.is_file() {
            continue;
        }
        let Some(file) = read_yaml(root, &path, warnings) else {
            continue;
        };
        return file
            .root
            .get("packages")
            .and_then(Value::as_sequence)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.get("package")
                            .or_else(|| item.get("git"))
                            .or_else(|| item.get("local"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    Vec::new()
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
        adapter_type: get_str(target, "type").map(str::to_string),
        database: get_str(target, "database").map(str::to_string),
        catalog: get_str(target, "catalog").map(str::to_string),
        schema: get_str(target, "schema").map(str::to_string),
    })
}

fn get_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
