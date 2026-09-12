//! Git change-provider tests. Each case builds a real repository in a
//! tempdir: `git init`, a base commit, then the change under test.

use std::path::{Path, PathBuf};
use std::process::Command;

use phlo_transform_core::{compile, git_changes, load_project, GitChanges, GitError, PathStatus};

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write");
}

/// A two-model workspace: `assay.raw` reads a source, `assay.results`
/// reads `assay.raw`.
fn workspace(root: &Path) {
    write(root, "phlo.toml", "[transform]\nroots = [\"transforms\"]\n");
    write(
        root,
        "transforms/assay/raw.sql",
        "select * from raw.events\n",
    );
    write(
        root,
        "transforms/assay/results.sql",
        "select * from assay.raw\n",
    );
}

/// Commit the workspace on `main` and return the root.
fn repo() -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    workspace(&dir);
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "test@phlo.dev"]);
    git(&dir, &["config", "user.name", "Phlo Test"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    git(&dir, &["branch", "-M", "main"]);
    dir
}

fn changes(root: &Path, since: &str) -> GitChanges {
    let project = load_project(root).expect("workspace loads");
    let compilation = compile(&project);
    git_changes(root, &compilation, since).expect("git changes")
}

fn model_names(changes: &GitChanges) -> Vec<String> {
    changes
        .models
        .iter()
        .map(|model| model.model.clone())
        .collect()
}

#[test]
fn clean_repo_has_no_changes() {
    let root = repo();
    let changes = changes(&root, "main");
    assert!(changes.models.is_empty());
    assert!(changes.paths.is_empty(), "{:?}", changes.paths);
    assert_eq!(changes.since, "main");
    assert_eq!(changes.merge_base.len(), 40);
}

#[test]
fn modified_model_sql_is_a_direct_change() {
    let root = repo();
    write(
        &root,
        "transforms/assay/raw.sql",
        "select id from raw.events\n",
    );
    let changes = changes(&root, "main");
    assert_eq!(model_names(&changes), ["assay.raw"]);
    let causes = &changes.models[0].causes;
    assert_eq!(causes.len(), 1);
    assert_eq!(causes[0].path, "transforms/assay/raw.sql");
    assert!(causes[0].detail.contains("modified since main"));
}

#[test]
fn comment_only_sql_change_is_not_a_change() {
    let root = repo();
    write(
        &root,
        "transforms/assay/raw.sql",
        "-- just a comment\n\nselect  *\nfrom raw.events\n",
    );
    let changes = changes(&root, "main");
    assert!(changes.models.is_empty(), "{:?}", changes.models);
    // The path is still recorded — the diff saw it — but it maps to nothing.
    assert_eq!(
        changes
            .paths
            .iter()
            .map(|path| path.path.as_str())
            .collect::<Vec<_>>(),
        ["transforms/assay/raw.sql"]
    );
}

#[test]
fn directive_change_counts_as_a_semantic_change() {
    let root = repo();
    write(
        &root,
        "transforms/assay/raw.sql",
        "-- @materialization table\nselect * from raw.events\n",
    );
    let changes = changes(&root, "main");
    assert_eq!(model_names(&changes), ["assay.raw"]);
}

#[test]
fn untracked_model_is_a_direct_change() {
    let root = repo();
    write(
        &root,
        "transforms/assay/extra.sql",
        "select * from assay.raw\n",
    );
    let changes = changes(&root, "main");
    assert_eq!(model_names(&changes), ["assay.extra"]);
    assert_eq!(
        changes.models[0].causes[0].detail,
        "transforms/assay/extra.sql is untracked"
    );
    assert_eq!(changes.paths[0].status, PathStatus::Untracked);
}

#[test]
fn staged_add_is_a_direct_change() {
    let root = repo();
    write(
        &root,
        "transforms/assay/extra.sql",
        "select * from assay.raw\n",
    );
    git(&root, &["add", "transforms/assay/extra.sql"]);
    let changes = changes(&root, "main");
    assert_eq!(model_names(&changes), ["assay.extra"]);
    assert_eq!(changes.paths[0].status, PathStatus::Added);
}

#[test]
fn deleted_model_names_dependents() {
    let root = repo();
    std::fs::remove_file(root.join("transforms/assay/raw.sql")).expect("rm");
    let changes = changes(&root, "main");
    // `assay.results` still reads `assay.raw`, which no longer exists.
    assert_eq!(model_names(&changes), ["assay.results"]);
    assert!(changes.models[0].causes[0]
        .detail
        .contains("dependency assay.raw was removed"));
    assert_eq!(
        changes.deleted_models,
        vec![phlo_transform_core::DeletedModel {
            id: Some("assay.raw".to_string()),
            path: "transforms/assay/raw.sql".to_string(),
        }]
    );
}

#[test]
fn renamed_model_reports_old_and_new_identities() {
    let root = repo();
    git(
        &root,
        &[
            "mv",
            "transforms/assay/raw.sql",
            "transforms/assay/staging.sql",
        ],
    );
    let changes = changes(&root, "main");
    // `assay.staging` is the rename target; `assay.results` lost its
    // `assay.raw` dependency.
    assert_eq!(model_names(&changes), ["assay.results", "assay.staging"]);
    assert_eq!(changes.deleted_models[0].id.as_deref(), Some("assay.raw"));
}

#[test]
fn changed_seed_marks_consumers() {
    let root = repo();
    write(&root, "seeds/events.csv", "id\n1\n");
    let project = load_project(&root).expect("workspace loads");
    let compilation = compile(&project);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "with seed"]);
    write(&root, "seeds/events.csv", "id\n1\n2\n");
    let changes = git_changes(&root, &compilation, "HEAD~1").expect("git changes");
    assert_eq!(model_names(&changes), ["assay.raw"]);
    let seed = &changes.seeds[0];
    assert_eq!(seed.name, "events");
    assert_eq!(seed.consumers, ["assay.raw"]);
    assert!(seed.used);
}

