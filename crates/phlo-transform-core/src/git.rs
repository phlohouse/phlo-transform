//! Git-aware change detection behind `--since <ref>`.
//!
//! One diff is resolved against the merge base of the requested ref and
//! HEAD — `merge-base(<since>, HEAD) .. working tree` — so feature branches
//! compare against where they diverged, and uncommitted working-tree changes
//! count like committed ones. Untracked files come from `git ls-files`.
//!
//! The resulting change set identifies *directly changed* models: models
//! whose own file changed semantically (canonical SQL or directives — pure
//! formatting/comment edits do not count), models affected by a changed
//! `transform.toml`/`phlo.toml`, consumers of changed seeds, targets of
//! changed tests, and models whose dependency was removed. Downstream
//! propagation is the selector engine's job (`changed+`), not this
//! provider's — this is deliberately different from the state-derived
//! `changed` set, where a moved upstream version marks dependents changed
//! all by itself.
//!
//! Git is driven through the CLI: a fixed handful of invocations
//! (rev-parse, merge-base, one diff, ls-files, one `cat-file --batch`),
//! never one command per model or per file.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use phlo_transform_sql::{parse_directives, parse_statements, Dialect, Directives};
use serde::Serialize;

use crate::compiled::Compilation;
use crate::config::{read_phlo_config, PhloConfig, TransformRootConfig};
use crate::discovery::model_id_for_path;
use crate::identity::ModelId;
use crate::select::SelectionCause;

/// The tree-ish used as the comparison base when the checkout has no HEAD
/// yet (a repository with no commits): Git's well-known empty tree.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// How a changed path entered the diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathStatus {
    /// Newly tracked or staged since the base.
    Added,
    /// Content differs from the base.
    Modified,
    /// Removed from the working tree relative to the base.
    Deleted,
    /// Renamed since the base (`old_path` carries the base path).
    Renamed,
    /// Present in the working tree but not tracked.
    Untracked,
}

impl std::fmt::Display for PathStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            PathStatus::Added => "added",
            PathStatus::Modified => "modified",
            PathStatus::Deleted => "deleted",
            PathStatus::Renamed => "renamed",
            PathStatus::Untracked => "untracked",
        };
        formatter.write_str(label)
    }
}

/// A path that differs between the merge base and the working tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangedPath {
    /// Workspace-relative path.
    pub path: String,
    pub status: PathStatus,
    /// The base-side path for renames — repository-relative, since it may
    /// legitimately lie outside the workspace (e.g. the workspace moved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

/// A directly changed model and the causes behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangedModel {
    /// Logical model name.
    pub model: String,
    pub causes: Vec<SelectionCause>,
}

/// A seed whose CSV changed, and the models that consume it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangedSeed {
    /// Seed name (CSV stem) as models reference it.
    pub name: String,
    /// Workspace-relative CSV path.
    pub path: String,
    pub status: PathStatus,
    /// Models reading the seed's source relation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<String>,
    /// False when no current model consumes the seed — the change is
    /// reported rather than silently dropped.
    pub used: bool,
}

/// A model file that no longer exists relative to the base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeletedModel {
    /// The identity the file produced, when it could be recovered
    /// (path-derived, or the `-- @id` pin recorded at the base).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub path: String,
}

/// A changed custom test and the models it targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangedTest {
    pub path: String,
    pub status: PathStatus,
    /// Models the test reads — selected so `test --since` covers them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
}

/// The Git-derived change set: what changed relative to a ref, and which
/// models it directly affects.
#[derive(Clone, Debug, Serialize)]
pub struct GitChanges {
    /// The `--since` argument as given.
    pub since: String,
    /// The merge-base commit the comparison ran against.
    pub merge_base: String,
    /// The HEAD commit, when the checkout has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The workspace root inside the repository (`""` at repo root).
    pub workspace: String,
    /// Every changed path inside the workspace.
    pub paths: Vec<ChangedPath>,
    /// Directly changed models and their causes.
    pub models: Vec<ChangedModel>,
    /// Changed or removed seeds and their consumers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub seeds: Vec<ChangedSeed>,
    /// Changed tests and the models they target.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<ChangedTest>,
    /// Model files that no longer exist.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deleted_models: Vec<DeletedModel>,
    /// Changed paths inside the workspace that map to nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unaffected: Vec<ChangedPath>,
    /// Changed paths outside the workspace root.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outside_workspace: Vec<String>,
}

impl GitChanges {
    /// The model ids feeding the `changed` selector.
    pub fn changed_model_ids(&self) -> BTreeSet<ModelId> {
        self.models
            .iter()
            .filter_map(|model| ModelId::parse(&model.model).ok())
            .collect()
    }

    /// Per-model provenance for the resolved [`crate::select::Selection`].
    pub fn selection_causes(&self) -> BTreeMap<String, Vec<SelectionCause>> {
        self.models
            .iter()
            .map(|model| (model.model.clone(), model.causes.clone()))
            .collect()
    }
}

/// Failures a `--since` comparison can produce.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    #[error("`--since` needs a Git repository: `{0}` is not inside one")]
    NotARepository(String),
    #[error("unknown Git ref `{0}`")]
    UnknownRef(String),
    #[error(
        "no merge base between `{since}` and HEAD — the clone may be shallow \
         or the histories unrelated; fetch more history and retry"
    )]
    NoMergeBase { since: String },
    #[error("{0}")]
    Command(String),
}

/// What a workspace-relative path is to the project.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PathKind {
    /// `phlo.toml` at the workspace root.
    PhloConfig,
    /// A `transform.toml` at any directory.
    RootConfig,
    /// `seeds/**/*.csv`.
    Seed,
    /// `tests/**/*.sql`.
    Test,
    /// A `.sql` file that could be a model.
    Model,
    /// `.phlo/**` — the tool's own artifacts; recorded in `paths` but never
    /// reported as unaffected noise.
    Internal,
    /// Anything else.
    Other,
}

fn classify(path: &str) -> PathKind {
    let name = path.rsplit('/').next().unwrap_or(path);
    if path == "phlo.toml" {
        PathKind::PhloConfig
    } else if path.starts_with(".phlo/") {
        PathKind::Internal
    } else if name == "transform.toml" {
        PathKind::RootConfig
    } else if path.starts_with("seeds/") && path.ends_with(".csv") {
        PathKind::Seed
    } else if path.starts_with("tests/") && path.ends_with(".sql") {
        PathKind::Test
    } else if path.ends_with(".sql") {
        PathKind::Model
    } else {
        PathKind::Other
    }
}

/// Compare the working tree against `merge-base(<since>, HEAD)` and map the
/// changed paths onto the compiled workspace.
pub fn changes(
    workspace_root: &Path,
    compilation: &Compilation,
    since: &str,
) -> Result<GitChanges, GitError> {
    let workspace = std::fs::canonicalize(workspace_root).map_err(|error| {
        GitError::Command(format!(
            "could not resolve workspace root `{}`: {error}",
            workspace_root.display()
        ))
    })?;

    let repo_text = git_stdout(&workspace, &["rev-parse", "--show-toplevel"])
        .ok_or_else(|| GitError::NotARepository(workspace.display().to_string()))?;
    let repo = std::fs::canonicalize(repo_text.trim())
        .map_err(|error| GitError::Command(format!("could not resolve repo root: {error}")))?;
    // The workspace may sit below the repository root; diff paths are
    // repo-relative and get relativised through this prefix.
    let prefix = workspace
        .strip_prefix(&repo)
        .unwrap_or(&workspace)
        .to_path_buf();

    let reference = git_stdout(
        &repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{since}^{{commit}}"),
        ],
    )
    .ok_or_else(|| GitError::UnknownRef(since.to_string()))?;
    let head = git_stdout(&repo, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .map(|head| head.trim().to_string());
    let merge_base = match &head {
        Some(head) => git_stdout(&repo, &["merge-base", reference.trim(), head.trim()])
            .map(|base| base.trim().to_string())
            .ok_or_else(|| GitError::NoMergeBase {
                since: since.to_string(),
            })?,
        // No commits yet: compare the empty tree against the worktree.
        None => EMPTY_TREE.to_string(),
    };

    let mut paths = diff_paths(&repo, &merge_base)?;
    paths.extend(untracked_paths(&repo)?);
    let mut seen = BTreeSet::new();
    paths.retain(|entry| seen.insert((entry.path.clone(), entry.status)));

    // Split repo-relative paths into workspace-relative and external.
    let mut inside: Vec<ChangedPath> = Vec::new();
    let mut outside: Vec<String> = Vec::new();
    for entry in paths {
        match relativize(&entry.path, &prefix) {
            Some(path) => inside.push(ChangedPath {
                path,
                status: entry.status,
                // Kept repo-relative: the base side of a rename may sit
                // outside the workspace.
                old_path: entry.old_path,
            }),
            None => outside.push(entry.path),
        }
    }
    inside.sort_by(|left, right| left.path.cmp(&right.path));
    outside.sort();

    // Fetch every needed base-side blob in one `cat-file --batch` round.
    let mut need: Vec<String> = Vec::new();
    for entry in &inside {
        let wants_old = match classify(&entry.path) {
            PathKind::Model | PathKind::Seed => matches!(
                entry.status,
                PathStatus::Modified | PathStatus::Deleted | PathStatus::Renamed
            ),
            PathKind::PhloConfig | PathKind::RootConfig => {
                matches!(entry.status, PathStatus::Modified | PathStatus::Renamed)
            }
            _ => false,
        };
        if wants_old {
            need.push(
                entry
                    .old_path
                    .clone()
                    .unwrap_or_else(|| join_repo(&prefix, &entry.path)),
            );
        }
    }
    let blobs = blob_contents(&repo, &merge_base, &need)?;

    let mut mapped = Mapper::new(compilation, since);
    for entry in inside {
        mapped.map(workspace_root, &prefix, &entry, &blobs);
    }
    Ok(mapped.finish(merge_base, head, &prefix, outside))
}