#[test]
fn seed_rename_marks_old_and_new_consumers() {
    let root = repo();
    // `assay.orders` read raw.orders as an external source at the base.
    write(
        &root,
        "transforms/assay/orders.sql",
        "select * from raw.orders\n",
    );
    write(&root, "seeds/events.csv", "id\n1\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "orders consumer"]);
    // Identical bytes, different name: `events` disappears, `orders`
    // appears — not a no-op.
    git(&root, &["mv", "seeds/events.csv", "seeds/orders.csv"]);
    let changes = changes(&root, "main");
    assert_eq!(model_names(&changes), ["assay.orders", "assay.raw"]);
    let raw = &changes.models[1];
    assert_eq!(
        raw.causes[0].detail,
        "consumes seed events, renamed to orders since main"
    );
    // Both sides of the rename are reported.
    assert_eq!(changes.seeds.len(), 2, "{:?}", changes.seeds);
    let events = &changes.seeds[0];
    assert_eq!(events.name, "events");
    assert_eq!(events.status, PathStatus::Deleted);
    assert_eq!(events.consumers, ["assay.raw"]);
    let orders = &changes.seeds[1];
    assert_eq!(orders.name, "orders");
    assert_eq!(orders.status, PathStatus::Renamed);
    assert_eq!(orders.renamed_from.as_deref(), Some("events"));
    assert_eq!(orders.consumers, ["assay.orders"]);
}

#[test]
fn unused_seed_change_is_reported_not_dropped() {
    let root = repo();
    write(&root, "seeds/unused.csv", "id\n1\n");
    let changes = changes(&root, "main");
    // The seed maps to no model; it still surfaces in the change set.
    assert!(changes.models.is_empty());
    assert_eq!(changes.seeds[0].name, "unused");
    assert!(!changes.seeds[0].used);
    assert!(changes.seeds[0].consumers.is_empty());
}

#[test]
fn transform_toml_change_marks_its_subtree() {
    let root = repo();
    write(
        &root,
        "transforms/assay/transform.toml",
        "namespace = \"assay\"\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "root config"]);
    write(
        &root,
        "transforms/assay/transform.toml",
        "namespace = \"assay\"\nmaterialized = \"table\"\n",
    );
    let changes = changes(&root, "main");
    // Both models under transforms/assay are marked.
    assert_eq!(model_names(&changes), ["assay.raw", "assay.results"]);
}

#[test]
fn transform_toml_semantic_noop_marks_nothing() {
    let root = repo();
    write(
        &root,
        "transforms/assay/transform.toml",
        "namespace = \"assay\"\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "root config"]);
    // An unknown key parses away — the effective config is unchanged.
    write(
        &root,
        "transforms/assay/transform.toml",
        "namespace = \"assay\"\n\n[unused]\nx = 1\n",
    );
    let changes = changes(&root, "main");
    assert!(changes.models.is_empty(), "{:?}", changes.models);
}

#[test]
fn transform_toml_rename_marks_old_and_new_scopes() {
    let root = repo();
    write(
        &root,
        "transforms/assay/transform.toml",
        "namespace = \"assay\"\nmaterialized = \"table\"\n",
    );
    write(&root, "transforms/other/keep.sql", "select 1\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "scoped config"]);
    // Moving the config rewrites both scopes: `assay` loses it and
    // `other` gains it — the identical bytes are not a no-op.
    git(
        &root,
        &[
            "mv",
            "transforms/assay/transform.toml",
            "transforms/other/transform.toml",
        ],
    );
    let changes = changes(&root, "main");
    // `other.keep` compiles under the moved config's namespace.
    assert_eq!(
        model_names(&changes),
        ["assay.keep", "assay.raw", "assay.results"]
    );
}

#[test]
fn phlo_toml_model_section_scopes_the_change() {
    let root = repo();
    write(&root, "transforms/assay/other.sql", "select 1\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "third model"]);
    write(
        &root,
        "phlo.toml",
        "[transform]\nroots = [\"transforms\"]\n\n[model.assay_raw]\nmaterialization = \"table\"\n",
    );
    let changes = changes(&root, "main");
    // `model.assay_raw` applies to assay_raw and assay.raw — only raw moves.
    assert_eq!(model_names(&changes), ["assay.raw"]);
}

#[test]
fn phlo_toml_defaults_change_marks_everything() {
    let root = repo();
    write(
        &root,
        "phlo.toml",
        "[transform]\nroots = [\"transforms\"]\ndefault_materialization = \"table\"\n",
    );
    let changes = changes(&root, "main");
    assert_eq!(model_names(&changes), ["assay.raw", "assay.results"]);
}

#[test]
fn feature_branch_compares_against_merge_base() {
    let root = repo();
    // Feature branch changes raw; main then gains an unrelated file.
    git(&root, &["checkout", "-qb", "feature"]);
    write(
        &root,
        "transforms/assay/raw.sql",
        "select id from raw.events\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "feature"]);
    git(&root, &["checkout", "-q", "main"]);
    write(&root, "README.md", "docs\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "main moves on"]);
    git(&root, &["checkout", "-q", "feature"]);

    let changes = changes(&root, "main");
    // The README commit on main is NOT part of the diff — the merge base is
    // the fork point.
    assert_eq!(model_names(&changes), ["assay.raw"]);
    assert!(
        changes.paths.iter().all(|path| path.path != "README.md"),
        "{:?}",
        changes.paths
    );
}

#[test]
fn nested_workspace_strips_the_repo_prefix() {
    let root = repo();
    // Move the workspace one level down inside the repository.
    let nested = root.join("pkg");
    std::fs::create_dir_all(&nested).expect("mkdir pkg");
    for entry in std::fs::read_dir(&root).expect("read_dir") {
        let entry = entry.expect("entry");
        if entry.file_name() == ".git" || entry.file_name() == "pkg" {
            continue;
        }
        std::fs::rename(entry.path(), nested.join(entry.file_name())).expect("move");
    }
    write(&root, "outside.txt", "changed\n");
    write(
        &nested,
        "transforms/assay/raw.sql",
        "select id from raw.events\n",
    );
    git(&root, &["add", "-A"]);
    let changes = changes(&nested, "main");
    assert_eq!(model_names(&changes), ["assay.raw"]);
    assert_eq!(changes.workspace, "pkg");
    // The vacated repo-root paths are outside the (now nested) workspace.
    assert_eq!(
        changes.outside_workspace,
        ["outside.txt", "transforms/assay/raw.sql"]
    );
}

#[test]
fn unknown_ref_errors() {
    let root = repo();
    let project = load_project(&root).expect("workspace loads");
    let compilation = compile(&project);
    let error = git_changes(&root, &compilation, "no-such-ref").expect_err("unknown ref");
    assert!(matches!(error, GitError::UnknownRef(_)), "{error}");
}

#[test]
fn outside_a_repository_errors() {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    workspace(&dir);
    let project = load_project(&dir).expect("workspace loads");
    let compilation = compile(&project);
    let error = git_changes(&dir, &compilation, "main").expect_err("not a repo");
    assert!(matches!(error, GitError::NotARepository(_)), "{error}");
}

#[test]
fn ephemeral_models_are_direct_changes() {
    let root = repo();
    write(
        &root,
        "transforms/assay/helper.sql",
        "-- @materialization ephemeral\nselect * from raw.events\n",
    );
    write(
        &root,
        "transforms/assay/results.sql",
        "select * from assay.helper\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "with ephemeral"]);
    write(
        &root,
        "transforms/assay/helper.sql",
        "-- @materialization ephemeral\nselect id from raw.events\n",
    );
    let changes = changes(&root, "main");
    // The ephemeral itself is a direct Git change — unlike state-derived
    // `changed`, which never reports ephemerals. `changed+` expansion is
    // the selector engine's job.
    assert_eq!(model_names(&changes), ["assay.helper"]);
}

#[test]
fn multiple_paths_to_one_model_produce_ordered_causes() {
    let root = repo();
    write(
        &root,
        "transforms/assay/transform.toml",
        "namespace = \"assay\"\n",
    );
    write(&root, "seeds/events.csv", "id\n1\n");
    write(
        &root,
        "transforms/assay/raw.sql",
        "select * from raw.events\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "seed + toml"]);
    // Touch the toml, the seed and the file — raw collects three causes.
    write(
        &root,
        "transforms/assay/transform.toml",
        "namespace = \"assay\"\nmaterialized = \"table\"\n",
    );
    write(&root, "seeds/events.csv", "id\n1\n2\n");
    write(
        &root,
        "transforms/assay/raw.sql",
        "select id from raw.events\n",
    );
    let changes = changes(&root, "main");
    assert_eq!(model_names(&changes), ["assay.raw", "assay.results"]);
    let raw = &changes.models[0];
    assert_eq!(raw.model, "assay.raw");
    assert_eq!(raw.causes.len(), 3, "{:?}", raw.causes);
    // Deterministic: models sorted by name.
    assert_eq!(model_names(&changes), {
        let mut names = model_names(&changes);
        names.sort();
        names
    });
}