/// Accumulates the mapped change set.
struct Mapper<'a> {
    compilation: &'a Compilation,
    since: &'a str,
    paths: Vec<ChangedPath>,
    models: BTreeMap<String, Vec<SelectionCause>>,
    seeds: BTreeMap<String, ChangedSeed>,
    tests: Vec<ChangedTest>,
    deleted: Vec<DeletedModel>,
    unaffected: Vec<ChangedPath>,
    /// Model ids removed since the base, waiting for the dependent scan.
    removed_ids: Vec<String>,
}

impl<'a> Mapper<'a> {
    fn new(compilation: &'a Compilation, since: &'a str) -> Self {
        Self {
            compilation,
            since,
            paths: Vec::new(),
            models: BTreeMap::new(),
            seeds: BTreeMap::new(),
            tests: Vec::new(),
            deleted: Vec::new(),
            unaffected: Vec::new(),
            removed_ids: Vec::new(),
        }
    }

    fn finish(
        mut self,
        merge_base: String,
        head: Option<String>,
        prefix: &Path,
        outside: Vec<String>,
    ) -> GitChanges {
        // A removed model leaves its dependents reading a source where a
        // model used to be — mark them changed.
        let removed_ids = std::mem::take(&mut self.removed_ids);
        for removed in &removed_ids {
            for model in &self.compilation.models {
                let reads = model.source_dependencies().any(|source| {
                    let name = source.logical_name();
                    name == *removed || removed.ends_with(&format!(".{name}").as_str())
                });
                if reads {
                    self.cause(
                        model.id.logical_name(),
                        String::new(),
                        format!("dependency {removed} was removed since {}", self.since),
                    );
                }
            }
        }
        GitChanges {
            since: self.since.to_string(),
            merge_base,
            head,
            workspace: prefix.to_string_lossy().replace('\\', "/"),
            paths: self.paths,
            models: self
                .models
                .into_iter()
                .map(|(model, causes)| ChangedModel { model, causes })
                .collect(),
            seeds: self.seeds.into_values().collect(),
            tests: self.tests,
            deleted_models: self.deleted,
            unaffected: self.unaffected,
            outside_workspace: outside,
        }
    }

    fn cause(&mut self, model: String, path: String, detail: String) {
        self.models
            .entry(model)
            .or_default()
            .push(SelectionCause { path, detail });
    }

    fn map(
        &mut self,
        workspace_root: &Path,
        prefix: &Path,
        entry: &ChangedPath,
        blobs: &BTreeMap<String, Option<String>>,
    ) {
        self.paths.push(entry.clone());
        match classify(&entry.path) {
            PathKind::PhloConfig => self.map_phlo_config(workspace_root, entry, blobs, prefix),
            PathKind::RootConfig => self.map_root_config(workspace_root, entry, blobs, prefix),
            PathKind::Seed => self.map_seed(workspace_root, prefix, entry, blobs),
            PathKind::Test => self.map_test(entry),
            PathKind::Model => self.map_model(workspace_root, prefix, entry, blobs),
            PathKind::Internal => {}
            PathKind::Other => self.unaffected.push(entry.clone()),
        }
    }

    /// `phlo.toml` differs: narrow to the changed sections where possible,
    /// fall back to every model when the change is broad or unparseable.
    fn map_phlo_config(
        &mut self,
        workspace_root: &Path,
        entry: &ChangedPath,
        blobs: &BTreeMap<String, Option<String>>,
        prefix: &Path,
    ) {
        let since = self.since;
        let detail = |part: &str| format!("phlo.toml{part} changed since {since}");

        if matches!(
            entry.status,
            PathStatus::Added | PathStatus::Untracked | PathStatus::Deleted
        ) {
            self.cause_all(&entry.path, &detail(""));
            return;
        }

        let old = blobs
            .get(
                &entry
                    .old_path
                    .clone()
                    .unwrap_or_else(|| join_repo(prefix, &entry.path)),
            )
            .and_then(|content| content.as_deref())
            .and_then(|text| toml::from_str::<PhloConfig>(text).ok());
        let new = read_phlo_config(workspace_root).ok();
        let (Some(old), Some(new)) = (old, new) else {
            // Cannot compare — treat the whole file as changed.
            self.cause_all(&entry.path, &detail(""));
            return;
        };

        // Defaults, discovery globs and the dependency policy can move any
        // model's identity or version.
        if old.transform != new.transform || old.dependencies != new.dependencies {
            self.cause_all(&entry.path, &detail(""));
        }
        for key in union_keys(&old.model, &new.model) {
            if old.model.get(key) == new.model.get(key) {
                continue;
            }
            // `model.foo_bar` applies to both `foo_bar` and `foo.bar`.
            for candidate in [key.clone(), key.replace('_', ".")] {
                if let Ok(id) = ModelId::parse(&candidate) {
                    if self.compilation.model(&id).is_some() {
                        self.cause(
                            id.logical_name(),
                            entry.path.clone(),
                            detail(&format!(" [model.{key}]")),
                        );
                    }
                }
            }
        }
        let mut seed_names: BTreeSet<String> = BTreeSet::new();
        if old.seeds != new.seeds {
            seed_names.extend(self.compilation.seeds.iter().map(|seed| seed.name.clone()));
        }
        for key in union_keys(&old.seed, &new.seed) {
            if old.seed.get(key) != new.seed.get(key) {
                seed_names.insert(key.clone());
            }
        }
        for name in seed_names {
            self.mark_seed_consumers(
                &name,
                &entry.path,
                &format!("consumes seed {name}, whose config changed since {since}"),
            );
        }
    }

    /// A `transform.toml` configures the subtree rooted at its directory:
    /// any change there can move the identity or config of every model
    /// beneath it.
    fn map_root_config(
        &mut self,
        workspace_root: &Path,
        entry: &ChangedPath,
        blobs: &BTreeMap<String, Option<String>>,
        prefix: &Path,
    ) {
        let Some(dir) = entry.path.rsplit_once('/').map(|(dir, _)| dir) else {
            // A transform.toml at the workspace root is never consulted.
            self.unaffected.push(entry.clone());
            return;
        };
        let changed = if matches!(entry.status, PathStatus::Modified | PathStatus::Renamed) {
            let old = blobs
                .get(
                    &entry
                        .old_path
                        .clone()
                        .unwrap_or_else(|| join_repo(prefix, &entry.path)),
                )
                .and_then(|content| content.as_deref())
                .and_then(|text| toml::from_str::<TransformRootConfig>(text).ok());
            let new = std::fs::read_to_string(workspace_root.join(&entry.path))
                .ok()
                .and_then(|text| toml::from_str::<TransformRootConfig>(&text).ok());
            old != new
        } else {
            true
        };
        if !changed {
            return;
        }
        let under_dir = format!("{dir}/");
        for model in &self.compilation.models {
            let under = model
                .path_display()
                .map(|path| path.starts_with(&under_dir))
                .unwrap_or(false);
            if under {
                self.cause(
                    model.id.logical_name(),
                    entry.path.clone(),
                    format!("{} changed since {}", entry.path, self.since),
                );
            }
        }
    }

    /// A changed seed selects the models consuming its source relation.
    /// Byte-identical content (a pure move) is not a change. Matching is
    /// name-suffix based (`raw.events` consumes seed `events`), which is
    /// conservative when two relations share a stem.
    fn map_seed(
        &mut self,
        workspace_root: &Path,
        prefix: &Path,
        entry: &ChangedPath,
        blobs: &BTreeMap<String, Option<String>>,
    ) {
        if matches!(entry.status, PathStatus::Modified | PathStatus::Renamed) {
            let old = blobs
                .get(
                    &entry
                        .old_path
                        .clone()
                        .unwrap_or_else(|| join_repo(prefix, &entry.path)),
                )
                .and_then(|content| content.as_deref());
            let new = std::fs::read_to_string(workspace_root.join(&entry.path)).ok();
            if old.is_some() && old == new.as_deref() {
                return;
            }
        }
        let name = Path::new(&entry.path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        let consumers = self.seed_consumers(&name);
        for consumer in &consumers {
            let verb = if entry.status == PathStatus::Deleted {
                "was removed"
            } else {
                "changed"
            };
            self.cause(
                consumer.clone(),
                entry.path.clone(),
                format!("consumes seed {name}, which {verb} since {}", self.since),
            );
        }
        self.seeds.insert(
            name.clone(),
            ChangedSeed {
                name,
                path: entry.path.clone(),
                status: entry.status,
                used: !consumers.is_empty(),
                consumers,
            },
        );
    }

    /// A changed test selects the models it targets so `test --since`
    /// covers them; the version system keeps them a SKIP everywhere else.
    fn map_test(&mut self, entry: &ChangedPath) {
        let targets: Vec<String> = self
            .compilation
            .tests
            .iter()
            .find(|test| test.path_display().as_deref() == Some(entry.path.as_str()))
            .map(|test| {
                test.targets
                    .iter()
                    .map(|target| target.logical_name())
                    .collect()
            })
            .unwrap_or_default();
        for target in &targets {
            self.cause(
                target.clone(),
                entry.path.clone(),
                format!("test {} changed since {}", entry.path, self.since),
            );
        }
        self.tests.push(ChangedTest {
            path: entry.path.clone(),
            status: entry.status,
            targets,
        });
    }

    /// A `.sql` file under the discovery globs: a model in the current
    /// compilation, or a deleted/renamed-away model to name.
    fn map_model(
        &mut self,
        workspace_root: &Path,
        prefix: &Path,
        entry: &ChangedPath,
        blobs: &BTreeMap<String, Option<String>>,
    ) {
        let current = self
            .compilation
            .models
            .iter()
            .find(|model| model.path_display().as_deref() == Some(entry.path.as_str()));

        match entry.status {
            PathStatus::Added | PathStatus::Untracked => {
                if let Some(model) = current {
                    let detail = if entry.status == PathStatus::Added {
                        format!("{} added since {}", entry.path, self.since)
                    } else {
                        format!("{} is untracked", entry.path)
                    };
                    self.cause(model.id.logical_name(), entry.path.clone(), detail);
                } else {
                    self.unaffected.push(entry.clone());
                }
            }
            PathStatus::Modified => {
                let Some(model) = current else {
                    self.unaffected.push(entry.clone());
                    return;
                };
                let old = blobs
                    .get(&join_repo(prefix, &entry.path))
                    .and_then(|content| content.as_deref());
                let Some(old) = old else {
                    // No base blob — treat it as changed rather than guess.
                    self.cause(
                        model.id.logical_name(),
                        entry.path.clone(),
                        format!("{} modified since {}", entry.path, self.since),
                    );
                    return;
                };
                let (old_sql, old_directives) = semantic_signature(old);
                let (new_sql, new_directives) = semantic_signature(&model.sql);
                if old_sql != new_sql || old_directives != new_directives {
                    self.cause(
                        model.id.logical_name(),
                        entry.path.clone(),
                        format!("{} modified since {}", entry.path, self.since),
                    );
                }
                // A changed `-- @id` pin means the old identity vanished:
                // its dependents now read a source where a model was.
                let old_id = old_directives
                    .pinned_id
                    .as_deref()
                    .and_then(|pinned| ModelId::parse(pinned).ok())
                    .filter(|id| *id != model.id);
                if let Some(old_id) = old_id {
                    self.removed_ids.push(old_id.logical_name());
                    self.deleted.push(DeletedModel {
                        id: Some(old_id.logical_name()),
                        path: entry.path.clone(),
                    });
                }
            }
            PathStatus::Deleted | PathStatus::Renamed => {
                // A rename is a deletion at the old path plus a file at the
                // new one. The new side is a no-op when the model kept both
                // its identity and its semantics (e.g. the whole workspace
                // moved inside the repo); the old side only matters when
                // its identity actually went away.
                //
                // `entry.old_path` is repo-relative and may sit outside the
                // workspace; the workspace-relative reading falls back to
                // the repo path itself — right when the workspace moved and
                // the old path is already a valid relative path.
                let old_ws_path = entry
                    .old_path
                    .as_deref()
                    .map(|old| relativize(old, prefix).unwrap_or_else(|| old.to_string()))
                    .unwrap_or_else(|| entry.path.clone());
                let old = blobs
                    .get(
                        &entry
                            .old_path
                            .clone()
                            .unwrap_or_else(|| join_repo(prefix, &entry.path)),
                    )
                    .and_then(|content| content.as_deref());
                if entry.status == PathStatus::Renamed {
                    if let Some(model) = current {
                        let same_semantics = old
                            .map(|old| semantic_signature(old) == semantic_signature(&model.sql))
                            .unwrap_or(false);
                        let same_identity =
                            model_id_for_path(workspace_root, Path::new(&old_ws_path))
                                .map(|id| id == model.id)
                                .unwrap_or(false);
                        if !(same_semantics && same_identity) {
                            self.cause(
                                model.id.logical_name(),
                                entry.path.clone(),
                                format!(
                                    "{} renamed from {} since {}",
                                    entry.path, old_ws_path, self.since
                                ),
                            );
                        }
                    }
                }
                let pinned = old
                    .and_then(|text| parse_directives(text).pinned_id)
                    .and_then(|pinned| ModelId::parse(&pinned).ok());
                let id =
                    pinned.or_else(|| model_id_for_path(workspace_root, Path::new(&old_ws_path)));
                let still_exists = id
                    .as_ref()
                    .map(|id| self.compilation.model(id).is_some())
                    .unwrap_or(false);
                if !still_exists {
                    if let Some(id) = &id {
                        self.removed_ids.push(id.logical_name());
                    }
                    self.deleted.push(DeletedModel {
                        id: id.map(|id| id.logical_name()),
                        path: old_ws_path,
                    });
                }
            }
        }
    }

    /// Mark every model in the compilation as changed by `detail`.
    fn cause_all(&mut self, path: &str, detail: &str) {
        let ids: Vec<String> = self
            .compilation
            .models
            .iter()
            .map(|model| model.id.logical_name())
            .collect();
        for id in ids {
            self.cause(id, path.to_string(), detail.to_string());
        }
    }

    /// Mark every model consuming the named seed.
    fn mark_seed_consumers(&mut self, name: &str, path: &str, detail: &str) {
        for model in self.seed_consumers(name) {
            self.cause(model, path.to_string(), detail.to_string());
        }
    }

    /// Models reading a source relation named for the seed.
    fn seed_consumers(&self, name: &str) -> Vec<String> {
        self.compilation
            .models
            .iter()
            .filter(|model| {
                model.source_dependencies().any(|source| {
                    let source = source.logical_name();
                    source == name || source.ends_with(&format!(".{name}"))
                })
            })
            .map(|model| model.id.logical_name())
            .collect()
    }
}

fn union_keys<'a, V>(
    left: &'a BTreeMap<String, V>,
    right: &'a BTreeMap<String, V>,
) -> BTreeSet<&'a String> {
    left.keys().chain(right.keys()).collect()
}

/// Canonical semantics of a model file: the serialised AST plus directives.
/// Formatting and comments vanish; directive comments (`-- @...`) survive
/// through `parse_directives`.
fn semantic_signature(sql: &str) -> (String, Directives) {
    let canonical = parse_statements(sql, Dialect::Generic)
        .map(|statements| {
            statements
                .iter()
                .map(|statement| statement.to_string())
                .collect::<Vec<_>>()
                .join(";\n")
        })
        .unwrap_or_else(|_| sql.to_string());
    (canonical, parse_directives(sql))
}

/// Run `git` in `dir`; stdout on success, `None` on any failure.
fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| GitError::Command(format!("could not run git: {error}")))?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// `git diff --name-status -M -z <base>`: base vs working tree, tracked
/// files (staged and unstaged both count).
fn diff_paths(repo: &Path, base: &str) -> Result<Vec<ChangedPath>, GitError> {
    let output = git_bytes(repo, &["diff", "--name-status", "-M", "-z", base])?;
    let mut entries = Vec::new();
    let mut tokens = output
        .split(|byte| *byte == 0)
        .filter(|token| !token.is_empty());
    while let Some(token) = tokens.next() {
        let status = String::from_utf8_lossy(token).to_string();
        let next = |tokens: &mut dyn Iterator<Item = &[u8]>| {
            tokens
                .next()
                .map(|token| String::from_utf8_lossy(token).to_string())
        };
        match status.chars().next() {
            Some('A') => {
                if let Some(path) = next(&mut tokens) {
                    entries.push(ChangedPath {
                        path,
                        status: PathStatus::Added,
                        old_path: None,
                    });
                }
            }
            Some('D') => {
                if let Some(path) = next(&mut tokens) {
                    entries.push(ChangedPath {
                        path,
                        status: PathStatus::Deleted,
                        old_path: None,
                    });
                }
            }
            Some('R') | Some('C') => {
                if let (Some(old), Some(new)) = (next(&mut tokens), next(&mut tokens)) {
                    entries.push(ChangedPath {
                        path: new,
                        status: PathStatus::Renamed,
                        old_path: Some(old),
                    });
                }
            }
            // M, T, U and anything unexpected: content differs.
            _ => {
                if let Some(path) = next(&mut tokens) {
                    entries.push(ChangedPath {
                        path,
                        status: PathStatus::Modified,
                        old_path: None,
                    });
                }
            }
        }
    }
    Ok(entries)
}

/// `git ls-files --others --exclude-standard -z`: untracked files.
fn untracked_paths(repo: &Path) -> Result<Vec<ChangedPath>, GitError> {
    let output = git_bytes(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(output
        .split(|byte| *byte == 0)
        .filter(|token| !token.is_empty())
        .map(|token| ChangedPath {
            path: String::from_utf8_lossy(token).to_string(),
            status: PathStatus::Untracked,
            old_path: None,
        })
        .collect())
}

/// Fetch `<base>:<path>` contents for every repo-relative path in one
/// `git cat-file --batch` process. Missing entries come back `None`.
fn blob_contents(
    repo: &Path,
    base: &str,
    paths: &[String],
) -> Result<BTreeMap<String, Option<String>>, GitError> {
    let mut contents = BTreeMap::new();
    if paths.is_empty() {
        return Ok(contents);
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| GitError::Command(format!("could not run git: {error}")))?;

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        for path in paths {
            writeln!(stdin, "{base}:{path}")
                .map_err(|error| GitError::Command(format!("git cat-file failed: {error}")))?;
        }
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    for path in paths {
        let mut header = String::new();
        stdout
            .read_line(&mut header)
            .map_err(|error| GitError::Command(format!("git cat-file failed: {error}")))?;
        let header = header.trim_end().to_string();
        if header.ends_with(" missing") {
            contents.insert(path.clone(), None);
            continue;
        }
        let size = header
            .split_whitespace()
            .nth(2)
            .and_then(|size| size.parse::<usize>().ok())
            .ok_or_else(|| GitError::Command(format!("unexpected cat-file header `{header}`")))?;
        let mut buffer = vec![0u8; size + 1];
        stdout
            .read_exact(&mut buffer)
            .map_err(|error| GitError::Command(format!("git cat-file failed: {error}")))?;
        buffer.pop();
        contents.insert(
            path.clone(),
            Some(String::from_utf8_lossy(&buffer).to_string()),
        );
    }
    drop(stdout);
    let _ = child.wait();
    Ok(contents)
}

/// The workspace-relative form of a repo-relative path, or `None` when the
/// path lies outside the workspace.
fn relativize(path: &str, prefix: &Path) -> Option<String> {
    if prefix.as_os_str().is_empty() {
        return Some(path.to_string());
    }
    let prefix = prefix.to_string_lossy().replace('\\', "/");
    path.strip_prefix(&format!("{prefix}/")).map(str::to_string)
}

/// The repo-relative form of a workspace-relative path.
fn join_repo(prefix: &Path, path: &str) -> String {
    if prefix.as_os_str().is_empty() {
        return path.to_string();
    }
    format!("{}/{}", prefix.to_string_lossy().replace('\\', "/"), path)
}
