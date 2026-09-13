//! `phlo-transform` command line interface.
//!
//! The CLI is a thin consumer of the compiler and engine. Every semantic
//! command supports `--json`; human and JSON output are derived from the same
//! report structures.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use phlo_transform_core::{
    compile, compile_with_options, git_changes, load_project, parse_selector, resolve_selection,
    CheckReport, Compilation, DataType, Diagnostic, GitChanges, InspectReport, ListReport, ModelId,
    Nullability, Relation, RelationSchema, SchemaColumn, Selection, SelectorKind, SelectorSet,
    StaticSchemaProvider,
};
use phlo_transform_daemon::{serve, spawn_watcher, WorkspaceService};
use phlo_transform_duckdb::DuckDbAdapter;
use phlo_transform_engine::{
    adapter_default_schema, branch_diff, changed_models, collect_source_states, diff, diff_reasons,
    ensure_environment, evaluate_gates, materialized_for_environment, model_keys, promote,
    relation_for_source, retarget, seeds_for_environment, Adapter, ArtifactWriter,
    BranchDiffReport, BranchDiffRequest, CancelHandle, CandidateProvenance, ContractSafety,
    DatasetKind, DiffPolicy, DiffRequest, DiffStrategy, EnvironmentArtifact, EnvironmentSetup,
    EnvironmentSpec, ExecutionStatus, GateInput, LineageDiffArtifact, LineageEnvironment,
    Membership, Plan, PlanAction, PlanOptions, PlanReason, Planner, PostgresStateStore,
    PromotionRequest, ReasonKind, RetryPolicy, RunOptions, RunResult, Runner, SqliteStateStore,
    StateStore, SCHEMA_VERSION,
};
use phlo_transform_nessie::{NessieClient, NessieConfig, NessieRestClient};
use phlo_transform_trino::{TrinoAdapter, TrinoConfig};

#[derive(Debug, Parser)]
#[command(
    name = "phlo-transform",
    version,
    about = "Workspace-native SQL transformation compiler"
)]
struct Cli {
    /// Workspace root (defaults to the current directory).
    #[arg(long, short = 'r', global = true, default_value = ".")]
    root: PathBuf,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    /// Select models by name, glob or selector term (repeatable).
    /// Terms: `model`, `model+`, `+model`, `+model+`, `tag:x`,
    /// `namespace:x`, `source:x`, `source:x+`, `changed`, `changed+`, `all`.
    #[arg(long, global = true)]
    select: Vec<String>,

    /// Exclude models matching a selector term (repeatable, applied last).
    #[arg(long, global = true)]
    exclude: Vec<String>,

    /// Include every transitive dependency of the selected models.
    #[arg(long, global = true)]
    upstream: bool,

    /// Include every transitive dependent of the selected models.
    #[arg(long, global = true)]
    downstream: bool,

    /// Select models carrying a tag.
    #[arg(long, global = true)]
    tag: Option<String>,

    /// Select models belonging to a workflow namespace.
    #[arg(long, global = true)]
    workflow: Option<String>,

    /// Select models whose desired version differs from recorded state.
    #[arg(long, global = true)]
    changed: bool,

    /// Compare the workspace against a Git ref (merge base through the
    /// working tree) and use that change set for the `changed` selector.
    /// Implies `changed` when no other include terms are given.
    #[arg(long, global = true)]
    since: Option<String>,

    /// Rebuild selected models regardless of recorded state (plan/apply/run).
    #[arg(long, global = true)]
    force: bool,

    /// Execution adapter: `trino` or `duckdb`. Defaults to `trino` when a
    /// Trino endpoint is configured.
    #[arg(long, global = true)]
    adapter: Option<String>,

    /// DuckDB database file for `--adapter duckdb`
    /// (default `.phlo/transform/local.duckdb`; `:memory:` for transient).
    #[arg(long, global = true)]
    duckdb_path: Option<String>,

    /// Trino endpoint, e.g. http://localhost:8080.
    #[arg(long, global = true)]
    trino_endpoint: Option<String>,

    #[arg(long, global = true)]
    trino_user: Option<String>,

    #[arg(long, global = true)]
    trino_password: Option<String>,

    #[arg(long, global = true)]
    trino_catalog: Option<String>,

    #[arg(long, global = true)]
    trino_schema: Option<String>,

    /// Maximum concurrent model builds (run/apply).
    #[arg(long, alias = "concurrency", global = true, default_value_t = 4)]
    jobs: usize,

    /// Extra attempts for transient adapter failures (run/apply).
    #[arg(long, global = true)]
    retries: Option<u32>,

    /// Stop scheduling new work on the first unrecoverable failure
    /// (run/apply).
    #[arg(long, global = true)]
    fail_fast: bool,

    /// Per-attempt execution timeout, e.g. `30m`, `90s`, `1h` (run/apply).
    #[arg(long, global = true, value_parser = parse_duration)]
    model_timeout: Option<Duration>,

    /// Continue an interrupted run, keeping its id and reusing successful
    /// work (run/apply). Selector flags do not apply to a resumed run.
    #[arg(long, global = true)]
    resume: Option<String>,

    /// Re-execute the failed/blocked portion of a finished run as a new run
    /// (run/apply). Selector flags do not apply.
    #[arg(long, global = true)]
    retry_failed: Option<String>,

    /// Environment label recorded in plans and run history.
    #[arg(long, global = true)]
    environment: Option<String>,

    /// Nessie reference (environment) for plan/apply.
    #[arg(long = "ref", visible_alias = "reference", global = true)]
    reference: Option<String>,

    /// Base Nessie reference a candidate is created from (default `main`);
    /// the candidate reference for `diff`/`promote`; the source format for
    /// `translate` (e.g. `--from dbt`).
    #[arg(long, global = true)]
    from: Option<String>,

    /// Iceberg warehouse for provisioned catalogs, e.g. `s3://bucket/wh`.
    #[arg(long, global = true)]
    warehouse: Option<String>,

    /// Physical catalog for model targets (overrides workspace config).
    #[arg(long, global = true)]
    catalog: Option<String>,

    /// State store location: a `postgres://`/`postgresql://` URL for a shared
    /// backend, or a filesystem path for a local SQLite database
    /// (default `.phlo/transform/state.db`; `PHLO_STATE_URL` also honoured).
    #[arg(long, global = true)]
    state: Option<String>,

    /// Nessie endpoint, e.g. http://localhost:19120.
    #[arg(long, global = true)]
    nessie_endpoint: Option<String>,

    /// Nessie bearer token.
    #[arg(long, global = true)]
    nessie_token: Option<String>,

    /// Enrich compilation with Trino catalogue schemas for external sources.
    #[arg(long, global = true)]
    catalogue: bool,

    #[command(subcommand)]
    command: Command,
}

/// Export formats for `lineage --format`.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum LineageFormat {
    /// Phlo's canonical lineage graph document.
    Graph,
    /// An OpenLineage document: a JSON array of spec-valid JobEvents and
    /// DatasetEvents — a valid batch-endpoint payload.
    Openlineage,
}

/// Nessie reference management (`phlo-transform ref ...`).
#[derive(Debug, Subcommand)]
enum RefAction {
    /// List every reference with its current hash.
    List,
    /// Resolve a reference to its current hash.
    Show {
        /// Reference name, e.g. `ci/pr-1`.
        name: String,
    },
    /// Create a branch from another reference (`--from`, default `main`).
    /// Mutates Nessie.
    Create {
        /// New branch name, e.g. `ci/pr-1`.
        name: String,
    },
    /// Delete a branch. Mutates Nessie.
    Delete {
        /// Branch name to delete.
        name: String,
    },
}

/// State-store inspection (`phlo-transform state ...`).
#[derive(Debug, Subcommand)]
enum StateAction {
    /// List recorded runs, newest first (`--environment` filters).
    Runs,
    /// Show one run: its record plus model, seed and test executions.
    /// Accepts a unique run-id prefix.
    Show {
        /// Run id or unique prefix.
        run: String,
    },
    /// Show the materialised version recorded for a model in the effective
    /// environment (`--environment`/`--ref`, else the default).
    Model {
        /// Model name (`assay.results`).
        model: String,
    },
    /// List recorded promotions, newest first.
    Promotions,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compile the workspace and report diagnostics.
    Check,
    /// List discovered models, sources and tests.
    List {
        /// Selector terms (see `--select`). Omit to list everything.
        selectors: Vec<String>,
    },
    /// Show details for a single model.
    Inspect {
        /// Model name (`assay.results`) or URI (`model://assay/results`).
        model: String,
    },
    /// Show the work that would be done, without mutating anything.
    Plan {
        /// Selector terms (see `--select`). Omit to plan everything.
        selectors: Vec<String>,
    },
    /// Execute the plan.
    Apply {
        /// Selector terms (see `--select`). Omit to plan everything.
        selectors: Vec<String>,
    },
    /// Convenience: plan + apply.
    Run {
        /// Selector terms (see `--select`). Omit to run everything.
        selectors: Vec<String>,
    },
    /// Run custom SQL tests against the current target.
    Test {
        /// Selector terms (see `--select`). Omit to test everything.
        selectors: Vec<String>,
    },
    /// Show upstream/downstream model lineage or a column's lineage.
    Lineage {
        /// Model (`assay.results`) or column (`assay.results.concentration`).
        /// Omit to print the model graph (optionally scoped by selectors).
        target: Option<String>,
        /// Export the canonical lineage graph instead of the human listing:
        /// `graph` emits Phlo's structured document, `openlineage` emits a
        /// JSON array of OpenLineage JobEvents and DatasetEvents.
        #[arg(long, value_enum)]
        format: Option<LineageFormat>,
        /// Diff lineage against the lineage compiled at a Git baseline.
        /// One ref — `lineage --diff main` — uses the same merge-base
        /// semantics as `--since`: the workspace is compared against
        /// `merge-base(ref, HEAD)`, so a feature branch diffs against where
        /// it diverged. Two refs — `lineage --diff main feature/foo` —
        /// compare the exact refs, no worktree involved. Writes
        /// `lineage_diff.json` alongside the other artifacts.
        #[arg(long, value_name = "BASE_REF [CANDIDATE_REF]", num_args = 1..=2, conflicts_with_all = ["target", "format"])]
        diff: Vec<String>,
    },
    /// Show downstream impact of a column or a selection.
    Impact {
        /// Column reference (`assay.results.concentration`) or model
        /// (`assay.results`). Omit and pass `--select` for the impact of a
        /// selection.
        column: Option<String>,
    },
    /// Manage Nessie references: list, show, create and delete branches.
    /// `create` and `delete` mutate Nessie; `list` and `show` are read-only.
    Ref {
        #[command(subcommand)]
        action: RefAction,
    },
    /// Promote an audited candidate Nessie reference to a target.
    Promote {
        /// Candidate reference, e.g. `ci/pr-1`. May also be given as `--from`.
        candidate: Option<String>,
        /// Target reference, e.g. `main`.
        #[arg(long)]
        to: String,
        /// Check preconditions without merging.
        #[arg(long)]
        check: bool,
        /// Require a passing data diff before promoting.
        #[arg(long)]
        require_diff: bool,
        /// Allow breaking schema changes (removed/incompatible columns).
        #[arg(long)]
        allow_breaking_schema: bool,
        /// Delete the candidate branch and drop its catalog after promoting.
        #[arg(long)]
        cleanup: bool,
    },
    /// Move a Nessie reference to a previous hash.
    Rollback {
        /// Target hash to restore.
        #[arg(long)]
        to: String,
    },
    /// Compare a model's candidate and base data, or — with no model —
    /// compare two references: `diff --from ci/pr-1 --to main`.
    Diff {
        /// Model name (`assay.results`). Omit for a branch-level diff.
        model: Option<String>,
        /// Base reference label (model diffs; defaults to the candidate).
        #[arg(long)]
        base: Option<String>,
        /// Base Nessie reference for a branch diff (`--to main`).
        #[arg(long)]
        to: Option<String>,
        /// Base physical relation (`catalog.schema.table`; model diffs).
        #[arg(long)]
        base_relation: Option<String>,
        /// Full comparison: keyed diff (model) / deep value diffs (branch).
        #[arg(long)]
        full: bool,
        /// Compare at partition granularity (comma-separated columns).
        #[arg(long)]
        partition: Option<String>,
        /// Deterministic sample fraction (0..1).
        #[arg(long)]
        sample: Option<f64>,
    },
    /// Translate a foreign project into a native Phlo workspace
    /// (`--from dbt`; `--from` is a global flag).
    Translate {
        /// Analysis only: print the migration report, write nothing.
        #[arg(long)]
        check: bool,
        /// Directory to write the translated workspace into.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Overwrite existing files in the output directory.
        #[arg(long)]
        overwrite: bool,
        /// Run `phlo-transform check` on the generated workspace afterwards.
        #[arg(long)]
        verify: bool,
    },
    /// Explain a model: identity, dependencies, planned action and reasons.
    Explain {
        /// Model name (`assay.results`) or URI (`model://assay/results`).
        model: String,
    },
    /// Inspect the state store: run history, one run's records, recorded
    /// materialisations and promotions. Read-only.
    State {
        #[command(subcommand)]
        action: StateAction,
    },
    /// Diagnose the workspace and execution environment.
    Doctor,
    /// Scaffold a new Phlo workspace at `--root`.
    Init,
    /// Print the last generated manifest artifact.
    Manifest,
    /// Run the local semantic service.
    Daemon {
        /// Local port to bind.
        #[arg(long, default_value_t = 7070)]
        port: u16,
        /// File-watch polling interval in milliseconds.
        #[arg(long, default_value_t = 500)]
        watch_interval_ms: u64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Rust ignores SIGPIPE by default, which turns `cmd | head` into a
    // panic on println!. Restore the default so a closed pipe exits quietly.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match run(&cli).await {
        Ok(code) => code,
        Err(error) => {
            if cli.json {
                let payload = serde_json::json!({ "ok": false, "error": error });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
                );
            } else {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: &Cli) -> Result<ExitCode, String> {
    // Commands that do not need a valid Phlo workspace at --root.
    match &cli.command {
        Command::Daemon {
            port,
            watch_interval_ms,
        } => return run_daemon(cli, *port, *watch_interval_ms).await,
        Command::Translate {
            check,
            out,
            overwrite,
            verify,
        } => return run_translate(cli, *check, out.as_ref(), *overwrite, *verify),
        Command::Init => return run_init(cli),
        Command::Doctor => return run_doctor(cli).await,
        Command::Manifest => return run_manifest(cli),
        Command::Ref { action } => return run_ref(cli, action).await,
        Command::State { action } => return run_state(cli, action),
        _ => {}
    }

    let mut project = match load_project(&cli.root) {
        Ok(project) => project,
        Err(diagnostics) => {
            if cli.json {
                print_json(&serde_json::json!({ "ok": false, "diagnostics": diagnostics }))?;
            } else {
                render_diagnostics(&diagnostics);
            }
            return Ok(ExitCode::FAILURE);
        }
    };

    let original_catalog = project.defaults.catalog.clone();
    let original_schema = project.defaults.schema.clone();

    // Parse selectors up front: errors surface before compilation, and the
    // `changed` term decides whether the compile needs source-state
    // enrichment even for commands that do not otherwise touch an adapter.
    let set = selector_set(cli)?;
    let wants_changed = set.uses_changed();

    let environment = match &cli.command {
        Command::Plan { .. } | Command::Apply { .. } | Command::Run { .. } => {
            provision_environment(cli).await?
        }
        _ => None,
    };
    if let Some(catalog) = cli
        .catalog
        .clone()
        .or_else(|| environment.as_ref().map(|setup| setup.catalog.clone()))
    {
        project.defaults.catalog = Some(catalog);
    }
    if let Some(setup) = &environment {
        write_environment_artifacts(cli, setup)?;
    }

    let compilation = {
        let base = compile(&project);
        if should_enrich(cli) || wants_changed {
            enrich(
                cli,
                &project,
                original_catalog.as_deref(),
                original_schema.as_deref(),
                &base,
            )
            .await
            .unwrap_or(base)
        } else {
            base
        }
    };

    // `--since` resolves the Git diff once against the compiled workspace;
    // every selection-consuming command then shares it.
    let git = match &cli.since {
        Some(since) => {
            Some(git_changes(&cli.root, &compilation, since).map_err(|error| error.to_string())?)
        }
        None => None,
    };

    match &cli.command {
        Command::Check => run_check(cli, &compilation),
        Command::List { .. } => run_list(cli, &compilation, &set, git.as_ref()),
        Command::Inspect { model } => run_inspect(cli, &compilation, model),
        Command::Plan { .. } => run_plan(cli, &compilation, &set, git.as_ref()).await,
        Command::Apply { .. } => run_apply(cli, &compilation, &set, git.as_ref(), false).await,
        Command::Run { .. } => run_apply(cli, &compilation, &set, git.as_ref(), true).await,
        Command::Test { .. } => run_test(cli, &compilation, &set, git.as_ref()).await,
        Command::Lineage {
            target,
            format,
            diff,
        } => {
            run_lineage(
                cli,
                &compilation,
                target.as_deref(),
                &set,
                git.as_ref(),
                *format,
                diff.as_slice(),
            )
            .await
        }
        Command::Impact { column } => {
            run_impact(cli, &compilation, column.as_deref(), &set, git.as_ref())
        }
        Command::Explain { model } => run_explain(cli, &compilation, model).await,
        Command::Translate { .. }
        | Command::Doctor
        | Command::Init
        | Command::Manifest
        | Command::Ref { .. }
        | Command::State { .. } => unreachable!("handled before workspace load"),
        Command::Promote {
            candidate,
            to,
            check,
            require_diff,
            allow_breaking_schema,
            cleanup,
        } => {
            run_promote(
                cli,
                &compilation,
                candidate.as_deref(),
                to,
                *check,
                *require_diff,
                *allow_breaking_schema,
                *cleanup,
            )
            .await
        }
        Command::Rollback { to } => run_rollback(cli, to).await,
        Command::Diff {
            model,
            base,
            to,
            base_relation,
            full,
            partition,
            sample,
        } => {
            run_diff(
                cli,
                &compilation,
                model.as_deref(),
                base,
                to,
                base_relation,
                *full,
                partition.as_deref(),
                *sample,
            )
            .await
        }
        Command::Daemon { .. } => Ok(ExitCode::SUCCESS),
    }
}

fn run_check(cli: &Cli, compilation: &Compilation) -> Result<ExitCode, String> {
    let report = compilation.check_report();
    if cli.json {
        print_json(&report)?;
    } else {
        print_check_human(&report);
        render_diagnostics(&report.diagnostics);
    }
    Ok(if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run_list(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    git: Option<&GitChanges>,
) -> Result<ExitCode, String> {
    let mut report = compilation.list_report();
    if !set.is_unrestricted() {
        let selection = resolve(cli, compilation, set, git)?;
        filter_list_report(compilation, &mut report, &selection);
    }
    if cli.json {
        print_json(&report)?;
    } else {
        print_list_human(&report);
    }
    Ok(ExitCode::SUCCESS)
}

/// Scope a list report to a resolved selection: the selected models, the
/// sources they read, the seeds backing those sources, and tests whose
/// targets are all in the selection.
fn filter_list_report(compilation: &Compilation, report: &mut ListReport, selection: &Selection) {
    let members: std::collections::BTreeSet<&str> = selection
        .members
        .iter()
        .map(|member| member.id.as_str())
        .collect();
    report
        .models
        .retain(|model| members.contains(model.name.as_str()));
    report.tests.retain(|test| {
        !test.targets.is_empty()
            && test
                .targets
                .iter()
                .all(|target| members.contains(target.as_str()))
    });
    let mut sources: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for member in &selection.members {
        if let Ok(id) = ModelId::parse(&member.id) {
            if let Some(model) = compilation.model(&id) {
                sources.extend(
                    model
                        .source_dependencies()
                        .map(|source| source.logical_name()),
                );
            }
        }
    }
    report
        .sources
        .retain(|source| sources.contains(source.name.as_str()));
    // A seed backs the source whose relation lands on its table.
    let seed_names: std::collections::BTreeSet<&str> = sources
        .iter()
        .filter_map(|source| source.rsplit('.').next())
        .collect();
    report
        .seeds
        .retain(|seed| seed_names.contains(seed.name.as_str()));
}

fn run_inspect(cli: &Cli, compilation: &Compilation, model: &str) -> Result<ExitCode, String> {
    let id = ModelId::parse(model)
        .map_err(|error| format!("invalid model reference `{model}`: {error}"))?;
    match compilation.inspect_report(&id) {
        Some(report) => {
            let diagnostics = model_diagnostics(compilation, &report);
            let desired = report.model.version.clone();
            let current = open_state(cli)?.and_then(|state| {
                state
                    .materialized_version(&id.logical_name(), environment(cli).as_deref())
                    .ok()
                    .flatten()
            });
            let status = match &current {
                None => "new",
                Some(record) if record.version.short() == desired => "unchanged",
                Some(_) => "changed",
            }
            .to_string();
            if cli.json {
                let mut value = serde_json::to_value(&report)
                    .map_err(|error| format!("could not serialise JSON: {error}"))?;
                value["state"] = serde_json::json!({
                    "desired": desired,
                    "current": current.as_ref().map(|record| record.version.hash.clone()),
                    "status": status,
                });
                value["diagnostics"] = serde_json::to_value(&diagnostics)
                    .map_err(|error| format!("could not serialise JSON: {error}"))?;
                print_json(&value)?;
            } else {
                print_inspect_human(&report);
                println!("State:");
                println!("  desired:  {desired}");
                println!(
                    "  current:  {}",
                    current
                        .as_ref()
                        .map(|record| record.version.short().to_string())
                        .unwrap_or_else(|| "(none)".to_string())
                );
                println!("  status:   {status}");
                println!();
                render_diagnostics(&diagnostics);
            }
            Ok(ExitCode::SUCCESS)
        }
        None => {
            let message = format!("no such model: {}", id.logical_name());
            if cli.json {
                print_json(&serde_json::json!({ "ok": false, "error": message }))?;
            } else {
                eprintln!("error: {message}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Diagnostics whose path is the model's source file.
fn model_diagnostics(compilation: &Compilation, report: &InspectReport) -> Vec<Diagnostic> {
    let Some(path) = report.model.path.as_deref() else {
        return Vec::new();
    };
    compilation
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.path.as_deref() == Some(path))
        .cloned()
        .collect()
}

/// The selector set for this invocation: positional selectors, `--select`
/// and `--changed` unioned as include terms; `--tag`/`--workflow` intersect;
/// `--exclude` subtracts last; `--upstream`/`--downstream` expand.
fn selector_set(cli: &Cli) -> Result<SelectorSet, String> {
    let positional: &[String] = match &cli.command {
        Command::List { selectors }
        | Command::Plan { selectors }
        | Command::Apply { selectors }
        | Command::Run { selectors }
        | Command::Test { selectors } => selectors,
        _ => &[],
    };
    // Selector flags are global so they can precede the subcommand, but they
    // are meaningless on commands that never resolve them — fail loudly
    // rather than silently ignoring them. (`lineage <target>` is exempt for
    // --upstream/--downstream, which double as direction filters.)
    let consumes = matches!(
        cli.command,
        Command::List { .. }
            | Command::Plan { .. }
            | Command::Apply { .. }
            | Command::Run { .. }
            | Command::Test { .. }
            | Command::Lineage { .. }
            | Command::Impact { .. }
    );
    if !consumes
        && (!positional.is_empty()
            || !cli.select.is_empty()
            || !cli.exclude.is_empty()
            || cli.tag.is_some()
            || cli.workflow.is_some()
            || cli.changed
            || cli.since.is_some()
            || cli.upstream
            || cli.downstream)
    {
        return Err(
            "selection flags apply to list, plan, apply, run, test, lineage and impact".to_string(),
        );
    }
    let mut include: Vec<String> = positional.to_vec();
    include.extend(cli.select.iter().cloned());
    if cli.changed {
        include.push("changed".to_string());
    }
    let mut filter = Vec::new();
    if let Some(tag) = &cli.tag {
        filter.push(format!("tag:{tag}"));
    }
    if let Some(workflow) = &cli.workflow {
        filter.push(format!("namespace:{workflow}"));
    }
    let mut set = SelectorSet::parse(
        &include,
        &cli.exclude,
        &filter,
        cli.upstream,
        cli.downstream,
    )
    .map_err(|error| error.to_string())?;

    if cli.since.is_some() {
        // `--since` supplies the change set for the `changed` selector. With
        // no include terms it means `--select changed`; an exclude-only
        // `changed` ("everything except what changed") is already coherent.
        let exclude_uses_changed = set
            .exclude
            .iter()
            .any(|term| term.kind == SelectorKind::Changed);
        if set.include.is_empty() && !exclude_uses_changed {
            set.include
                .push(parse_selector("changed").expect("`changed` parses"));
        }
        if !set.uses_changed() {
            return Err(
                "`--since` supplies the `changed` selector's change set — add a \
                 `changed` term (e.g. `--select changed+`) or drop the selection flags"
                    .to_string(),
            );
        }
    }
    Ok(set)
}

/// Resolve the selector set against the compilation. The `changed` term
/// compares desired versions against the recorded state for the effective
/// environment — or, when `--since` was given, against the Git-derived
/// change set, which also carries per-model provenance into the selection.
fn resolve(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    git: Option<&GitChanges>,
) -> Result<Selection, String> {
    let changed = if set.uses_changed() {
        match git {
            Some(git) => Some(git.changed_model_ids()),
            None => {
                let state = open_state(cli)?;
                Some(
                    changed_models(compilation, state.as_ref(), environment(cli).as_deref())
                        .map_err(|error| error.to_string())?,
                )
            }
        }
    } else {
        None
    };
    let mut selection =
        resolve_selection(compilation, set, changed.as_ref()).map_err(|error| error.to_string())?;
    if let Some(git) = git {
        selection.causes = git.selection_causes();
    }
    Ok(selection)
}

/// Resolve a single model reference through the selector engine, so bare
/// unique suffixes work the same way everywhere.
fn resolve_model(cli_arg: &str, compilation: &Compilation) -> Result<ModelId, String> {
    let set = SelectorSet::parse(&[cli_arg.to_string()], &[], &[], false, false)
        .map_err(|error| error.to_string())?;
    let selection =
        resolve_selection(compilation, &set, None).map_err(|error| error.to_string())?;
    let ids = selection.ids();
    match ids.len() {
        1 => Ok(ids.into_iter().next().expect("one id")),
        _ => Err(format!(
            "`{cli_arg}` matched {} models; explain needs exactly one",
            ids.len()
        )),
    }
}

async fn build_plan(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    state: Option<Arc<dyn StateStore>>,
    git: Option<&GitChanges>,
) -> Result<(Plan, ArtifactWriter), String> {
    let selection = resolve(cli, compilation, set, git)?;
    let adapter = build_adapter(cli)?;
    let planner = Planner::new(adapter, state);
    let mut plan = planner
        .plan(
            compilation,
            &selection,
            environment(cli),
            &PlanOptions { force: cli.force },
        )
        .await
        .map_err(|error| error.to_string())?;
    plan.git = git.cloned();
    Ok((plan, ArtifactWriter::for_workspace(&cli.root)))
}

/// Open the state store: an explicit `--state`/`PHLO_STATE_URL` location
/// selects a shared Postgres backend or a SQLite file path; otherwise the
/// default `.phlo/transform/state.db` is opened best-effort (a workspace
/// without state degrades to rebuild-everything, so its failure is silent).
/// An explicitly configured location must work — silently falling back to
/// local state would record runs where nobody looks for them.
fn open_state(cli: &Cli) -> Result<Option<Arc<dyn StateStore>>, String> {
    let location = cli
        .state
        .clone()
        .or_else(|| std::env::var("PHLO_STATE_URL").ok());
    match location.as_deref() {
        Some(url) if url.starts_with("postgres://") || url.starts_with("postgresql://") => {
            PostgresStateStore::connect(url)
                .map(|store| Some(Arc::new(store) as Arc<dyn StateStore>))
                .map_err(|error| {
                    format!(
                        "could not connect to state store {}: {error}",
                        state_location_display(url)
                    )
                })
        }
        Some(path) => SqliteStateStore::open(std::path::Path::new(path))
            .map(|store| Some(Arc::new(store) as Arc<dyn StateStore>))
            .map_err(|error| format!("could not open state store {path}: {error}")),
        None => Ok(SqliteStateStore::open(&state_path(cli))
            .ok()
            .map(|store| Arc::new(store) as Arc<dyn StateStore>)),
    }
}

/// Display form of a state location: credentials embedded in a Postgres URL
/// are stripped before the string can reach errors or doctor output.
fn state_location_display(location: &str) -> String {
    for scheme in ["postgres://", "postgresql://"] {
        if let Some(rest) = location.strip_prefix(scheme) {
            let host_part = rest.rsplit('@').next().unwrap_or(rest);
            return format!("{scheme}{host_part}");
        }
    }
    location.to_string()
}

/// The effective environment: `--environment`, else `--ref`.
fn environment(cli: &Cli) -> Option<String> {
    cli.environment.clone().or_else(|| cli.reference.clone())
}

/// Parse `--model-timeout` values: `30s`, `5m`, `1h`, or bare seconds.
fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (digits, unit_seconds) = if let Some(number) = value.strip_suffix('s') {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3600)
    } else {
        (value, 1)
    };
    let seconds: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration `{value}` — try `30s`, `5m` or `1h`"))?;
    Ok(Duration::from_secs(seconds.saturating_mul(unit_seconds)))
}

/// Run/apply execution options from the CLI flags.
fn run_options(cli: &Cli, cancel: CancelHandle) -> RunOptions {
    RunOptions {
        environment: environment(cli),
        concurrency: cli.jobs,
        run_tests: true,
        fail_fast: cli.fail_fast,
        retry: RetryPolicy {
            retries: cli.retries.unwrap_or(0),
            ..Default::default()
        },
        model_timeout: cli.model_timeout,
        cancel,
    }
}

fn nessie_endpoint(cli: &Cli) -> Option<String> {
    cli.nessie_endpoint
        .clone()
        .or_else(|| std::env::var("PHLO_NESSIE_ENDPOINT").ok())
}

/// Provision a candidate Nessie branch and its Trino catalog when `--ref`
/// names a candidate environment and a Nessie endpoint is configured.
async fn provision_environment(cli: &Cli) -> Result<Option<EnvironmentSetup>, String> {
    let Some(candidate) = cli.reference.clone() else {
        return Ok(None);
    };
    let base = cli.from.clone().unwrap_or_else(|| "main".to_string());
    if candidate == base {
        return Ok(None);
    }
    let Some(nessie_uri) = nessie_endpoint(cli) else {
        return Ok(None);
    };
    let nessie = build_nessie(cli)?;
    let adapter = build_adapter(cli)?;
    let catalog = cli
        .catalog
        .clone()
        .unwrap_or_else(|| catalog_name(&candidate));
    let spec = EnvironmentSpec {
        base_ref: base,
        candidate_ref: candidate,
        nessie_uri: Some(nessie_uri),
        warehouse: cli.warehouse.clone(),
        catalog,
    };
    let mut setup = ensure_environment(nessie.as_ref(), adapter.as_ref(), &spec)
        .await
        .map_err(|error| error.to_string())?;
    if setup.created_from.is_none() {
        // The branch pre-existed: never redefine its base as today's `base`.
        // Preserve whatever an earlier artifact recorded — if nothing did,
        // provenance is unknown and `promote` will refuse this candidate.
        setup.created_from =
            read_environment_for(cli, &setup.candidate.name).and_then(|prior| prior.created_from);
        if setup.created_from.is_none() && !cli.json {
            eprintln!(
                "warning: candidate branch `{}` already exists with unrecorded base \
                 provenance — `promote` will refuse it; recreate the branch with \
                 `ref delete` + `ref create` to record where it was cut from",
                setup.candidate.name
            );
        }
    }
    Ok(Some(setup))
}

fn catalog_name(reference: &str) -> String {
    let mut name = String::from("phlo_");
    let mut previous_underscore = false;
    for character in reference.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
            previous_underscore = false;
        } else if !previous_underscore {
            name.push('_');
            previous_underscore = true;
        }
    }
    name.trim_end_matches('_').to_string()
}

fn artifact_path(cli: &Cli, name: &str) -> PathBuf {
    ArtifactWriter::for_workspace(&cli.root)
        .directory()
        .join(name)
}

fn read_environment(cli: &Cli) -> Option<EnvironmentSetup> {
    let text = std::fs::read_to_string(artifact_path(cli, "environment.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(value.get("environment")?.clone()).ok()
}

/// The per-candidate environment artifact file: `environment_<ref>_<hash>.json`
/// with characters unsafe in a filename folded to `_`. Folding can collide
/// (`ci/pr-1` vs `ci_pr_1`) and a ref of nothing but unsafe characters
/// collapses to `environment` — the FNV-1a suffix keeps every ref's evidence
/// its own file, and stays stable across builds (unlike `DefaultHasher`).
fn environment_artifact_name(reference: &str) -> String {
    let mut name = String::from("environment_");
    let mut previous_underscore = true;
    for character in reference.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
            previous_underscore = false;
        } else if !previous_underscore {
            name.push('_');
            previous_underscore = true;
        }
    }
    let sanitized = name.trim_end_matches('_');
    let mut hash: u32 = 0x811c9dc5;
    for byte in reference.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    format!("{sanitized}_{hash:08x}.json")
}

/// Persist the provisioning record: the conventional `environment.json` for
/// the workspace's current environment, plus a per-candidate copy so one
/// candidate's evidence survives another being provisioned later.
fn write_environment_artifacts(cli: &Cli, setup: &EnvironmentSetup) -> Result<(), String> {
    ArtifactWriter::for_workspace(&cli.root)
        .write_environment(setup)
        .map_err(|error| error.to_string())?;
    let path = artifact_path(cli, &environment_artifact_name(&setup.candidate.name));
    let payload = serde_json::to_string_pretty(&EnvironmentArtifact {
        schema_version: SCHEMA_VERSION,
        environment: setup.clone(),
    })
    .map_err(|error| error.to_string())?;
    std::fs::write(path, payload).map_err(|error| error.to_string())
}

/// The recorded provisioning setup for a specific candidate: the
/// per-candidate artifact first, then the single-slot `environment.json`
/// (which only describes the most recently provisioned candidate).
fn read_environment_for(cli: &Cli, candidate: &str) -> Option<EnvironmentSetup> {
    let matches = |setup: &EnvironmentSetup| setup.candidate.name == candidate;
    let setup = std::fs::read_to_string(artifact_path(cli, &environment_artifact_name(candidate)))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| serde_json::from_value(value.get("environment")?.clone()).ok());
    setup
        .filter(matches)
        .or_else(|| read_environment(cli).filter(matches))
}

/// Drop a candidate's local provisioning evidence after its branch is gone.
fn remove_environment_artifacts(cli: &Cli, candidate: &str) {
    let _ = std::fs::remove_file(artifact_path(cli, &environment_artifact_name(candidate)));
    if read_environment(cli).is_some_and(|setup| setup.candidate.name == candidate) {
        let _ = std::fs::remove_file(artifact_path(cli, "environment.json"));
    }
}

fn read_diff(cli: &Cli) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(artifact_path(cli, "diff.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn build_nessie(cli: &Cli) -> Result<Arc<dyn NessieClient>, String> {
    let endpoint = cli
        .nessie_endpoint
        .clone()
        .or_else(|| std::env::var("PHLO_NESSIE_ENDPOINT").ok())
        .ok_or_else(|| {
            "no Nessie configured: pass --nessie-endpoint or set PHLO_NESSIE_ENDPOINT".to_string()
        })?;
    let mut config = NessieConfig::new(endpoint);
    config.token = cli
        .nessie_token
        .clone()
        .or_else(|| std::env::var("PHLO_NESSIE_TOKEN").ok());
    let client = NessieRestClient::new(config).map_err(|error| error.to_string())?;
    Ok(Arc::new(client))
}

#[allow(clippy::too_many_arguments)]
async fn run_promote(
    cli: &Cli,
    compilation: &Compilation,
    candidate: Option<&str>,
    to: &str,
    check: bool,
    require_diff: bool,
    allow_breaking_schema: bool,
    cleanup: bool,
) -> Result<ExitCode, String> {
    let candidate = match (candidate, cli.from.as_deref()) {
        (Some(positional), Some(from)) if positional != from => {
            return Err(format!(
                "candidate given twice and disagreeing: `{positional}` vs `--from {from}`"
            ));
        }
        (positional, from) => positional.or(from).ok_or_else(|| {
            "promote requires a candidate: `promote <ref> --to <target>` or `promote --from <ref> --to <target>`".to_string()
        })?,
    };
    let nessie = build_nessie(cli)?;
    let state = open_state(cli)?;

    // Resolve both references up front: promotion needs both to exist.
    let candidate_reference = nessie
        .get_reference(candidate)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("candidate reference `{candidate}` was not found"))?;
    let target = nessie
        .get_reference(to)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("target reference `{to}` was not found"))?;

    // Gather gate evidence: run history, the audited diff artifact, the
    // recorded provisioning base and a non-destructive merge check.
    let run = match &state {
        Some(state) => state
            .latest_run(Some(candidate))
            .map_err(|error| error.to_string())?,
        None => None,
    };
    let (model_runs, seed_runs, test_runs) = match (&state, &run) {
        (Some(state), Some(run)) => (
            state
                .model_runs(&run.run_id)
                .map_err(|error| error.to_string())?,
            state
                .seed_runs(&run.run_id)
                .map_err(|error| error.to_string())?,
            state
                .test_runs(&run.run_id)
                .map_err(|error| error.to_string())?,
        ),
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };

    let environment = read_environment_for(cli, candidate);
    let mut audit = audited_diff(
        cli,
        state.as_deref(),
        candidate,
        to,
        Some(&candidate_reference.hash),
        Some(&target.hash),
    );
    // Contract breaks are computed live — the workspace's desired contracts
    // against what the target environment last recorded — so a contract
    // edited after `diff` cannot sneak a break past the gate on a stale
    // artifact's analysis.
    audit
        .breaking_schema_changes
        .extend(contract_breaking_changes(
            state.as_deref(),
            compilation,
            to,
        )?);
    let merge_check = nessie.can_merge(candidate, to).await.ok();
    // Lineage evidence: the artifact only speaks for this promotion when
    // the identities it was produced against still hold — including the
    // candidate's compiled lineage fingerprint — stale or unbound reports
    // are surfaced as such, never silently as "no changes".
    let current_lineage_hash = compilation.lineage.fingerprint();
    let lineage = audited_lineage(
        cli,
        candidate,
        to,
        &candidate_reference.hash,
        &target.hash,
        Some(&current_lineage_hash),
    );

    // The `base` gate needs a target commit the evidence was established
    // against. Two sources, freshest first: a hash-bound diff artifact that
    // audited this exact target, else the recorded commit the candidate was
    // provably created from. A candidate whose origin is unknown and which
    // was never audited against this target has no evidence — the gate must
    // fail rather than redefine its base as today's head.
    let expected_target_hash = audit.audited_base_hash.clone().or_else(|| {
        environment
            .as_ref()
            .filter(|setup| setup.candidate.name == candidate && setup.base.name == to)
            .and_then(|setup| setup.created_from.as_ref())
            .map(|base| base.hash.clone())
    });

    let input = GateInput {
        run: run.clone(),
        model_runs,
        seed_runs,
        test_runs,
        require_diff,
        diff_passed: audit.diff_passed,
        diff_rejected: audit.diff_rejected.clone(),
        breaking_schema_changes: audit.breaking_schema_changes.clone(),
        allow_breaking_schema,
        expected_target_hash: expected_target_hash.clone(),
        actual_target_hash: Some(target.hash.clone()),
        actual_candidate_hash: Some(candidate_reference.hash.clone()),
        schema_audited: audit.schema_audited,
        merge_check,
    };
    let report = evaluate_gates(&input);

    if !report.passed || check {
        if cli.json {
            print_json(&serde_json::json!({
                "ok": report.passed,
                "candidate_ref": candidate,
                "target_ref": to,
                "check_only": check,
                "gates": report.results,
                "lineage": lineage,
            }))?;
        } else {
            print_gates_human(&report);
            print_lineage_evidence(&lineage);
            if check && report.passed {
                println!("Candidate `{candidate}` can be promoted to `{to}` (check only).");
            }
        }
        return Ok(if report.passed {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    let request = PromotionRequest {
        candidate_ref: candidate.to_string(),
        target_ref: to.to_string(),
        candidate_hash: Some(candidate_reference.hash.clone()),
        // Assert the target hash the gates were evaluated against: when no
        // provisioning base was recorded, pin the just-resolved hash so a
        // commit racing this promotion is rejected rather than merged over.
        expected_target_hash: expected_target_hash.or_else(|| Some(target.hash.clone())),
        plan_id: run.as_ref().map(|run| run.plan_id.clone()),
        run_id: run.as_ref().map(|run| run.run_id.clone()),
        quality_gates_passed: true,
        diff_passed: audit.diff_passed,
        require_diff,
        breaking_schema_changes: input.breaking_schema_changes,
        allow_breaking_schema,
        dry_run: false,
        actor: None,
        gates: report.results.clone(),
    };
    match promote(nessie.as_ref(), &request).await {
        Ok(record) => {
            ArtifactWriter::for_workspace(&cli.root)
                .write_promotion(&record)
                .map_err(|error| error.to_string())?;
            if let Some(state) = &state {
                state
                    .record_promotion(&record)
                    .map_err(|error| error.to_string())?;
            }
            // Cleanup is part of what the caller asked for; a failure is
            // reported and fails the command — the merge record already
            // persisted shows the promotion itself succeeded.
            let cleanup_error = if cleanup && record.merged {
                cleanup_candidate(cli, &nessie, candidate, environment.as_ref())
                    .await
                    .err()
            } else {
                None
            };
            if cli.json {
                print_json(&serde_json::json!({
                    "ok": cleanup_error.is_none(),
                    "gates": report.results,
                    "lineage": lineage,
                    "promotion": record,
                    "cleanup_error": cleanup_error,
                }))?;
            } else {
                print_gates_human(&report);
                print_lineage_evidence(&lineage);
                print_promotion_human(&record);
                if let Some(error) = &cleanup_error {
                    eprintln!(
                        "error: promotion merged but cleanup failed: {error}; \
                         remove the branch with `phlo-transform ref delete {candidate}`"
                    );
                }
            }
            Ok(if cleanup_error.is_some() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Err(error) => {
            let message = error.to_string();
            if cli.json {
                print_json(&serde_json::json!({
                    "ok": false,
                    "gates": report.results,
                    "error": message,
                }))?;
            } else {
                print_gates_human(&report);
                eprintln!("error: {message}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

/// What the persisted diff artifacts prove for this promotion.
#[derive(Clone, Debug, Default)]
struct AuditEvidence {
    /// The audited diff's verdict, when an applicable artifact was inspected.
    diff_passed: Option<bool>,
    /// Why the artifact cannot stand as evidence, when rejected.
    diff_rejected: Option<String>,
    /// Breaking schema changes the audit recorded.
    breaking_schema_changes: Vec<String>,
    /// The base commit the artifact audited — a second provenance source for
    /// the `base` gate when branch-cut provenance is unavailable.
    audited_base_hash: Option<String>,
    /// A fresh, ref-and-commit-bound audit actually inspected this pair.
    /// `false` means "no evidence", which must never read as "no changes".
    schema_audited: bool,
}

/// Live contract analysis for the promotion gate: the workspace's desired
/// contracts against the contracts the target environment last recorded.
/// Returns the breaking subset in the same `model.column: detail` shape as
/// physical schema breaks.
fn contract_breaking_changes(
    state: Option<&dyn StateStore>,
    compilation: &Compilation,
    to: &str,
) -> Result<Vec<String>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    // The recorded base contracts are promotion evidence — a store error
    // must fail promotion, never read as "no contracts recorded".
    let base = materialized_for_environment(state, to).map_err(|error| error.to_string())?;
    let mut breaking = Vec::new();
    for model in &compilation.models {
        let name = model.id.logical_name();
        let record = base.get(&name);
        let mut changes = phlo_transform_engine::contract_diff(
            record.and_then(|record| record.contract.as_ref()),
            model.contract.as_ref(),
        );
        // The effective key — incremental `key` columns and unique-assertion
        // columns collapse onto the same identity concept — is compared
        // against the key the target's materialisation persisted. Changing
        // or dropping it is breaking; a record that cannot prove its
        // historical key fails closed.
        if let Some(change) = phlo_transform_engine::key_change(
            &record
                .map(|record| record.recorded_key())
                .unwrap_or(phlo_transform_engine::RecordedKey::Known(None)),
            phlo_transform_engine::effective_key(model).as_deref(),
        ) {
            changes.push(change);
        }
        for change in changes {
            if change.safety == ContractSafety::Breaking {
                let subject = if change.column.is_empty() {
                    name.clone()
                } else {
                    format!("{name}.{}", change.column)
                };
                breaking.push(format!("{subject}: contract {}", change.detail));
            }
        }
    }
    Ok(breaking)
}

/// The lineage-diff artifact's standing as evidence for this promotion.
#[derive(Clone, Debug, serde::Serialize)]
struct LineageEvidence {
    /// `current` — the artifact audited this Nessie pair at these commits;
    /// `advisory` — only Git-bound, so no environment identity to check;
    /// `stale` — the identity it was produced for has moved and it cannot
    /// be treated as describing the candidate being promoted.
    status: &'static str,
    /// Why the artifact is not current evidence, when stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// The base the diff was produced against (`ref (commit)`).
    base: String,
    /// Total lineage changes the artifact reports.
    changes: usize,
}

/// Read `lineage_diff.json` and audit its provenance against the pair
/// being promoted. The artifact only counts when it was produced for this
/// candidate against this target — bound to the Nessie commits when it
/// carries an environment binding, else to the Git identity it recorded —
/// and only while its candidate fingerprint still matches the compiled
/// workspace. Anything stale, unreadable, or about another pair is
/// rejected with a reason naming the rerun rather than silently treated
/// as current.
fn audited_lineage(
    cli: &Cli,
    candidate: &str,
    to: &str,
    candidate_hash: &str,
    target_hash: &str,
    current_lineage_hash: Option<&str>,
) -> Option<LineageEvidence> {
    let text = std::fs::read_to_string(artifact_path(cli, "lineage_diff.json")).ok()?;
    let artifact = match serde_json::from_str::<LineageDiffArtifact>(&text) {
        Ok(artifact) => artifact,
        Err(error) => {
            return Some(LineageEvidence {
                status: "stale",
                reason: Some(format!("unreadable lineage artifact: {error}")),
                base: "unknown".to_string(),
                changes: 0,
            });
        }
    };
    let base = format!(
        "{} ({})",
        artifact.base_ref,
        short(&artifact.base_commit, 12)
    );
    let changes = artifact.diff.change_count();
    let stale = |reason: String| LineageEvidence {
        status: "stale",
        reason: Some(reason),
        base: base.clone(),
        changes,
    };
    let evidence = |status: &'static str| LineageEvidence {
        status,
        reason: None,
        base: base.clone(),
        changes,
    };
    let rerun = format!("rerun `lineage --diff {to} --ref {candidate}`");
    let verdict = match &artifact.environment {
        // Nessie-bound: the pair and both commits must match the promotion.
        Some(binding) => {
            if binding.candidate_ref != candidate || binding.target_ref != to {
                stale(format!(
                    "lineage diff covers `{}` -> `{}`, not `{candidate}` -> `{to}`; {rerun}",
                    binding.candidate_ref, binding.target_ref
                ))
            } else if binding.candidate_hash != candidate_hash {
                stale(format!(
                    "candidate `{candidate}` moved since the lineage diff; {rerun}"
                ))
            } else if binding.target_hash != target_hash {
                stale(format!(
                    "target `{to}` moved since the lineage diff; {rerun}"
                ))
            } else {
                evidence("current")
            }
        }
        // Git-bound only: the recorded candidate identity must still hold.
        None => match artifact.base_kind.as_str() {
            "merge-base" => {
                match phlo_transform_core::comparison_base(&cli.root, &artifact.base_ref) {
                    Ok(current)
                        if current.commit == artifact.base_commit
                            && current.head == artifact.candidate.head
                            && current.dirty == artifact.candidate.dirty =>
                    {
                        evidence("advisory")
                    }
                    Ok(_) => stale(format!(
                        "the worktree or `{}` moved since the lineage diff; {rerun}",
                        artifact.base_ref
                    )),
                    Err(_) => stale(format!(
                        "base ref `{}` no longer resolves; {rerun}",
                        artifact.base_ref
                    )),
                }
            }
            // Exact ref -> ref: both refs must still resolve to the commits
            // the diff was produced from — a deleted or unrecorded ref is
            // unverifiable, not "unchanged".
            _ => {
                let base_now =
                    match phlo_transform_core::resolve_commit(&cli.root, &artifact.base_ref) {
                        Ok(commit) => commit,
                        Err(_) => {
                            return Some(stale(format!(
                                "base ref `{}` no longer resolves; {rerun}",
                                artifact.base_ref
                            )))
                        }
                    };
                let candidate_now = match artifact.candidate.git_ref.as_deref() {
                    Some(git_ref) => {
                        match phlo_transform_core::resolve_commit(&cli.root, git_ref) {
                            Ok(commit) => Some(commit),
                            Err(_) => {
                                return Some(stale(format!(
                                    "candidate ref `{git_ref}` no longer resolves; {rerun}"
                                )))
                            }
                        }
                    }
                    None => None,
                };
                match (candidate_now, artifact.candidate.head.as_deref()) {
                    (Some(now), Some(recorded))
                        if now == recorded && base_now == artifact.base_commit =>
                    {
                        evidence("advisory")
                    }
                    (Some(_), Some(_)) => stale(format!(
                        "a diffed ref moved since the lineage diff; {rerun}"
                    )),
                    _ => stale(format!(
                        "the artifact does not record a resolvable candidate ref; {rerun}"
                    )),
                }
            }
        },
    };
    // Whichever identity check passed, the artifact must also describe the
    // candidate's current definitions: refs can sit still while edited
    // code waits unpromoted, and a dirty worktree stays "dirty" as its
    // contents change.
    if verdict.status == "stale" {
        return Some(verdict);
    }
    Some(
        match (&artifact.candidate.lineage_hash, current_lineage_hash) {
            (Some(recorded), Some(now)) if recorded == now => verdict,
            (Some(_), Some(_)) => stale(format!(
                "the candidate's lineage changed since the diff; {rerun}"
            )),
            (Some(_), None) => stale(format!(
                "the candidate's lineage could not be fingerprinted; {rerun}"
            )),
            (None, _) => stale(format!(
                "the lineage artifact predates candidate fingerprinting; {rerun}"
            )),
        },
    )
}

/// Read the audited `branch_diff.json` artifact and derive the evidence it
/// carries for this
/// promotion: the diff verdict, why the artifact cannot be used, the breaking
/// schema changes it recorded, and whether a schema audit genuinely ran.
///
/// The artifact only counts when it was produced for this candidate against
/// this target at the commits being promoted — a diff of another pair, of an
/// older head, or one that went stale since is rejected with a reason naming
/// the rerun. `candidate_hash`/`base_hash` are the refs' current heads; a
/// hash-bound artifact must match them exactly.
fn audited_diff(
    cli: &Cli,
    state: Option<&dyn StateStore>,
    candidate: &str,
    to: &str,
    candidate_hash: Option<&str>,
    base_hash: Option<&str>,
) -> AuditEvidence {
    let Some(state) = state else {
        return AuditEvidence::default();
    };
    if let Some(report) = read_branch_diff(cli) {
        // An audit of another candidate, or against another target, is not
        // evidence for this promotion.
        if report.candidate_ref != candidate || report.base_ref != to {
            return AuditEvidence {
                diff_rejected: Some(format!(
                    "branch diff covers `{}` -> `{}`, not `{candidate}` -> `{to}`; \
                     rerun `diff --from {candidate} --to {to} --full`",
                    report.candidate_ref, report.base_ref
                )),
                ..AuditEvidence::default()
            };
        }
        let mut rejected = None;
        let mut reject = |reason: String| {
            rejected.get_or_insert(reason);
        };
        // Whether the artifact inspected this pair at these commits and still
        // applies — binding and freshness failures revoke the schema audit;
        // shallowness does not (the schema pass ran either way).
        let mut fresh = true;
        let mut stale = |reason: String| {
            fresh = false;
            reject(reason);
        };
        let mut breaking = Vec::new();
        for change in &report.schema_changes {
            for item in &change.changes {
                if matches!(item.safety.as_str(), "error" | "full_rebuild_required") {
                    breaking.push(format!("{}.{}: {}", change.model, item.column, item.detail));
                }
            }
        }
        // Commit binding: the artifact must name the exact heads being
        // promoted. An unbound artifact cannot prove what it audited.
        for (label, recorded, expected) in [
            (
                "candidate",
                report.candidate_hash.as_deref(),
                candidate_hash,
            ),
            ("base", report.base_hash.as_deref(), base_hash),
        ] {
            let Some(expected) = expected else { continue };
            match recorded {
                Some(recorded) if recorded == expected => {}
                Some(recorded) => stale(format!(
                    "branch diff audited {label}@{recorded}, not current {label}@{expected}; \
                     rerun `diff --from {candidate} --to {to} --full`"
                )),
                None => stale(format!(
                    "branch diff does not record the {label} commit it audited; \
                     rerun `diff --from {candidate} --to {to} --full`"
                )),
            }
        }
        // The artifact is stale when a dataset's recorded version no longer
        // matches the current materialisation — on either side — or when a
        // dataset that had no materialisation at diff time has one now (it
        // stopped being `removed`/`absent` since the audit). `main` folds in
        // the default environment, matching how the diff was produced.
        // State reads are promotion evidence: a store error cannot masquerade
        // as "nothing recorded" — the artifact is rejected rather than
        // trusted against an empty map.
        let (candidate_models, candidate_seeds) = match (
            materialized_for_environment(state, candidate),
            seeds_for_environment(state, candidate),
        ) {
            (Ok(models), Ok(seeds)) => (models, seeds),
            (Err(error), _) | (_, Err(error)) => {
                stale(format!("cannot confirm the diff is current: {error}"));
                (BTreeMap::new(), BTreeMap::new())
            }
        };
        let (base_models, base_seeds) = match (
            materialized_for_environment(state, to),
            seeds_for_environment(state, to),
        ) {
            (Ok(models), Ok(seeds)) => (models, seeds),
            (Err(error), _) | (_, Err(error)) => {
                stale(format!("cannot confirm the diff is current: {error}"));
                (BTreeMap::new(), BTreeMap::new())
            }
        };
        for dataset in &report.datasets {
            let (current_candidate, current_base) = match dataset.kind {
                DatasetKind::Model => (
                    candidate_models
                        .get(&dataset.dataset)
                        .map(|record| record.version.hash.clone()),
                    base_models
                        .get(&dataset.dataset)
                        .map(|record| record.version.hash.clone()),
                ),
                DatasetKind::Seed => (
                    candidate_seeds
                        .get(&dataset.dataset)
                        .map(|record| record.content_hash.clone()),
                    base_seeds
                        .get(&dataset.dataset)
                        .map(|record| record.content_hash.clone()),
                ),
            };
            if current_candidate != dataset.candidate_version {
                stale(format!(
                    "branch diff is stale: `{}` changed on the candidate since the diff",
                    dataset.dataset
                ));
            }
            if current_base != dataset.base_version {
                stale(format!(
                    "branch diff is stale: `{}` changed on `{to}` since the diff",
                    dataset.dataset
                ));
            }
        }
        // A dataset materialised after the diff was never compared — the
        // report cannot speak for it.
        let covered: std::collections::BTreeSet<(&str, DatasetKind)> = report
            .datasets
            .iter()
            .map(|dataset| (dataset.dataset.as_str(), dataset.kind))
            .collect();
        for (name, kind) in candidate_models
            .keys()
            .map(|name| (name, DatasetKind::Model))
            .chain(candidate_seeds.keys().map(|name| (name, DatasetKind::Seed)))
        {
            if !covered.contains(&(name.as_str(), kind)) {
                stale(format!(
                    "branch diff is stale: `{name}` materialised on the candidate after the diff"
                ));
            }
        }
        for (name, kind) in base_models
            .keys()
            .map(|name| (name, DatasetKind::Model))
            .chain(base_seeds.keys().map(|name| (name, DatasetKind::Seed)))
        {
            if !covered.contains(&(name.as_str(), kind)) {
                stale(format!(
                    "branch diff is stale: `{name}` materialised on `{to}` after the diff"
                ));
            }
        }
        // A diff entry that compared a relation to itself measured nothing —
        // it cannot back a required audit.
        if report
            .diffs
            .iter()
            .any(|diff| diff.candidate_relation == diff.base_relation)
        {
            stale(
                "branch diff compared a relation to itself; rerun against distinct \
                 candidate and base relations"
                    .to_string(),
            );
        }
        // A shallow diff compared schema and row counts only — no data-diff
        // policies were evaluated, so it carries no verdict and cannot
        // satisfy a required audit. (`diffs` non-empty also proves `--full`,
        // for pre-`deep`-field artifacts.) The schema audit it did run still
        // stands.
        let shallow = !report.deep && report.diffs.is_empty();
        if shallow {
            reject(format!(
                "branch diff ran without `--full`; rerun \
                 `diff --from {candidate} --to {to} --full` for a value-level audit"
            ));
        }
        return AuditEvidence {
            diff_passed: (!shallow).then_some(report.passed),
            diff_rejected: rejected,
            breaking_schema_changes: breaking,
            audited_base_hash: report.base_hash.clone(),
            schema_audited: fresh,
        };
    }

    // The single-model `diff.json` is not promotion evidence: it examined
    // one model, so it cannot certify a branch's schema. Its presence means
    // someone audited a model, not the branch — say so rather than a bare
    // "no evidence".
    if read_diff(cli).is_some() {
        return AuditEvidence {
            diff_rejected: Some(format!(
                "a single-model diff cannot audit a branch; \
                 run `diff --from {candidate} --to {to} --full`"
            )),
            ..AuditEvidence::default()
        };
    }
    AuditEvidence::default()
}

fn read_branch_diff(cli: &Cli) -> Option<BranchDiffReport> {
    let text = std::fs::read_to_string(artifact_path(cli, "branch_diff.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(value.get("diff")?.clone()).ok()
}

fn print_gates_human(report: &phlo_transform_engine::GateReport) {
    for result in &report.results {
        let status = if result.passed { "PASS" } else { "FAIL" };
        println!("{status} {:<10} {}", result.name, result.detail);
    }
    println!();
}

/// The lineage artifact's standing under the gates — advisory context, not
/// a gate itself: `stale` means the report's provenance no longer matches
/// what is being promoted.
fn print_lineage_evidence(lineage: &Option<LineageEvidence>) {
    let Some(lineage) = lineage else {
        return;
    };
    match (lineage.status, &lineage.reason) {
        ("stale", Some(reason)) => println!("Lineage: stale — {reason}\n"),
        ("current", _) => println!(
            "Lineage: {} change(s) vs {} — current\n",
            lineage.changes, lineage.base
        ),
        _ => println!(
            "Lineage: {} change(s) vs {} (advisory — not bound to this environment)\n",
            lineage.changes, lineage.base
        ),
    }
}

/// Removal of a promoted candidate's catalog and branch. Every failure is
/// reported — the merge already happened, so a leftover catalog or branch
/// must never be silent.
async fn cleanup_candidate(
    cli: &Cli,
    nessie: &Arc<dyn NessieClient>,
    candidate: &str,
    environment: Option<&EnvironmentSetup>,
) -> Result<(), String> {
    let catalog = environment
        .map(|setup| setup.catalog.clone())
        .unwrap_or_else(|| catalog_name(candidate));
    let mut failures = Vec::new();
    match build_adapter(cli) {
        Ok(adapter) => {
            if let Err(error) = adapter
                .execute(&format!("DROP CATALOG IF EXISTS {}", catalog))
                .await
            {
                failures.push(format!("drop catalog `{catalog}`: {error}"));
            }
        }
        Err(error) => {
            failures.push(format!(
                "open the adapter to drop catalog `{catalog}`: {error}"
            ));
        }
    }
    if let Err(error) = nessie.delete_branch(candidate).await {
        failures.push(format!("delete branch `{candidate}`: {error}"));
    }
    if failures.is_empty() {
        // The branch and catalog are gone; the provisioning record is stale.
        remove_environment_artifacts(cli, candidate);
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// `phlo-transform ref ...` — Nessie reference management. `create` and
/// `delete` mutate Nessie; `list` and `show` are read-only.
async fn run_ref(cli: &Cli, action: &RefAction) -> Result<ExitCode, String> {
    // `main` is the default base for every environment — deleting it under
    // the same verb as a scratch branch is too easy to do by accident.
    if let RefAction::Delete { name } = action {
        if name == "main" {
            return Err("refusing to delete `main`: it is the default base reference".to_string());
        }
    }
    let nessie = build_nessie(cli)?;
    match action {
        RefAction::List => {
            let references = nessie
                .list_references()
                .await
                .map_err(|error| error.to_string())?;
            if cli.json {
                print_json(&references)?;
            } else {
                for reference in &references {
                    println!(
                        "{:<32} {} {}",
                        reference.name, reference.kind, reference.hash
                    );
                }
            }
        }
        RefAction::Show { name } => {
            let reference = nessie
                .get_reference(name)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("reference `{name}` was not found"))?;
            if cli.json {
                print_json(&reference)?;
            } else {
                println!("{} {} {}", reference.name, reference.kind, reference.hash);
            }
        }
        RefAction::Create { name } => {
            let base_name = cli.from.clone().unwrap_or_else(|| "main".to_string());
            let base = nessie
                .get_reference(&base_name)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("base reference `{base_name}` was not found"))?;
            let reference = nessie
                .create_branch(name, &base)
                .await
                .map_err(|error| error.to_string())?;
            // The branch was just provably cut from `base` — record that
            // provenance now so a later `promote` can trust it even before
            // the candidate is first provisioned for a run.
            write_environment_artifacts(
                cli,
                &EnvironmentSetup {
                    base: base.clone(),
                    candidate: reference.clone(),
                    created_from: Some(base.clone()),
                    created_branch: true,
                    catalog: cli.catalog.clone().unwrap_or_else(|| catalog_name(name)),
                },
            )?;
            if cli.json {
                print_json(&reference)?;
            } else {
                println!(
                    "Created branch {} at {} (from {} @ {})",
                    reference.name, reference.hash, base.name, base.hash
                );
            }
        }
        RefAction::Delete { name } => {
            nessie
                .delete_branch(name)
                .await
                .map_err(|error| error.to_string())?;
            // The branch is gone; its local provisioning evidence is stale.
            remove_environment_artifacts(cli, name);
            if cli.json {
                print_json(&serde_json::json!({ "deleted": name }))?;
            } else {
                println!("Deleted branch {name}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Show a recognisable prefix of an id or hash.
fn short(value: &str, len: usize) -> String {
    if value.len() <= len {
        value.to_string()
    } else {
        format!("{}…", &value[..len])
    }
}

/// `phlo-transform state ...` — read-only inspection of the state store.
fn run_state(cli: &Cli, action: &StateAction) -> Result<ExitCode, String> {
    let state = open_state(cli)?.ok_or_else(|| {
        format!(
            "no state store at {} — nothing has been recorded",
            state_path(cli).display()
        )
    })?;
    match action {
        StateAction::Runs => {
            let env = environment(cli);
            let runs = state
                .runs()
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|run| {
                    env.as_deref()
                        .map(|env| run.environment.as_deref() == Some(env))
                        .unwrap_or(true)
                })
                .collect::<Vec<_>>();
            if cli.json {
                print_json(&runs)?;
            } else if runs.is_empty() {
                println!("No runs recorded");
            } else {
                for run in &runs {
                    println!(
                        "{:<14} {:<10} {:<20} {:>3} models {:>3} failed  {}",
                        short(&run.run_id, 12),
                        run.status.label(),
                        run.environment.as_deref().unwrap_or("-"),
                        run.model_count,
                        run.failed_count,
                        run.started_at,
                    );
                }
            }
        }
        StateAction::Show { run } => {
            let matches = state.find_runs(run).map_err(|error| error.to_string())?;
            let summary = match matches.as_slice() {
                [] => return Err(format!("no run matches `{run}`")),
                [only] => only,
                _ => {
                    return Err(format!(
                        "`{run}` matches {} runs — give a longer prefix",
                        matches.len()
                    ))
                }
            };
            let stored = state
                .run(&summary.run_id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("run `{}` was not found", summary.run_id))?;
            let models = state
                .model_runs(&summary.run_id)
                .map_err(|error| error.to_string())?;
            let seeds = state
                .seed_runs(&summary.run_id)
                .map_err(|error| error.to_string())?;
            let tests = state
                .test_runs(&summary.run_id)
                .map_err(|error| error.to_string())?;
            if cli.json {
                print_json(&serde_json::json!({
                    "run": stored.record,
                    "plan": stored.plan,
                    "models": models,
                    "seeds": seeds,
                    "tests": tests,
                }))?;
            } else {
                println!(
                    "Run {} — {} ({} models, {} failed, env {})",
                    stored.record.run_id,
                    stored.record.status.label(),
                    stored.record.model_count,
                    stored.record.failed_count,
                    stored.record.environment.as_deref().unwrap_or("-"),
                );
                for model in &models {
                    println!(
                        "  {:<10} {:<40} {:<14} {}",
                        model.status.label(),
                        model.model_id,
                        model.action,
                        model.error.as_deref().unwrap_or("")
                    );
                }
                for seed in &seeds {
                    println!(
                        "  {:<10} {:<40} seed          {}",
                        seed.status.label(),
                        seed.name,
                        seed.error.as_deref().unwrap_or("")
                    );
                }
                for test in &tests {
                    println!(
                        "  {:<10} {:<40} test          {}",
                        test.status.label(),
                        test.test_id,
                        test.error.as_deref().unwrap_or("")
                    );
                }
            }
        }
        StateAction::Model { model } => {
            let id = ModelId::parse(model)
                .map_err(|error| format!("invalid model reference `{model}`: {error}"))?;
            let record = state
                .materialized_version(&id.logical_name(), environment(cli).as_deref())
                .map_err(|error| error.to_string())?;
            match record {
                None => {
                    return Err(format!(
                        "no materialisation recorded for {} in environment {}",
                        id.logical_name(),
                        environment(cli).unwrap_or_else(|| "<default>".to_string()),
                    ))
                }
                Some(record) => {
                    if cli.json {
                        print_json(&record)?;
                    } else {
                        println!("Model:         {}", record.model_id);
                        println!(
                            "Environment:   {}",
                            record.environment.as_deref().unwrap_or("-")
                        );
                        println!("Version:       {}", record.version.short());
                        println!("Target:        {}", record.target);
                        println!(
                            "Adapter:       {}",
                            record.adapter.as_deref().unwrap_or("<unrecorded>")
                        );
                        println!("Run:           {}", short(&record.run_id, 12));
                        println!("Materialised:  {}", record.materialized_at);
                        if let Some(strategy) = &record.incremental_strategy {
                            println!(
                                "Incremental:   {strategy} ({})",
                                record.incremental_key.as_deref().unwrap_or("-")
                            );
                        }
                        println!("Components:");
                        println!("  sql:      {}", record.version.sql_hash);
                        println!("  config:   {}", record.version.config_hash);
                        println!("  contract: {}", record.version.contract_hash);
                        println!("  deps:     {}", record.version.dependency_hash);
                        println!("  sources:  {}", record.version.source_state_hash);
                        println!("  compiler: {}", record.version.compiler_version);
                        println!("  target:   {}", record.version.target_hash);
                        if let Some(contract) = &record.contract {
                            let columns = contract
                                .columns
                                .iter()
                                .map(|column| column.name.clone())
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!(
                                "Contract:      {} ({} columns{})",
                                if contract.enforced {
                                    "enforced"
                                } else {
                                    "advisory"
                                },
                                contract.columns.len(),
                                if columns.is_empty() {
                                    String::new()
                                } else {
                                    format!(": {columns}")
                                }
                            );
                        }
                    }
                }
            }
        }
        StateAction::Promotions => {
            let promotions = state.promotions().map_err(|error| error.to_string())?;
            if cli.json {
                print_json(&promotions)?;
            } else if promotions.is_empty() {
                println!("No promotions recorded");
            } else {
                for promotion in &promotions {
                    println!(
                        "{:<14} {} -> {}  merged={} dry_run={}  {}",
                        short(&promotion.promotion_id, 12),
                        promotion.candidate_ref,
                        promotion.target_ref,
                        promotion.merged,
                        promotion.dry_run,
                        promotion.timestamp,
                    );
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_rollback(cli: &Cli, to: &str) -> Result<ExitCode, String> {
    let nessie = build_nessie(cli)?;
    let name = environment(cli).ok_or_else(|| "rollback requires --ref <reference>".to_string())?;
    match nessie.assign_reference(&name, to).await {
        Ok(reference) => {
            if cli.json {
                print_json(&reference)?;
            } else {
                println!("Rolled back {} to {}", reference.name, reference.hash);
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            let message = error.to_string();
            if cli.json {
                print_json(&serde_json::json!({ "ok": false, "error": message }))?;
            } else {
                eprintln!("error: {message}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

fn print_promotion_human(record: &phlo_transform_engine::PromotionRecord) {
    println!("Promotion: {}", record.promotion_id);
    println!(
        "Candidate: {} @ {:?}",
        record.candidate_ref, record.candidate_hash
    );
    println!(
        "Target:    {} @ {}",
        record.target_ref, record.target_hash_before
    );
    if record.dry_run {
        println!("Mode:      check (not merged)");
    } else {
        println!("Merged:    {}", record.merged);
    }
    if let Some(after) = &record.target_hash_after {
        println!("After:     {after}");
    }
    for conflict in &record.conflicts {
        println!("Conflict:  {} — {}", conflict.path, conflict.message);
    }
    println!();
}

async fn run_plan(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    git: Option<&GitChanges>,
) -> Result<ExitCode, String> {
    let (plan, writer) = build_plan(cli, compilation, set, open_state(cli)?, git).await?;
    writer
        .write_project(compilation)
        .and_then(|_| writer.write_plan(&plan))
        .map_err(|error| error.to_string())?;

    if cli.json {
        print_json(&plan)?;
    } else {
        print_plan_human(&plan);
        // `print_plan_human` already surfaces diagnostics up front when
        // the plan is blocked; only trailing (non-blocking) diagnostics
        // are printed here.
        if !plan.blocked {
            render_diagnostics(&plan.diagnostics);
        }
    }

    Ok(if plan.blocked {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Bind a completed run to its environment's post-run Nessie head. A run
/// advances a candidate branch with every write, so the head captured at
/// provisioning cannot vouch for the commit the run actually validated —
/// the binding is recorded only on a fully passed run, and only when the
/// run's environment resolves to a real Nessie reference.
async fn bind_run_reference(
    cli: &Cli,
    state: &Arc<dyn StateStore>,
    result: &RunResult,
) -> Result<(), String> {
    if result.status != ExecutionStatus::Passed || nessie_endpoint(cli).is_none() {
        return Ok(());
    }
    // The run's own environment label — for `--resume`/`--retry-failed` that
    // is the stored run's, not this invocation's flags.
    let Some(environment) = result.environment.clone() else {
        return Ok(());
    };
    let nessie = build_nessie(cli)?;
    // An environment label that is not a Nessie reference leaves the run
    // unbound — it simply cannot promote a commit-bound candidate.
    let Some(head) = nessie
        .get_reference(&environment)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    state
        .bind_run_reference_hash(&result.run_id, &head.hash)
        .map_err(|error| error.to_string())
}

async fn run_apply(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    git: Option<&GitChanges>,
    convenience_run: bool,
) -> Result<ExitCode, String> {
    let state = open_state(cli)?;

    // `--resume` / `--retry-failed` continue a prior run: their work comes
    // from the stored run, not from CLI selection.
    if cli.resume.is_some() || cli.retry_failed.is_some() {
        if !set.is_unrestricted() || cli.force || cli.since.is_some() {
            return Err(
                "--resume/--retry-failed derive their work from the prior run; \
                 selection flags do not apply"
                    .to_string(),
            );
        }
        let adapter = build_adapter(cli)?;
        let cancel = CancelHandle::default();
        spawn_ctrl_c_listener(cancel.clone());
        let runner = Runner::new(adapter, state.clone());
        let options = run_options(cli, cancel);
        let result = match (&cli.resume, &cli.retry_failed) {
            (Some(_), Some(_)) => {
                return Err(
                    "--resume and --retry-failed are different continuations; use one".to_string(),
                )
            }
            (Some(id), None) => runner.resume(compilation, id, &options).await,
            (None, Some(id)) => runner.retry_failed(compilation, id, &options).await,
            _ => unreachable!(),
        }
        .map_err(|error| error.to_string())?;
        if let Some(state) = &state {
            bind_run_reference(cli, state, &result).await?;
        }
        let writer = ArtifactWriter::for_workspace(&cli.root);
        writer
            .write_project(compilation)
            .and_then(|_| writer.write_run(&result))
            .map_err(|error| error.to_string())?;
        if cli.json {
            print_json(&result)?;
        } else {
            print_run_human(&result);
        }
        return Ok(if result.status == ExecutionStatus::Passed {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    let (plan, writer) = build_plan(cli, compilation, set, state.clone(), git).await?;
    writer
        .write_project(compilation)
        .and_then(|_| writer.write_plan(&plan))
        .map_err(|error| error.to_string())?;

    if plan.blocked {
        if cli.json {
            print_json(&plan)?;
        } else {
            print_plan_human(&plan);
        }
        return Ok(ExitCode::FAILURE);
    }

    let adapter = build_adapter(cli)?;
    let cancel = CancelHandle::default();
    spawn_ctrl_c_listener(cancel.clone());
    let runner = Runner::new(adapter, state.clone());
    let options = run_options(cli, cancel);
    let result = runner
        .apply(compilation, &plan, &options)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(state) = &state {
        bind_run_reference(cli, state, &result).await?;
    }
    writer
        .write_run(&result)
        .map_err(|error| error.to_string())?;

    if cli.json {
        print_json(&result)?;
    } else {
        if convenience_run {
            print_plan_human(&plan);
        }
        print_run_human(&result);
    }

    Ok(if result.status == ExecutionStatus::Passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn run_test(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    git: Option<&GitChanges>,
) -> Result<ExitCode, String> {
    let adapter = build_adapter(cli)?;
    // A test runs when every model it reads is selected; tests that only
    // read sources are left to unrestricted runs.
    let members: Option<std::collections::BTreeSet<String>> = if set.is_unrestricted() {
        None
    } else {
        let selection = resolve(cli, compilation, set, git)?;
        Some(
            selection
                .members
                .iter()
                .map(|member| member.id.clone())
                .collect(),
        )
    };
    let mut results = Vec::new();
    let mut failed = false;
    for test in &compilation.tests {
        if let Some(members) = &members {
            let covered = !test.targets.is_empty()
                && test
                    .targets
                    .iter()
                    .all(|target| members.contains(target.logical_name().as_str()));
            if !covered {
                continue;
            }
        }
        let outcome = adapter.execute(&test.compiled_sql).await;
        let (status, row_count, error) = match outcome {
            Ok(query) if query.row_count == 0 => (ExecutionStatus::Passed, 0, None),
            Ok(query) => (
                ExecutionStatus::Failed,
                query.row_count,
                Some(format!("test returned {} row(s)", query.row_count)),
            ),
            Err(error) => (ExecutionStatus::Failed, 0, Some(error.to_string())),
        };
        if status == ExecutionStatus::Failed {
            failed = true;
        }
        results.push(TestOutcome {
            test: test.id.to_string(),
            status,
            row_count,
            error,
        });
    }

    if cli.json {
        print_json(&TestReport { tests: results })?;
    } else {
        for result in &results {
            println!(
                "{:<8} {} ({} row(s))",
                result.status.label(),
                result.test,
                result.row_count
            );
            if let Some(error) = &result.error {
                println!("  {error}");
            }
        }
    }

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn run_lineage(
    cli: &Cli,
    compilation: &Compilation,
    target: Option<&str>,
    set: &SelectorSet,
    git: Option<&GitChanges>,
    format: Option<LineageFormat>,
    diff: &[String],
) -> Result<ExitCode, String> {
    if !diff.is_empty() {
        return run_lineage_diff(cli, compilation, diff).await;
    }
    // `--format` exports the canonical lineage graph: scoped to a model
    // target or a selection when given, the whole workspace otherwise.
    if let Some(format) = format {
        let scope = match target {
            Some(target) => {
                let id = ModelId::parse(target)
                    .map_err(|error| format!("invalid model `{target}`: {error}"))?;
                if compilation.model(&id).is_none() {
                    return Err(format!(
                        "`--format` exports model lineage; `{target}` is not a model"
                    ));
                }
                Some(std::collections::BTreeSet::from([id]))
            }
            None if !set.is_unrestricted() => Some(
                resolve(cli, compilation, set, git)?
                    .ids()
                    .into_iter()
                    .collect(),
            ),
            None => None,
        };
        let graph = match &scope {
            Some(models) => compilation.lineage.subgraph(models),
            None => compilation.lineage.clone(),
        };
        return match format {
            LineageFormat::Graph => {
                print_json(&graph.document())?;
                Ok(ExitCode::SUCCESS)
            }
            LineageFormat::Openlineage => {
                let document =
                    phlo_transform_openlineage::OpenLineageExporter::new(&graph).export();
                print_json(&document)?;
                Ok(ExitCode::SUCCESS)
            }
        };
    }

    let Some(target) = target else {
        return run_graph_lineage(cli, compilation, set, git);
    };
    // A target takes the whole report for one model; selectors only make
    // sense for the graph listing.
    if !set.include.is_empty() || !set.exclude.is_empty() || !set.filter.is_empty() {
        return Err("pass either a lineage target or selector terms, not both".to_string());
    }

    // A model target shows model lineage; otherwise the last segment is a
    // column and the prefix is the model.
    if let Ok(id) = ModelId::parse(target) {
        if compilation.model(&id).is_some() {
            let mut report = compilation.model_lineage_report(&id).expect("model exists");
            apply_direction(&mut report, cli);
            if cli.json {
                print_json(&report)?;
            } else {
                println!("Model:      {}", report.model);
                println!("Upstream:   {}", join_or_none(&report.upstream));
                println!("Downstream: {}", join_or_none(&report.downstream));
            }
            return Ok(ExitCode::SUCCESS);
        }
    }

    let Some((model, column)) = target.rsplit_once('.') else {
        return Err(format!(
            "invalid lineage target `{target}`; expected a model or model.column"
        ));
    };
    let id = ModelId::parse(model).map_err(|error| format!("invalid model `{model}`: {error}"))?;
    match compilation.column_lineage_report(&id, column) {
        Some(report) => {
            if cli.json {
                print_json(&report)?;
            } else {
                println!("Column:     {}", report.column);
                if report.confidence != phlo_transform_core::LineageConfidence::Exact {
                    println!("Confidence: {}", confidence_label(report.confidence));
                }
                println!("Direct:     {}", join_or_none(&report.direct));
                if !report.indirect.is_empty() {
                    println!("Indirect:   {}", join_or_none(&report.indirect));
                }
                println!("Transitive: {}", join_or_none(&report.transitive));
            }
            Ok(ExitCode::SUCCESS)
        }
        None => {
            let message = format!("no column lineage for `{target}`");
            if cli.json {
                print_json(&serde_json::json!({ "ok": false, "error": message }))?;
            } else {
                eprintln!("error: {message}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

/// `lineage --diff <base>` / `lineage --diff <base> <candidate>`: compile
/// the base workspace and diff its lineage graph against the candidate's.
///
/// One ref uses the same merge-base semantics as `--since` — the workspace
/// is compared against `merge-base(base, HEAD)`, so a feature branch diffs
/// against where it diverged rather than against the other ref's current
/// head. Two refs compare the exact refs with no worktree involved. Both
/// trees are materialised read-only — the checkout is never touched.
async fn run_lineage_diff(
    cli: &Cli,
    compilation: &Compilation,
    refs: &[String],
) -> Result<ExitCode, String> {
    let (base_kind, base_ref, base_tree, candidate_tree, candidate) = match refs {
        [base_ref] => {
            // merge-base(ref, HEAD) -> worktree: the branch-change
            // workflow, identical to `--since`.
            let comparison = phlo_transform_core::comparison_base(&cli.root, base_ref)
                .map_err(|error| error.to_string())?;
            let tree = phlo_transform_core::checkout_tree(&cli.root, &comparison.commit)
                .map_err(|error| error.to_string())?;
            (
                "merge-base",
                base_ref.clone(),
                tree,
                None,
                CandidateProvenance {
                    git_ref: environment(cli).or_else(|| cli.from.clone()),
                    head: comparison.head,
                    dirty: comparison.dirty,
                    lineage_hash: None,
                    model_versions: BTreeMap::new(),
                },
            )
        }
        [base_ref, candidate_ref] => {
            // Exact ref -> exact ref: the candidate is a checked-out tree,
            // not the working tree.
            let base_tree = phlo_transform_core::checkout_tree(&cli.root, base_ref)
                .map_err(|error| error.to_string())?;
            let candidate_tree = phlo_transform_core::checkout_tree(&cli.root, candidate_ref)
                .map_err(|error| error.to_string())?;
            (
                "ref",
                base_ref.clone(),
                base_tree,
                Some(candidate_tree),
                CandidateProvenance {
                    git_ref: Some(candidate_ref.clone()),
                    head: None,
                    dirty: false,
                    lineage_hash: None,
                    model_versions: BTreeMap::new(),
                },
            )
        }
        _ => return Err("`--diff` takes one or two Git refs".to_string()),
    };
    let base_commit = base_tree.commit.clone();
    let candidate = CandidateProvenance {
        head: candidate_tree
            .as_ref()
            .map(|tree| tree.commit.clone())
            .or(candidate.head),
        ..candidate
    };

    let base = match compile_tree(cli, &base_tree.workspace, &base_ref).await {
        Ok(base) => base,
        Err(code) => return Ok(code),
    };
    let base_graph = phlo_transform_core::LineageGraph::build(&base);
    let candidate_compilation = match &candidate_tree {
        Some(tree) => match compile_tree(cli, &tree.workspace, &refs[1]).await {
            Ok(candidate) => Some(candidate),
            Err(code) => return Ok(code),
        },
        None => None,
    };
    let candidate_graph = candidate_compilation
        .as_ref()
        .map(|candidate| candidate.lineage.clone())
        .unwrap_or_else(|| compilation.lineage.clone());
    // The candidate's definitional identity: the canonical graph's
    // fingerprint — the stale-artifact check promotion can trust — plus
    // each model's content-addressed version as diagnostic context.
    let candidate = CandidateProvenance {
        lineage_hash: Some(candidate_graph.fingerprint()),
        model_versions: candidate_compilation
            .as_ref()
            .unwrap_or(compilation)
            .models
            .iter()
            .map(|model| (model.id.logical_name(), model.version.hash.clone()))
            .collect(),
        ..candidate
    };
    let mut diff = phlo_transform_core::lineage_diff(&base_graph, &candidate_graph);
    diff.base_ref = Some(match base_kind {
        "merge-base" => format!("{base_ref} (merge-base {})", short(&base_commit, 12)),
        _ => format!("{base_ref} ({})", short(&base_commit, 12)),
    });

    // Bind the diff to the Nessie pair it describes, when both resolve:
    // the environment names the candidate branch, the base ref names the
    // target. Unconfigured or unresolved leaves the artifact unbound —
    // promotion then treats it as advisory rather than current evidence.
    let environment_binding = match (candidate.git_ref.clone(), nessie_endpoint(cli)) {
        (Some(candidate_ref), Some(_)) => match build_nessie(cli) {
            Ok(nessie) => {
                let candidate_nessie = nessie.get_reference(&candidate_ref).await.ok().flatten();
                let target_nessie = nessie.get_reference(&base_ref).await.ok().flatten();
                match (candidate_nessie, target_nessie) {
                    (Some(candidate), Some(target)) => Some(LineageEnvironment {
                        candidate_ref,
                        candidate_hash: candidate.hash,
                        target_ref: base_ref.clone(),
                        target_hash: target.hash,
                    }),
                    _ => None,
                }
            }
            Err(_) => None,
        },
        _ => None,
    };

    ArtifactWriter::for_workspace(&cli.root)
        .write_lineage_diff(&LineageDiffArtifact {
            schema_version: SCHEMA_VERSION,
            base_kind: base_kind.to_string(),
            base_ref: base_ref.clone(),
            base_commit,
            candidate,
            environment: environment_binding,
            diff: diff.clone(),
        })
        .map_err(|error| error.to_string())?;

    if cli.json {
        return print_json(&diff).map(|_| ExitCode::SUCCESS);
    }
    if diff.is_empty() {
        println!(
            "No lineage changes since {}",
            diff.base_ref.as_deref().unwrap_or(&base_ref)
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "Lineage diff vs {}",
        diff.base_ref.as_deref().unwrap_or(&base_ref)
    );
    let section = |title: &str, count: usize| {
        if count > 0 {
            println!();
            println!("{title}");
        }
    };
    section("Added", diff.nodes_added.len());
    for node in &diff.nodes_added {
        println!("  + {} ({})", node.name, node.kind);
    }
    section("Removed", diff.nodes_removed.len());
    for node in &diff.nodes_removed {
        println!("  - {} ({})", node.name, node.kind);
    }
    section("Changed", diff.nodes_changed.len());
    for node in &diff.nodes_changed {
        println!("  ~ {} ({})", node.name, node.kind);
        for change in &node.changes {
            println!("      {change}");
        }
    }
    if !diff.edges_added.is_empty()
        || !diff.edges_removed.is_empty()
        || !diff.edges_changed.is_empty()
    {
        println!();
        println!("Edges");
        for edge in &diff.edges_added {
            let detail = edge
                .detail
                .as_ref()
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default();
            println!("  + {} --{}-> {}{detail}", edge.from, edge.kind, edge.to);
        }
        for edge in &diff.edges_removed {
            let detail = edge
                .detail
                .as_ref()
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default();
            println!("  - {} --{}-> {}{detail}", edge.from, edge.kind, edge.to);
        }
        for edge in &diff.edges_changed {
            println!("  ~ {} --{}-> {}", edge.from, edge.kind, edge.to);
            for change in &edge.changes {
                println!("      {change}");
            }
        }
    }
    section("Impacts", diff.impacts.len() + diff.edge_impacts.len());
    for impact in &diff.impacts {
        println!("  {} orphans: {}", impact.node, impact.orphans.join(", "));
    }
    for impact in &diff.edge_impacts {
        println!(
            "  {} --{}-> {} {}: affects {}",
            impact.from,
            impact.kind,
            impact.to,
            impact.change,
            impact.downstream.join(", ")
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Compile the workspace inside a materialised Git tree: load it, apply
/// the same `--catalog` override, and enrich it against the same adapter —
/// the live compile's twin, or every target would diff as moved. `label`
/// is the ref name diagnostics are reported under.
async fn compile_tree(
    cli: &Cli,
    workspace: &std::path::Path,
    label: &str,
) -> Result<Compilation, ExitCode> {
    let mut project = match load_project(workspace) {
        Ok(project) => project,
        Err(diagnostics) => {
            report_diff_diagnostics(cli, label, "cannot load", &diagnostics);
            return Err(ExitCode::FAILURE);
        }
    };
    let base_catalog = project.defaults.catalog.clone();
    let base_schema = project.defaults.schema.clone();
    if let Some(catalog) = &cli.catalog {
        project.defaults.catalog = Some(catalog.clone());
    }
    let compiled = {
        let plain = compile(&project);
        enrich(
            cli,
            &project,
            base_catalog.as_deref(),
            base_schema.as_deref(),
            &plain,
        )
        .await
        .unwrap_or(plain)
    };
    if !compiled.is_ok() {
        report_diff_diagnostics(
            cli,
            label,
            "does not compile cleanly",
            &compiled.diagnostics,
        );
        return Err(ExitCode::FAILURE);
    }
    Ok(compiled)
}

fn report_diff_diagnostics(cli: &Cli, label: &str, problem: &str, diagnostics: &[Diagnostic]) {
    if cli.json {
        let _ = print_json(&serde_json::json!({
            "ok": false,
            "base_ref": label,
            "diagnostics": diagnostics,
        }));
    } else {
        eprintln!("`{label}` {problem}:");
        render_diagnostics(diagnostics);
    }
}

/// `lineage` with no target: the model graph as a compact edge list,
/// optionally scoped to a selection. Edges are restricted to selected
/// models — the listing answers "the selected subgraph", not "everything
/// touching it" (`+` terms already add the neighbourhood when wanted).
fn run_graph_lineage(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    git: Option<&GitChanges>,
) -> Result<ExitCode, String> {
    let mut report = compilation.list_report();
    if !set.is_unrestricted() {
        let selection = resolve(cli, compilation, set, git)?;
        filter_list_report(compilation, &mut report, &selection);
    }
    let mut downstream: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let names: std::collections::BTreeSet<&str> = report
        .models
        .iter()
        .map(|model| model.name.as_str())
        .collect();
    for model in &report.models {
        for dependency in &model.depends_on {
            if names.contains(dependency.as_str()) {
                downstream
                    .entry(dependency.as_str())
                    .or_default()
                    .push(model.name.as_str());
            }
        }
    }

    if cli.json {
        let models: Vec<serde_json::Value> = report
            .models
            .iter()
            .map(|model| {
                let upstream: Vec<&String> = model
                    .depends_on
                    .iter()
                    .filter(|dependency| names.contains(dependency.as_str()))
                    .collect();
                serde_json::json!({
                    "name": model.name,
                    "upstream": upstream,
                    "sources": model.sources,
                    "downstream": downstream.get(model.name.as_str()).cloned().unwrap_or_default(),
                })
            })
            .collect();
        print_json(&serde_json::json!({ "models": models }))?;
        return Ok(ExitCode::SUCCESS);
    }

    println!("Models ({})", report.models.len());
    let width = report
        .models
        .iter()
        .map(|model| model.name.len())
        .max()
        .unwrap_or(0);
    for model in &report.models {
        let upstream: Vec<&String> = model
            .depends_on
            .iter()
            .filter(|dependency| names.contains(dependency.as_str()))
            .collect();
        let mut edges = String::new();
        if !upstream.is_empty() {
            edges.push_str(&format!(
                " <- {}",
                upstream
                    .iter()
                    .map(|dependency| dependency.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(dependents) = downstream.get(model.name.as_str()) {
            edges.push_str(&format!(" -> {}", dependents.join(", ")));
        }
        if edges.is_empty() {
            edges.push_str("   (isolated)");
        }
        println!("  {:<width$}{}", model.name, edges, width = width + 2);
    }
    Ok(ExitCode::SUCCESS)
}

fn run_impact(
    cli: &Cli,
    compilation: &Compilation,
    column: Option<&str>,
    set: &SelectorSet,
    git: Option<&GitChanges>,
) -> Result<ExitCode, String> {
    // `impact --select ...` with no positional argument: the blast radius
    // of a selection — every downstream dependent outside the set, plus the
    // tests covering it.
    let Some(column) = column else {
        if set.is_unrestricted() {
            return Err(
                "impact needs a column (`impact assay.results.titre`) or selector terms"
                    .to_string(),
            );
        }
        return run_selection_impact(cli, compilation, set, git);
    };

    // An active selection scopes the reported impact to selected models.
    let members: Option<std::collections::BTreeSet<String>> = if set.is_unrestricted() {
        None
    } else {
        let selection = resolve(cli, compilation, set, git)?;
        Some(
            selection
                .members
                .iter()
                .map(|member| member.id.clone())
                .collect(),
        )
    };
    let in_scope = |name: &str| members.as_ref().map(|m| m.contains(name)).unwrap_or(true);
    let member_tests = |model_name: &str| -> Vec<String> {
        ModelId::parse(model_name)
            .ok()
            .map(|id| {
                compilation
                    .tests_for(&id)
                    .iter()
                    .map(|test| test.id.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    // A model-only argument reports downstream models/tests (works offline).
    if let Some(model) = compilation.model_by_name(column) {
        let lineage = compilation
            .model_lineage_report(&model.id)
            .expect("model exists");
        let downstream: Vec<String> = lineage
            .downstream
            .iter()
            .filter(|name| in_scope(name))
            .cloned()
            .collect();
        let mut tests: Vec<String> = Vec::new();
        for dependent in &downstream {
            tests.extend(member_tests(dependent));
        }
        if cli.json {
            print_json(&serde_json::json!({
                "model": model.id.logical_name(),
                "downstream_models": downstream,
                "tests": tests,
            }))?;
        } else {
            println!("Model:             {}", model.id.logical_name());
            println!("Downstream models: {}", join_or_none(&downstream));
            println!("Tests:             {}", join_or_none(&tests));
        }
        return Ok(ExitCode::SUCCESS);
    }

    let Some((model, name)) = column.rsplit_once('.') else {
        return Err(format!(
            "invalid column `{column}`; expected model.column or dataset.column"
        ));
    };
    // A model column first; otherwise the prefix may be a source or seed
    // dataset (`impact external.samples.volume`).
    let target = if let Ok(id) = ModelId::parse(model) {
        if compilation.model(&id).is_some() {
            phlo_transform_core::ColumnRef::model(id, name)
        } else if let Some(dataset) = compilation.lineage.dataset_by_name(model) {
            let source = phlo_transform_core::SourceId::new(dataset.parts().to_vec())
                .map_err(|error| error.to_string())?;
            phlo_transform_core::ColumnRef::source(source, name)
        } else {
            return Err(format!("no such model or dataset: {model}"));
        }
    } else if let Some(dataset) = compilation.lineage.dataset_by_name(model) {
        let source = phlo_transform_core::SourceId::new(dataset.parts().to_vec())
            .map_err(|error| error.to_string())?;
        phlo_transform_core::ColumnRef::source(source, name)
    } else {
        return Err(format!("invalid model `{model}`; no such dataset either"));
    };
    let mut report = compilation.impact_report(&target);
    if let Some(members) = &members {
        report
            .downstream_models
            .retain(|name| members.contains(name));
        report.downstream_columns.retain(|name| {
            name.rsplit_once('.')
                .map(|(model, _)| members.contains(model))
                .unwrap_or(false)
        });
        let member_test_ids: std::collections::BTreeSet<String> = members
            .iter()
            .flat_map(|model| member_tests(model))
            .collect();
        report.tests.retain(|test| member_test_ids.contains(test));
    }
    if cli.json {
        print_json(&report)?;
    } else {
        println!("Column:             {}", report.column);
        println!(
            "Downstream columns: {}",
            join_or_none(&report.downstream_columns)
        );
        println!(
            "Downstream models:  {}",
            join_or_none(&report.downstream_models)
        );
        println!("Tests:              {}", join_or_none(&report.tests));
        if !report.consumers.is_empty() {
            println!("Consumers:          {}", join_or_none(&report.consumers));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `impact --select <terms>`: the selection's blast radius — dependents
/// outside the selected set, and the tests covering them.
fn run_selection_impact(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    git: Option<&GitChanges>,
) -> Result<ExitCode, String> {
    let selection = resolve(cli, compilation, set, git)?;
    let members: std::collections::BTreeSet<ModelId> = selection.ids().into_iter().collect();
    let mut impacted: std::collections::BTreeSet<ModelId> = std::collections::BTreeSet::new();
    for member in &members {
        for node in compilation
            .lineage
            .downstream_transitive(&phlo_transform_core::LineageNode::Model(member.clone()))
        {
            if let phlo_transform_core::LineageNode::Model(id) = node {
                impacted.insert(id);
            }
        }
    }
    for member in &members {
        impacted.remove(member);
    }
    let mut tests: Vec<String> = Vec::new();
    for id in &impacted {
        for test in compilation
            .lineage
            .tests_for_dataset(&compilation.lineage.output_dataset(id))
        {
            tests.push(test.to_string());
        }
    }
    let impacted: Vec<String> = impacted.iter().map(|id| id.logical_name()).collect();
    if cli.json {
        print_json(&serde_json::json!({
            "selected": selection,
            "impacted_models": impacted,
            "tests": tests,
        }))?;
    } else {
        println!("Selected:          {}", join_or_none(&selection.terms));
        println!("Impacted models:   {}", join_or_none(&impacted));
        println!("Tests:             {}", join_or_none(&tests));
    }
    Ok(ExitCode::SUCCESS)
}

fn apply_direction(report: &mut phlo_transform_core::ModelLineageReport, cli: &Cli) {
    if cli.upstream && !cli.downstream {
        report.downstream.clear();
    } else if cli.downstream && !cli.upstream {
        report.upstream.clear();
    }
}

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "(none)".to_string()
    } else {
        values.join(", ")
    }
}

fn confidence_label(confidence: phlo_transform_core::LineageConfidence) -> &'static str {
    use phlo_transform_core::LineageConfidence::*;
    match confidence {
        Exact => "exact",
        Inferred => "inferred",
        Declared => "declared",
        Runtime => "runtime",
        Unknown => "unknown",
    }
}

#[derive(serde::Serialize)]
struct TestReport {
    tests: Vec<TestOutcome>,
}

#[derive(serde::Serialize)]
struct TestOutcome {
    test: String,
    status: ExecutionStatus,
    row_count: u64,
    error: Option<String>,
}

fn build_adapter(cli: &Cli) -> Result<Arc<dyn Adapter>, String> {
    match cli.adapter.as_deref() {
        Some("duckdb") => return build_duckdb(cli),
        Some("trino") | None => {}
        Some(other) => {
            return Err(format!(
                "unknown adapter `{other}` (expected `trino` or `duckdb`)"
            ));
        }
    }
    // `--duckdb-path` implies the DuckDB adapter for convenience.
    if cli.duckdb_path.is_some() {
        return build_duckdb(cli);
    }
    let endpoint = cli
        .trino_endpoint
        .clone()
        .or_else(|| std::env::var("PHLO_TRINO_ENDPOINT").ok())
        .ok_or_else(|| {
            "no target configured: pass --trino-endpoint/--adapter trino, or --adapter duckdb for local execution"
                .to_string()
        })?;
    let user = cli
        .trino_user
        .clone()
        .or_else(|| std::env::var("PHLO_TRINO_USER").ok())
        .unwrap_or_else(|| "phlo".to_string());
    let password = cli
        .trino_password
        .clone()
        .or_else(|| std::env::var("PHLO_TRINO_PASSWORD").ok());
    let catalog = cli
        .trino_catalog
        .clone()
        .or_else(|| std::env::var("PHLO_TRINO_CATALOG").ok());
    let schema = cli
        .trino_schema
        .clone()
        .or_else(|| std::env::var("PHLO_TRINO_SCHEMA").ok());

    let mut config = TrinoConfig::new(endpoint);
    config.user = user;
    config.password = password;
    config.catalog = catalog;
    config.schema = schema;
    let adapter = TrinoAdapter::new(config).map_err(|error| error.to_string())?;
    Ok(Arc::new(adapter))
}

fn build_duckdb(cli: &Cli) -> Result<Arc<dyn Adapter>, String> {
    let adapter = match cli.duckdb_path.as_deref() {
        Some(":memory:") => DuckDbAdapter::in_memory(),
        Some(path) => {
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                cli.root.join(path)
            };
            DuckDbAdapter::open(&path)
        }
        None => DuckDbAdapter::open(
            &cli.root
                .join(".phlo")
                .join("transform")
                .join("local.duckdb"),
        ),
    }
    .map_err(|error| error.to_string())?;
    Ok(Arc::new(adapter))
}

fn state_path(cli: &Cli) -> PathBuf {
    cli.root.join(".phlo").join("transform").join("state.db")
}

/// Whether a command benefits from catalogue-enriched schemas and observed
/// source states.
fn should_enrich(cli: &Cli) -> bool {
    cli.catalogue
        || matches!(
            cli.command,
            Command::Inspect { .. }
                | Command::Lineage { .. }
                | Command::Impact { .. }
                | Command::Explain { .. }
                | Command::Plan { .. }
                | Command::Apply { .. }
                | Command::Run { .. }
                | Command::Test { .. }
        )
}

/// Recompile with external source schemas and source states from the target.
async fn enrich(
    cli: &Cli,
    project: &phlo_transform_core::SemanticProject,
    default_catalog: Option<&str>,
    default_schema: Option<&str>,
    base: &Compilation,
) -> Option<Compilation> {
    let adapter = build_adapter(cli).ok()?;
    let sources = base.sources();
    // Unqualified sources resolve through the engine's search path, so a
    // bare `raw_orders` lands in the adapter's own default schema — `main`
    // on DuckDB. Match that here or state/schema lookups miss entirely.
    let default_schema = default_schema.or(adapter_default_schema(adapter.name()));
    let mut provider = StaticSchemaProvider::new();
    for source in &sources {
        let relation = relation_for_source(source, default_catalog, default_schema);
        let Ok(columns) = adapter.relation_columns(&relation).await else {
            continue;
        };
        if columns.is_empty() {
            continue;
        }
        let schema = RelationSchema::new(
            columns
                .into_iter()
                .map(|column| SchemaColumn {
                    name: column.name,
                    data_type: DataType::parse_trino(&column.data_type),
                    nullability: if column.nullable {
                        Nullability::Unknown
                    } else {
                        Nullability::NotNull
                    },
                })
                .collect(),
        );
        provider.insert(&source.logical_name(), schema);
    }
    // A source whose state cannot be observed must not discard the schema
    // enrichment already gathered for the others.
    let source_states = collect_source_states(
        adapter.as_ref(),
        &sources,
        &base.seeds,
        default_catalog,
        default_schema,
    )
    .await
    .unwrap_or_default();
    Some(compile_with_options(project, &provider, &source_states))
}

/// Forward Ctrl-C to the running plan as a cooperative cancellation.
fn spawn_ctrl_c_listener(cancel: CancelHandle) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("cancelling...");
            cancel.cancel();
        }
    });
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("could not serialise JSON: {error}"))?;
    println!("{text}");
    Ok(())
}

fn render_diagnostics(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        println!("{}", diagnostic.render_human());
        if let Some(path) = &diagnostic.path {
            match (diagnostic.line, diagnostic.column) {
                (Some(line), Some(column)) => println!("  --> {path}:{line}:{column}"),
                (Some(line), None) => println!("  --> {path}:{line}"),
                _ => println!("  --> {path}"),
            }
        }
    }
}

fn print_check_human(report: &CheckReport) {
    println!("Workspace: {}", report.workspace_root);
    println!("Roots:     {}", report.roots.len());
    println!("Models:    {}", report.model_count);
    println!("Sources:   {}", report.source_count);
    println!("Seeds:     {}", report.seed_count);
    println!("Tests:     {}", report.test_count);
    println!();
    println!(
        "{}",
        if report.ok {
            "check passed"
        } else {
            "check failed"
        }
    );
    println!();
}

fn print_list_human(report: &ListReport) {
    println!("Models ({})", report.models.len());
    for model in &report.models {
        let dependencies: Vec<String> = model
            .depends_on
            .iter()
            .chain(model.sources.iter())
            .cloned()
            .collect();
        let suffix = if dependencies.is_empty() {
            String::new()
        } else {
            format!(" <- {}", dependencies.join(", "))
        };
        println!(
            "  {:<28} [{}] {}{}",
            model.name, model.materialization, model.target, suffix
        );
    }

    println!();
    println!("Sources ({})", report.sources.len());
    for source in &report.sources {
        println!("  {}", source.name);
    }

    if !report.seeds.is_empty() {
        println!();
        println!("Seeds ({})", report.seeds.len());
        for seed in &report.seeds {
            let schema = seed.schema.as_deref().unwrap_or("<adapter default>");
            println!("  {:<28} {} -> {}", seed.name, seed.path, schema);
        }
    }

    println!();
    println!("Tests ({})", report.tests.len());
    for test in &report.tests {
        println!("  {}", test.name);
    }
}

fn print_inspect_human(report: &InspectReport) {
    let model = &report.model;
    println!("Model:         {}", model.name);
    println!("ID:            {}", model.id);
    println!(
        "Path:          {}",
        model.path.as_deref().unwrap_or("(in memory)")
    );
    println!("Materialized:  {}", model.materialization);
    println!("Target:        {}", model.target);
    println!(
        "Pinned:        {}",
        model.pinned_id.as_deref().unwrap_or("(none)")
    );
    println!();

    print_section("Depends on", &model.depends_on);
    print_section("Sources", &model.sources);
    print_section("Used by", &model.used_by);
    print_section("Tests", &model.tests);

    if !model.tags.is_empty() {
        print_section("Tags", &model.tags);
    }
    if let Some(owner) = &model.owner {
        println!("Owner: {owner}\n");
    }

    println!("Columns ({}):", model.columns.len());
    if model.columns.is_empty() {
        println!("  (none inferred)");
    } else {
        for column in &model.columns {
            let inputs = if column.inputs.is_empty() {
                String::new()
            } else {
                format!(" <- {}", column.inputs.join(", "))
            };
            println!(
                "  {:<20} {:<28} {}{}",
                column.name, column.data_type, column.nullability, inputs
            );
        }
    }
    println!();

    if !model.assertions.is_empty() {
        print_section("Assertions", &model.assertions);
    }
    if !model.limitations.is_empty() {
        print_section("Limitations", &model.limitations);
    }
}

fn print_plan_human(plan: &Plan) {
    println!("Plan:  {}", plan.id);
    println!("Adapter: {}", plan.adapter);
    if let Some(environment) = &plan.environment {
        println!("Environment: {environment}");
    }
    if !plan.selection.terms.is_empty() || !plan.selection.exclude.is_empty() {
        let mut line = format!("Selection: {}", plan.selection.terms.join(", "));
        if plan.selection.terms.is_empty() {
            line = "Selection: (all)".to_string();
        }
        if !plan.selection.exclude.is_empty() {
            line.push_str(&format!(
                " — excluding {}",
                plan.selection.exclude.join(", ")
            ));
        }
        println!("{line}");
    }
    if let Some(git) = &plan.git {
        println!(
            "Git changes since {} (merge-base {}):",
            git.since,
            &git.merge_base[..git.merge_base.len().min(12)]
        );
        if git.models.is_empty()
            && git.seeds.is_empty()
            && git.tests.is_empty()
            && git.deleted_models.is_empty()
            && git.unaffected.is_empty()
        {
            println!("  none");
        } else {
            for model in &git.models {
                println!("  {}", model.model);
                for cause in &model.causes {
                    println!("    {}", cause.detail);
                }
            }
            for seed in &git.seeds {
                let consumers = if seed.consumers.is_empty() {
                    " (unused)".to_string()
                } else {
                    format!(" -> {}", seed.consumers.join(", "))
                };
                let from = seed
                    .renamed_from
                    .as_deref()
                    .map(|old| format!(" (renamed from {old})"))
                    .unwrap_or_default();
                println!(
                    "  seed {}: {} {}{from}{consumers}",
                    seed.name, seed.path, seed.status
                );
            }
            for deleted in &git.deleted_models {
                match &deleted.id {
                    Some(id) => println!("  deleted {id} ({})", deleted.path),
                    None => println!("  deleted {}", deleted.path),
                }
            }
            for test in &git.tests {
                println!("  test {} {}", test.path, test.status);
            }
            for path in &git.unaffected {
                println!("  unaffected {} {}", path.path, path.status);
            }
        }
    }
    for warning in &plan.warnings {
        println!("warning: {warning}");
    }
    println!();

    if plan.blocked {
        // The errors are the actionable output — show them before the
        // model table so they are not buried under a long listing.
        println!("plan blocked by compilation errors:");
        println!();
        render_diagnostics(&plan.diagnostics);
        println!();
    }

    let mut counts = [0usize; 4]; // build, skip, cached, unknown
    for model in &plan.models {
        counts[match model.action {
            PlanAction::Build => 0,
            PlanAction::Skip => 1,
            PlanAction::Cached => 2,
            PlanAction::Unknown => 3,
        }] += 1;
    }
    println!(
        "Models ({}) — {} build, {} skip, {} cached{}",
        plan.models.len(),
        counts[0],
        counts[1],
        counts[2],
        if counts[3] > 0 {
            format!(", {} unknown", counts[3])
        } else {
            String::new()
        }
    );
    for model in &plan.models {
        let action = match model.action {
            PlanAction::Build => "BUILD",
            PlanAction::Skip => "SKIP",
            PlanAction::Cached => "CACHED",
            PlanAction::Unknown => "UNKNOWN",
        };
        println!(
            "  {:<6} {:<28} [{}] {}",
            action, model.id, model.materialization, model.target
        );
        for reason in &model.reasons {
            println!("           {}", reason.detail);
        }
        if let Some(incremental) = &model.incremental {
            println!("           strategy: {incremental}");
        }
        if model.full_rebuild {
            println!("           full rebuild required");
        }
    }

    if !plan.seeds.is_empty() {
        println!();
        println!("Seeds ({})", plan.seeds.len());
        for seed in &plan.seeds {
            let action = match seed.action {
                PlanAction::Build => "LOAD",
                PlanAction::Skip => "SKIP",
                PlanAction::Cached => "CACHED",
                PlanAction::Unknown => "UNKNOWN",
            };
            println!("  {:<6} {:<28} {}", action, seed.name, seed.target);
            for reason in &seed.reasons {
                println!("           {}", reason.detail);
            }
        }
    }

    println!();
    println!("Tests ({})", plan.tests.len());
    for test in &plan.tests {
        println!("  {}", test.id);
    }
    println!();
}

fn print_run_human(result: &RunResult) {
    println!(
        "Run {}  {}",
        &result.run_id[..result.run_id.len().min(8)],
        result.status.label().to_uppercase()
    );
    if let Some(from) = &result.continued_from {
        println!("  continues run {}", &from[..from.len().min(8)]);
    }
    println!();
    let counts = &result.counts;
    println!("{} passed", counts.passed);
    println!("{} skipped", counts.skipped);
    println!("{} cached", counts.cached);
    println!("{} failed", counts.failed);
    println!("{} blocked", counts.blocked);
    println!("{} cancelled", counts.cancelled);
    println!();
    for seed in &result.seeds {
        println!(
            "  {:<8} {:<28} {}",
            seed.status.label(),
            seed.seed,
            seed.target
        );
        if let Some(failure) = &seed.failure {
            println!(
                "           {}: {}",
                failure.category.code(),
                failure.message
            );
        }
    }
    if !result.seeds.is_empty() {
        println!();
    }
    for model in &result.models {
        println!(
            "  {:<8} {:<28} {}ms",
            model.status.label(),
            model.model,
            model.duration_ms
        );
        // Skipped and blocked models keep their plan reasons visible —
        // "unchanged" and "required by …" explain the outcome.
        if matches!(
            model.status,
            ExecutionStatus::Skipped | ExecutionStatus::Blocked
        ) {
            for reason in &model.reasons {
                println!("           {reason}");
            }
        }
        if let Some(failure) = &model.failure {
            println!(
                "           {}: {}",
                failure.category.code(),
                failure.message
            );
        }
        if model.attempts.len() > 1 {
            println!("           {} attempts", model.attempts.len());
        }
    }
    if !result.tests.is_empty() {
        println!();
        for test in &result.tests {
            println!(
                "  {:<8} {} ({} row(s))",
                test.status.label(),
                test.test,
                test.row_count
            );
            if let Some(failure) = &test.failure {
                println!(
                    "           {}: {}",
                    failure.category.code(),
                    failure.message
                );
            }
        }
    }
    if matches!(result.status, ExecutionStatus::Failed) {
        println!();
        println!(
            "  retry:  phlo-transform run --retry-failed {}",
            &result.run_id[..result.run_id.len().min(8)]
        );
        println!(
            "  resume: phlo-transform run --resume {}",
            &result.run_id[..result.run_id.len().min(8)]
        );
    }
    println!();
}

fn print_section(title: &str, values: &[String]) {
    println!("{title}:");
    if values.is_empty() {
        println!("  (none)");
    } else {
        for value in values {
            println!("  {value}");
        }
    }
    println!();
}

#[allow(clippy::too_many_arguments)]
async fn run_diff(
    cli: &Cli,
    compilation: &Compilation,
    model: Option<&str>,
    base: &Option<String>,
    to: &Option<String>,
    base_relation: &Option<String>,
    full: bool,
    partition: Option<&str>,
    sample: Option<f64>,
) -> Result<ExitCode, String> {
    let Some(model) = model else {
        let model_only = [
            (base.is_some(), "--base"),
            (base_relation.is_some(), "--base-relation"),
            (partition.is_some(), "--partition"),
            (sample.is_some(), "--sample"),
        ]
        .into_iter()
        .find_map(|(present, flag)| present.then_some(flag));
        if let Some(flag) = model_only {
            return Err(format!(
                "`{flag}` applies to model diffs; a branch diff compares two references"
            ));
        }
        return run_branch_diff(cli, compilation, to, full).await;
    };
    if to.is_some() {
        return Err("`--to` applies to branch diffs; a model diff uses `--base`".to_string());
    }
    if cli.from.is_some() {
        return Err(
            "`--from` selects a branch-diff candidate; a model diff takes its candidate from `--ref`"
                .to_string(),
        );
    }
    let id = ModelId::parse(model).map_err(|error| format!("invalid model `{model}`: {error}"))?;
    let compiled = compilation
        .model(&id)
        .ok_or_else(|| format!("no such model: {}", id.logical_name()))?;

    let adapter = build_adapter(cli)?;
    let state = open_state(cli)?;
    let candidate_ref = environment(cli);
    // `main` is the physical base when `--base` names nothing else.
    let base_ref = base.clone().unwrap_or_else(|| "main".to_string());
    let nessie_backed = nessie_endpoint(cli).is_some();

    let key_columns = model_keys(compiled);
    let columns: Vec<String> = if compiled.schema.known {
        compiled
            .schema
            .columns
            .iter()
            .map(|column| column.name.clone())
            .filter(|name| !key_columns.contains(name))
            .collect()
    } else {
        Vec::new()
    };

    // A recorded materialisation names the relation the environment actually
    // wrote, and the version that wrote it — both take precedence. Without a
    // record, a ref environment resolves through its provisioned catalog
    // (`environment_<ref>.json`, else the `phlo_<ref>` convention); with no
    // environment in play at all, the compiled target stands.
    let candidate_record = state
        .as_deref()
        .and_then(|state| {
            materialized_for_environment(state, candidate_ref.as_deref().unwrap_or("")).ok()
        })
        .and_then(|mut records| records.remove(&id.logical_name()));
    let base_record = state
        .as_deref()
        .and_then(|state| materialized_for_environment(state, &base_ref).ok())
        .and_then(|mut records| records.remove(&id.logical_name()));
    let candidate_catalog = candidate_ref.as_deref().and_then(|reference| {
        nessie_backed.then(|| {
            read_environment_for(cli, reference)
                .map(|setup| setup.catalog)
                .unwrap_or_else(|| catalog_name(reference))
        })
    });
    let base_catalog = nessie_backed.then(|| {
        if base_ref == "main" {
            cli.catalog
                .clone()
                .or_else(|| compilation_model_catalog(compilation))
                .unwrap_or_else(|| catalog_name(&base_ref))
        } else {
            read_environment_for(cli, &base_ref)
                .map(|setup| setup.catalog)
                .unwrap_or_else(|| catalog_name(&base_ref))
        }
    });
    let candidate_relation = match candidate_record
        .as_ref()
        .map(|record| Relation::parse(&record.target))
    {
        Some(Ok(relation)) => relation,
        Some(Err(_)) | None => retarget(&compiled.target, candidate_catalog.as_deref()),
    };
    let base_relation = match base_relation {
        Some(spec) => Relation::parse(spec)
            .map_err(|error| format!("invalid --base-relation `{spec}`: {error}"))?,
        None => match base_record
            .as_ref()
            .map(|record| Relation::parse(&record.target))
        {
            Some(Ok(relation)) => relation,
            Some(Err(_)) | None => retarget(&compiled.target, base_catalog.as_deref()),
        },
    };
    let partition_columns: Vec<String> = partition
        .map(|columns| {
            columns
                .split(',')
                .map(str::trim)
                .filter(|column| !column.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let strategy = if !partition_columns.is_empty() {
        DiffStrategy::Partition {
            columns: partition_columns,
        }
    } else if full {
        DiffStrategy::Full
    } else if sample.is_some() {
        DiffStrategy::Sampled
    } else if key_columns.is_empty() {
        DiffStrategy::Aggregate
    } else {
        DiffStrategy::Keyed
    };

    let request = DiffRequest {
        model: id.logical_name(),
        candidate_relation,
        base_relation,
        candidate_ref,
        base_ref: Some(base_ref),
        // The recorded materialisation versions are what the audit's
        // staleness check can actually verify — the compiled desired version
        // carries a different (default-catalog) target hash.
        candidate_version: candidate_record.map(|record| record.version.hash),
        base_version: base_record.map(|record| record.version.hash),
        key_columns,
        columns,
        strategy,
        policy: diff_policy(compiled.config.diff.as_ref()),
        sample_fraction: sample,
        renames: compiled
            .contract
            .as_ref()
            .map(|contract| contract.renames.clone())
            .unwrap_or_default(),
    };

    match diff(adapter, &request).await {
        Ok(report) => {
            ArtifactWriter::for_workspace(&cli.root)
                .write_diff(&report)
                .map_err(|error| error.to_string())?;
            if cli.json {
                print_json(&report)?;
            } else {
                print_diff_human(&report);
            }
            Ok(if report.passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Err(error) => {
            let message = error.to_string();
            if cli.json {
                print_json(&serde_json::json!({ "ok": false, "error": message }))?;
            } else {
                eprintln!("error: {message}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

fn diff_policy(spec: Option<&phlo_transform_core::DiffPolicySpec>) -> DiffPolicy {
    match spec {
        Some(spec) => DiffPolicy {
            max_added_rows: spec.max_added_rows,
            max_removed_rows: spec.max_removed_rows,
            max_modified_rows: spec.max_modified_rows,
            max_changed_fraction: spec.max_changed_fraction,
            require_full_diff: spec.require_full_diff,
            require_keyed_diff: spec.require_keyed_diff,
            tolerances: spec.tolerances.clone(),
        },
        None => DiffPolicy::default(),
    }
}

/// Branch-level diff: `phlo-transform diff --from <candidate> --to <base>`.
///
/// Candidate relations resolve through the reference's provisioned catalog
/// (`environment.json` when it names this candidate, else the `phlo_<ref>`
/// convention); base relations resolve through the workspace/default catalog
/// for `main` or the `phlo_<ref>` convention for other refs. Recorded
/// materialisation targets take precedence over both.
async fn run_branch_diff(
    cli: &Cli,
    compilation: &Compilation,
    to: &Option<String>,
    full: bool,
) -> Result<ExitCode, String> {
    let env_label = environment(cli);
    let candidate_ref = match (cli.from.as_deref(), env_label.as_deref()) {
        (Some(from), Some(label)) if from != label => {
            return Err(format!(
                "candidate given twice and disagreeing: `--from {from}` vs `{label}`"
            ));
        }
        (from, label) => from.or(label).map(str::to_string).ok_or_else(|| {
            "branch diff needs a candidate: `diff --from <ref> --to <ref>`".to_string()
        })?,
    };
    let base_ref = to.clone().unwrap_or_else(|| "main".to_string());

    // When Nessie is configured both sides must be real references — a
    // typo'd ref should error, not produce a plausible all-`removed` report.
    // The resolved heads are recorded on the report so promotion can verify
    // the audit covered exactly the commits being merged.
    let mut candidate_hash = None;
    let mut base_hash = None;
    if nessie_endpoint(cli).is_some() {
        let nessie = build_nessie(cli)?;
        for (name, slot) in [
            (&candidate_ref, &mut candidate_hash),
            (&base_ref, &mut base_hash),
        ] {
            let reference = nessie
                .get_reference(name)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    format!("reference `{name}` was not found — see `phlo-transform ref list`")
                })?;
            *slot = Some(reference.hash);
        }
    }

    let candidate_catalog = read_environment_for(cli, &candidate_ref)
        .map(|setup| setup.catalog)
        .unwrap_or_else(|| catalog_name(&candidate_ref));
    // `main` (and any ref that was never provisioned as a candidate) resolves
    // through the configured catalog; other refs use the provisioned catalog
    // or the `phlo_<ref>` convention.
    let base_catalog = if base_ref == "main" {
        cli.catalog
            .clone()
            .or_else(|| compilation_model_catalog(compilation))
    } else {
        Some(
            read_environment_for(cli, &base_ref)
                .map(|setup| setup.catalog)
                .unwrap_or_else(|| catalog_name(&base_ref)),
        )
    };

    let adapter = build_adapter(cli)?;
    let state = open_state(cli)?;
    let report = branch_diff(
        adapter,
        state.as_deref(),
        compilation,
        &BranchDiffRequest {
            candidate_ref: candidate_ref.clone(),
            base_ref: base_ref.clone(),
            candidate_catalog: Some(candidate_catalog),
            base_catalog,
            deep: full,
            default_schema: cli.trino_schema.clone(),
            candidate_hash,
            base_hash,
        },
    )
    .await
    .map_err(|error| error.to_string())?;

    ArtifactWriter::for_workspace(&cli.root)
        .write_branch_diff(&report)
        .map_err(|error| error.to_string())?;
    if cli.json {
        print_json(&report)?;
    } else {
        print_branch_diff_human(&report);
    }
    Ok(if report.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The catalog the workspace's compiled targets resolve through (the project
/// default the current compilation was built with).
fn compilation_model_catalog(compilation: &Compilation) -> Option<String> {
    compilation
        .models
        .iter()
        .find_map(|model| model.target.catalog.clone())
}

fn print_branch_diff_human(report: &BranchDiffReport) {
    println!(
        "Branch diff: {} -> {}",
        report.candidate_ref, report.base_ref
    );
    println!();
    println!("Datasets");
    for dataset in &report.datasets {
        let status = match dataset.status {
            phlo_transform_engine::DatasetStatus::Added => "added",
            phlo_transform_engine::DatasetStatus::Removed => "removed",
            phlo_transform_engine::DatasetStatus::Changed => "changed",
            phlo_transform_engine::DatasetStatus::Unchanged => "unchanged",
            phlo_transform_engine::DatasetStatus::Absent => "absent",
        };
        println!("  {:<10} {}", status, dataset.dataset);
    }
    if !report.schema_changes.is_empty() {
        println!();
        println!("Schema");
        for diff in &report.schema_changes {
            for change in &diff.changes {
                println!(
                    "  {}.{}: {} [{}]",
                    diff.model, change.column, change.detail, change.safety
                );
            }
        }
    }
    if !report.contract_changes.is_empty() {
        println!();
        println!("Contracts");
        for diff in &report.contract_changes {
            for change in &diff.changes {
                let subject = if change.column.is_empty() {
                    diff.model.clone()
                } else {
                    format!("{}.{}", diff.model, change.column)
                };
                println!(
                    "  {subject}: {} [{}]",
                    change.detail,
                    change.safety.as_str()
                );
            }
        }
    }
    if !report.rows.is_empty() {
        println!();
        println!("Rows");
        for row in &report.rows {
            let base = row
                .base_rows
                .map(|rows| rows.to_string())
                .unwrap_or_else(|| "-".to_string());
            let candidate = row
                .candidate_rows
                .map(|rows| rows.to_string())
                .unwrap_or_else(|| "-".to_string());
            let delta = row
                .delta
                .map(|delta| format!(" ({delta:+})"))
                .unwrap_or_default();
            println!(
                "  {:<32} base {base}  candidate {candidate}{delta}",
                row.dataset
            );
        }
    }
    if !report.diffs.is_empty() {
        println!();
        println!("Data diffs");
        for diff in &report.diffs {
            let verdict = if diff.passed { "PASS" } else { "FAIL" };
            println!(
                "  {verdict} {} ({} added, {} removed, {} modified)",
                diff.model,
                diff.row_summary.added,
                diff.row_summary.removed,
                diff.row_summary.modified
            );
        }
    }
    println!();
}

fn print_diff_human(report: &phlo_transform_engine::DiffReport) {
    println!("{}", report.model);
    println!("  candidate: {}", report.candidate_relation);
    println!("  base:      {}", report.base_relation);
    println!("  strategy:  {:?} ({})", report.strategy, report.coverage);
    println!();
    println!("Rows");
    println!("  base       {}", report.row_summary.base_rows);
    println!("  candidate  {}", report.row_summary.candidate_rows);
    println!("  delta      {}", report.row_summary.delta);
    println!();
    println!("Records");
    println!("  added      {}", report.row_summary.added);
    println!("  removed    {}", report.row_summary.removed);
    println!("  modified   {}", report.row_summary.modified);
    if !report.column_changes.is_empty() {
        println!();
        println!("Changed values");
        for (column, count) in &report.column_changes {
            println!("  {column:<20} {count}");
        }
    }
    if !report.partitions_added.is_empty()
        || !report.partitions_removed.is_empty()
        || !report.partitions_changed.is_empty()
    {
        println!();
        println!("Partitions");
        println!("  added    {}", report.partitions_added.len());
        println!("  removed  {}", report.partitions_removed.len());
        println!("  changed  {}", report.partitions_changed.len());
    }
    if !report.schema_changes.is_empty() {
        println!();
        println!("Schema");
        for change in &report.schema_changes {
            let marker = match change.kind.as_str() {
                "added" => "+",
                "removed" => "-",
                _ => "~",
            };
            println!("  {marker} {} ({})", change.column, change.detail);
        }
    }
    for result in &report.policy_results {
        println!(
            "  policy {:<22} {} ({})",
            result.policy,
            if result.passed { "pass" } else { "FAIL" },
            result.detail
        );
    }
    println!();
}

async fn run_daemon(cli: &Cli, port: u16, watch_interval_ms: u64) -> Result<ExitCode, String> {
    let service = WorkspaceService::load(&cli.root);
    let _watcher = spawn_watcher(
        service.clone(),
        Duration::from_millis(watch_interval_ms.max(50)),
    );
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    if !cli.json {
        eprintln!(
            "phlo-transform daemon listening on http://{address} (root {})",
            cli.root.display()
        );
    }
    serve(service, address)
        .await
        .map_err(|error| error.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// `translate --from dbt`: analyse a dbt project and optionally emit a native
/// Phlo workspace. Analysis is the default; writing requires `--out`.
fn run_translate(
    cli: &Cli,
    check: bool,
    out: Option<&PathBuf>,
    overwrite: bool,
    verify: bool,
) -> Result<ExitCode, String> {
    match cli.from.as_deref() {
        Some("dbt") => {}
        Some(other) => {
            return Err(format!(
                "unknown translation source `{other}`; only `--from dbt` is supported"
            ));
        }
        None => return Err("translate requires --from <format> (e.g. --from dbt)".to_string()),
    }

    let project = phlo_transform_dbt::load(&cli.root).map_err(|error| error.to_string())?;
    let translation = phlo_transform_dbt::translate(&project);

    let mut wrote = false;
    let mut verify_report: Option<CheckReport> = None;
    if !check {
        let out = out.ok_or_else(|| {
            "writing requires --out <dir> (or use --check for analysis only)".to_string()
        })?;
        let conflicts: Vec<String> = translation
            .files
            .iter()
            .map(|file| file.rel_path.clone())
            .filter(|rel| out.join(rel).exists())
            .collect();
        if !overwrite && !conflicts.is_empty() {
            return Err(format!(
                "refusing to overwrite {} existing file(s) in {}: {} (pass --overwrite)",
                conflicts.len(),
                out.display(),
                conflicts.join(", ")
            ));
        }
        phlo_transform_dbt::write_translation(out, &translation)
            .map_err(|error| error.to_string())?;
        wrote = true;

        if verify {
            match load_project(out) {
                Ok(generated) => {
                    verify_report = Some(compile(&generated).check_report());
                }
                Err(diagnostics) => {
                    verify_report = Some(CheckReport {
                        ok: false,
                        workspace_root: out.to_string_lossy().to_string(),
                        roots: Vec::new(),
                        model_count: 0,
                        source_count: 0,
                        seed_count: 0,
                        test_count: 0,
                        diagnostics,
                    });
                }
            }
        }
    }

    if cli.json {
        let mut value = serde_json::to_value(&translation.report)
            .map_err(|error| format!("could not serialise JSON: {error}"))?;
        value["wrote_files"] = serde_json::json!(wrote);
        if let Some(report) = &verify_report {
            value["generated_check"] = serde_json::to_value(report)
                .map_err(|error| format!("could not serialise JSON: {error}"))?;
        }
        print_json(&value)?;
    } else {
        print!("{}", translation.report.render_human());
        if wrote {
            println!(
                "\nWrote {} file(s) to {}",
                translation.files.len(),
                out.map(|o| o.display().to_string()).unwrap_or_default()
            );
            println!("Manifest: .phlo/migration/dbt-translation.json");
        }
        if let Some(report) = &verify_report {
            println!();
            print_check_human(report);
            if !report.ok {
                render_diagnostics(&report.diagnostics);
            }
        }
    }
    Ok(match &verify_report {
        Some(report) if !report.ok => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    })
}

/// `init`: scaffold a minimal runnable workspace.
fn run_init(cli: &Cli) -> Result<ExitCode, String> {
    let root = &cli.root;
    if root.join("phlo.toml").exists() || root.join("transforms").exists() {
        return Err(format!(
            "{} already looks like a Phlo workspace (phlo.toml or transforms/ exists)",
            root.display()
        ));
    }
    std::fs::create_dir_all(root.join("transforms/example"))
        .and_then(|_| std::fs::create_dir_all(root.join("tests")))
        .map_err(|error| format!("could not create workspace: {error}"))?;

    std::fs::write(
        root.join("phlo.toml"),
        "[transform]\n# default_namespace = \"main\"   # for models directly under transforms/\n# default_materialization = \"view\"\n",
    )
    .map_err(|error| error.to_string())?;
    std::fs::write(
        root.join("transforms/example/raw_events.sql"),
        "-- @table\n-- @key id\n\nselect 1 as id, 'signup' as kind, date '2024-01-01' as ts\nunion all\nselect 2, 'purchase', date '2024-01-01'\nunion all\nselect 3, 'signup', date '2024-01-02'\n",
    )
    .map_err(|error| error.to_string())?;
    std::fs::write(
        root.join("transforms/example/daily_events.sql"),
        "select\n    ts,\n    kind,\n    count(*) as events\nfrom example.raw_events\ngroup by ts, kind\n",
    )
    .map_err(|error| error.to_string())?;
    std::fs::write(
        root.join("tests/daily_events_id_present.sql"),
        "select * from example.daily_events where ts is null or kind is null\n",
    )
    .map_err(|error| error.to_string())?;

    // Keep the state database and generated artifacts out of version control.
    let gitignore = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if !existing.lines().any(|line| line.trim() == ".phlo/") {
        let mut contents = existing;
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(".phlo/\n");
        std::fs::write(&gitignore, contents).map_err(|error| error.to_string())?;
    }

    let payload = serde_json::json!({
        "ok": true,
        "root": root.display().to_string(),
        "files": [
            "phlo.toml",
            "transforms/example/raw_events.sql",
            "transforms/example/daily_events.sql",
            "tests/daily_events_id_present.sql",
        ],
        "next": [
            "phlo-transform check",
            "phlo-transform run --adapter duckdb",
        ],
    });
    if cli.json {
        print_json(&payload)?;
    } else {
        println!("Initialised a Phlo workspace at {}", root.display());
        println!();
        println!("Next:");
        println!("  phlo-transform check");
        println!("  phlo-transform run --adapter duckdb");
    }
    Ok(ExitCode::SUCCESS)
}

/// A single doctor check result.
#[derive(serde::Serialize)]
struct DoctorCheck {
    name: &'static str,
    /// `ok`, `warn`, or `fail`.
    status: &'static str,
    detail: String,
}

/// `doctor`: diagnose the workspace, configuration, and adapter.
async fn run_doctor(cli: &Cli) -> Result<ExitCode, String> {
    let mut checks = Vec::new();
    let mut failed = false;

    let mut record = |check: DoctorCheck| {
        if check.status == "fail" {
            failed = true;
        }
        checks.push(check);
    };

    // 1. Workspace root.
    let looks_like_workspace = cli.root.join("phlo.toml").exists()
        || cli.root.join("transforms").is_dir()
        || cli.root.join("workflows").is_dir();
    record(DoctorCheck {
        name: "workspace",
        status: if cli.root.is_dir() && looks_like_workspace {
            "ok"
        } else {
            "fail"
        },
        detail: if looks_like_workspace {
            format!("{}", cli.root.display())
        } else {
            format!(
                "no phlo.toml, transforms/ or workflows/ under {}; run `phlo-transform init` or pass `-r <workspace>`",
                cli.root.display()
            )
        },
    });

    // 2. Project discovery + compilation.
    match load_project(&cli.root) {
        Ok(project) => {
            let compilation = compile(&project);
            let check = compilation.check_report();
            let errors: Vec<&Diagnostic> = compilation
                .diagnostics
                .iter()
                .filter(|d| matches!(d.severity, phlo_transform_core::Severity::Error))
                .collect();
            let mut detail = format!(
                "{} models, {} sources, {} tests; {} error(s)",
                check.model_count,
                check.source_count,
                check.test_count,
                errors.len()
            );
            if let Some(first) = errors.first() {
                detail.push_str(&format!(" — first: [{}] {}", first.code, first.message));
            }
            if !errors.is_empty() {
                detail.push_str("; run `phlo-transform check`");
            }
            record(DoctorCheck {
                name: "compile",
                status: if compilation.is_ok() { "ok" } else { "fail" },
                detail,
            });
        }
        Err(diagnostics) => {
            record(DoctorCheck {
                name: "compile",
                status: "fail",
                detail: format!("project failed to load ({} diagnostics)", diagnostics.len()),
            });
        }
    }

    // 3. Adapter connectivity.
    match build_adapter(cli) {
        Ok(adapter) => {
            let name = adapter.name().to_string();
            match adapter.execute("select 1").await {
                Ok(_) => record(DoctorCheck {
                    name: "adapter",
                    status: "ok",
                    detail: format!("{name} is reachable"),
                }),
                Err(error) => record(DoctorCheck {
                    name: "adapter",
                    status: "fail",
                    detail: format!("{name} connection failed: {error}"),
                }),
            }
        }
        Err(error) => record(DoctorCheck {
            name: "adapter",
            status: "warn",
            detail: format!("{error} (only needed for plan/apply/test)"),
        }),
    }

    // 4. State store.
    match open_state(cli) {
        Ok(Some(_)) => {
            let location = cli
                .state
                .clone()
                .or_else(|| std::env::var("PHLO_STATE_URL").ok())
                .map(|location| state_location_display(&location))
                .unwrap_or_else(|| state_path(cli).display().to_string());
            record(DoctorCheck {
                name: "state",
                status: "ok",
                detail: location,
            })
        }
        Ok(None) => record(DoctorCheck {
            name: "state",
            status: "warn",
            detail: format!(
                "could not open {} (runs will not be recorded)",
                state_path(cli).display()
            ),
        }),
        Err(error) => record(DoctorCheck {
            name: "state",
            status: "fail",
            detail: error,
        }),
    }

    if cli.json {
        print_json(&serde_json::json!({ "ok": !failed, "checks": checks }))?;
    } else {
        for check in &checks {
            println!(
                "  {:<4} {:<10} {}",
                check.status.to_uppercase(),
                check.name,
                check.detail
            );
        }
        println!();
        println!(
            "{}",
            if failed {
                "doctor found problems"
            } else {
                "everything looks healthy"
            }
        );
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `manifest`: print the generated manifest artifact.
fn run_manifest(cli: &Cli) -> Result<ExitCode, String> {
    let path = artifact_path(cli, "manifest.json");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "no manifest at {}; run `phlo-transform plan` first",
            path.display()
        )
    })?;
    if cli.json {
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| format!("invalid manifest: {error}"))?;
        print_json(&value)?;
    } else {
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| format!("invalid manifest: {error}"))?;
        println!("Manifest: {}", path.display());
        if let Some(models) = value.get("models").and_then(|m| m.as_array()) {
            println!("Models ({})", models.len());
            for model in models {
                println!(
                    "  {:<28} [{}] {}",
                    model.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                    model
                        .get("materialization")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?"),
                    model.get("target").and_then(|v| v.as_str()).unwrap_or("?")
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `explain <model>`: identity, state and the current plan decision.
///
/// The state comparison (which version inputs moved since the recorded
/// materialisation) needs no adapter; when one is configured the planner
/// also checks whether the target relation exists, so the decision is the
/// same one `plan` would report.
async fn run_explain(
    cli: &Cli,
    compilation: &Compilation,
    model: &str,
) -> Result<ExitCode, String> {
    let id = resolve_model(model, compilation)?;
    let Some(report) = compilation.inspect_report(&id) else {
        return Err(format!("no such model: {}", id.logical_name()));
    };
    let diagnostics = model_diagnostics(compilation, &report);
    let compiled = compilation.model(&id).expect("inspect report exists");

    let state = open_state(cli)?;
    let env = environment(cli);
    let current = state.as_ref().and_then(|state| {
        state
            .materialized_version(&id.logical_name(), env.as_deref())
            .ok()
            .flatten()
    });

    // Which version inputs moved since the recorded materialisation —
    // the same reasons `plan` reports for a hash mismatch.
    let state_reasons: Vec<PlanReason> = match &current {
        Some(record) if record.version.hash == compiled.version.hash => vec![PlanReason {
            kind: ReasonKind::Unchanged,
            detail: "SQL, config, contract and inputs unchanged".to_string(),
            subject: None,
        }],
        Some(record) => diff_reasons(compiled, record),
        None => vec![PlanReason {
            kind: if state.is_some() {
                ReasonKind::UnknownState
            } else {
                ReasonKind::StateUnavailable
            },
            detail: if state.is_some() {
                "no version recorded for this environment".to_string()
            } else {
                "no state store; cannot compare against a recorded version".to_string()
            },
            subject: None,
        }],
    };

    // The full decision needs relation existence, which needs an adapter.
    let mut plan_model: Option<phlo_transform_engine::PlannedModel> = None;
    if let Ok(adapter) = build_adapter(cli) {
        let planner = Planner::new(adapter, state.clone());
        let scoped = Selection::of(compilation, std::slice::from_ref(&id));
        if let Ok(plan) = planner
            .plan(
                compilation,
                &scoped,
                env.clone(),
                &PlanOptions { force: cli.force },
            )
            .await
        {
            plan_model = plan
                .models
                .iter()
                .find(|entry| entry.id == id.logical_name())
                .cloned();
        }
    }

    if cli.json {
        print_json(&serde_json::json!({
            "model": report,
            "version": compiled.version.hash,
            "state": {
                "environment": env,
                "recorded_version": current.as_ref().map(|record| record.version.hash.clone()),
                "materialized_at": current.as_ref().map(|record| record.materialized_at.clone()),
                "target": current.as_ref().map(|record| record.target.clone()),
                "reasons": state_reasons,
            },
            "plan": plan_model,
            "diagnostics": diagnostics,
        }))?;
    } else {
        print_inspect_human(&report);
        println!("Version:       {}", compiled.version.short());
        println!(
            "Recorded:      {}",
            current
                .as_ref()
                .map(|record| format!(
                    "{} in {} (materialised {})",
                    record.version.short(),
                    record.environment.as_deref().unwrap_or("default"),
                    record.materialized_at
                ))
                .unwrap_or_else(|| "(none)".to_string())
        );
        match &plan_model {
            Some(entry) => {
                println!(
                    "Decision:      {}",
                    match entry.action {
                        PlanAction::Build => "build",
                        PlanAction::Skip => "skip",
                        PlanAction::Cached => "cached",
                        PlanAction::Unknown => "unknown",
                    }
                );
                if entry.membership != Membership::Selected {
                    println!(
                        "Selection:     {}",
                        match entry.membership {
                            Membership::Expanded => "via `+` expansion",
                            Membership::Dependency => "dependency of the selection",
                            Membership::Selected => unreachable!(),
                        }
                    );
                }
                if !entry.reasons.is_empty() {
                    println!("Reasons:");
                    for reason in &entry.reasons {
                        println!("  {}", reason.detail);
                    }
                }
            }
            None => {
                println!("Decision:      (needs an adapter to check the target relation)");
                if !state_reasons.is_empty() {
                    println!("Versus recorded:");
                    for reason in &state_reasons {
                        println!("  {}", reason.detail);
                    }
                }
            }
        }
        if !diagnostics.is_empty() {
            println!();
            render_diagnostics(&diagnostics);
        }
        println!();
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phlo_transform_engine::{
        BranchDiffReport, DatasetDiff, DatasetKind, DatasetStatus, EngineError, MaterializedRecord,
        ModelSchemaDiff, SchemaChange, SqliteStateStore,
    };

    fn cli_at(root: &std::path::Path) -> Cli {
        Cli::try_parse_from([
            "phlo-transform",
            "--root",
            root.to_str().expect("utf-8 root"),
            "check",
        ])
        .expect("cli parses")
    }

    fn branch_report(candidate: &str, base: &str, deep: bool) -> BranchDiffReport {
        BranchDiffReport {
            candidate_ref: candidate.to_string(),
            base_ref: base.to_string(),
            candidate_hash: None,
            base_hash: None,
            datasets: Vec::new(),
            schema_changes: Vec::new(),
            contract_changes: Vec::new(),
            impacts: Vec::new(),
            rows: Vec::new(),
            diffs: Vec::new(),
            deep,
            passed: true,
            started_at: "t".to_string(),
            finished_at: "t".to_string(),
        }
    }

    fn dataset(name: &str, candidate_version: Option<&str>) -> DatasetDiff {
        DatasetDiff {
            dataset: name.to_string(),
            kind: DatasetKind::Model,
            status: DatasetStatus::Changed,
            candidate_version: candidate_version.map(str::to_string),
            base_version: Some("v1".to_string()),
            candidate_relation: None,
            base_relation: None,
            upstream: Vec::new(),
        }
    }

    fn write_branch_diff(cli: &Cli, report: &BranchDiffReport) {
        ArtifactWriter::for_workspace(&cli.root)
            .write_branch_diff(report)
            .expect("artifact writes");
    }

    fn write_diff_json(cli: &Cli, candidate_ref: Option<&str>, base_ref: Option<&str>) {
        let directory = cli.root.join(".phlo").join("transform");
        std::fs::create_dir_all(&directory).expect("artifact dir");
        std::fs::write(
            directory.join("diff.json"),
            serde_json::json!({
                "schema_version": 1,
                "diff": {
                    "model": "m.a",
                    "candidate_ref": candidate_ref,
                    "base_ref": base_ref,
                    "candidate_version": "v1",
                    "passed": true,
                }
            })
            .to_string(),
        )
        .expect("diff.json writes");
    }

    #[test]
    fn audited_diff_rejects_an_artifact_for_other_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        // A deep, passing diff — for the wrong pair — must not authorise this
        // promotion, and its breaking changes must not leak in either.
        let mut report = branch_report("ci/other", "main", true);
        report.schema_changes.push(ModelSchemaDiff {
            model: "m.a".to_string(),
            changes: vec![SchemaChange {
                column: "id".to_string(),
                kind: "changed".to_string(),
                detail: "removed".to_string(),
                safety: "full_rebuild_required".to_string(),
            }],
        });
        write_branch_diff(&cli, &report);

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", None, None);
        let (passed, rejected, breaking) = (
            evidence.diff_passed,
            evidence.diff_rejected,
            evidence.breaking_schema_changes,
        );
        assert_eq!(passed, None);
        let reason = rejected.expect("other-ref artifact must be rejected");
        assert!(reason.contains("ci/other"), "{reason}");
        assert!(breaking.is_empty());
    }

    #[test]
    fn audited_diff_rejects_an_audit_against_another_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        // Audited against `dev`; promoting to `main` must not consume it.
        write_branch_diff(&cli, &branch_report("ci/x", "dev", true));

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", None, None);
        let (passed, rejected) = (evidence.diff_passed, evidence.diff_rejected);
        assert_eq!(passed, None);
        let reason = rejected.expect("wrong-target artifact must be rejected");
        assert!(reason.contains("dev"), "{reason}");
    }

    #[test]
    fn audited_diff_requires_a_value_level_branch_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        // A shallow (schema + row-count) diff passes no data-diff verdict.
        write_branch_diff(&cli, &branch_report("ci/x", "main", false));

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", None, None);
        let (passed, rejected) = (evidence.diff_passed, evidence.diff_rejected);
        assert_eq!(passed, None);
        let reason = rejected.expect("shallow diff must not satisfy require-diff");
        assert!(reason.contains("--full"), "{reason}");
    }

    #[test]
    fn audited_diff_accepts_a_deep_diff_for_these_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        write_branch_diff(&cli, &branch_report("ci/x", "main", true));

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", None, None);
        let (passed, rejected) = (evidence.diff_passed, evidence.diff_rejected);
        assert_eq!(passed, Some(true));
        assert_eq!(rejected, None);
    }

    #[test]
    fn audited_diff_rejects_a_stale_candidate_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        let mut report = branch_report("ci/x", "main", true);
        report.datasets.push(dataset("m.a", Some("v1")));
        write_branch_diff(&cli, &report);

        state
            .record_materialized(&MaterializedRecord {
                model_id: "m.a".to_string(),
                environment: Some("ci/x".to_string()),
                version: phlo_transform_core::ModelVersion {
                    hash: "v2".to_string(),
                    ..Default::default()
                },
                detail: None,
                target: "cat.m.a".to_string(),
                incremental_strategy: None,
                incremental_key: None,
                adapter: None,
                output_identity: None,
                contract: None,
                effective_key: None,
                run_id: "run-1".to_string(),
                materialized_at: "t".to_string(),
            })
            .expect("record materialised");

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", None, None);
        let (passed, rejected) = (evidence.diff_passed, evidence.diff_rejected);
        assert_eq!(passed, Some(true));
        let reason = rejected.expect("stale artifact must be rejected");
        assert!(reason.contains("stale"), "{reason}");
    }

    #[test]
    fn a_single_model_diff_is_not_promotion_evidence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        // However well-scoped the model diff was — matching refs, passing
        // verdict — it examined one model and cannot certify a branch.
        write_diff_json(&cli, Some("ci/x"), Some("main"));

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", None, None);
        assert_eq!(evidence.diff_passed, None);
        assert!(!evidence.schema_audited);
        let reason = evidence
            .diff_rejected
            .expect("a model diff must be rejected as branch evidence");
        assert!(reason.contains("single-model"), "{reason}");
    }

    fn record(model_id: &str, env: Option<&str>, hash: &str) -> MaterializedRecord {
        MaterializedRecord {
            model_id: model_id.to_string(),
            environment: env.map(str::to_string),
            version: phlo_transform_core::ModelVersion {
                hash: hash.to_string(),
                ..Default::default()
            },
            detail: None,
            target: "cat.m.a".to_string(),
            incremental_strategy: None,
            incremental_key: None,
            adapter: None,
            output_identity: None,
            contract: None,
            // A current-era record: recorded keyless, not legacy-unknown.
            effective_key: Some(Vec::new()),
            run_id: "run-1".to_string(),
            materialized_at: "t".to_string(),
        }
    }

    #[test]
    fn audited_diff_rejects_a_stale_base_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        let mut report = branch_report("ci/x", "main", true);
        report.datasets.push(dataset("m.a", Some("v1")));
        write_branch_diff(&cli, &report);

        // The candidate still matches the audit; the base moved on.
        state
            .record_materialized(&record("m.a", Some("ci/x"), "v1"))
            .expect("record");
        state
            .record_materialized(&record("m.a", Some("main"), "v2"))
            .expect("record");

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", None, None);
        let (passed, rejected) = (evidence.diff_passed, evidence.diff_rejected);
        assert_eq!(passed, Some(true));
        let reason = rejected.expect("stale base must be rejected");
        assert!(reason.contains("stale"), "{reason}");
        assert!(reason.contains("main"), "{reason}");
    }

    #[test]
    fn audited_diff_rejects_a_dataset_materialised_after_the_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        // The report covers no datasets, but the candidate has since
        // materialised one — the audit cannot speak for it.
        write_branch_diff(&cli, &branch_report("ci/x", "main", true));
        state
            .record_materialized(&record("m.b", Some("ci/x"), "v9"))
            .expect("record");

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", None, None);
        let rejected = evidence.diff_rejected;
        let reason = rejected.expect("uncovered materialisation must be rejected");
        assert!(reason.contains("materialised on the candidate"), "{reason}");
    }

    #[test]
    fn audited_diff_rejects_a_diff_of_an_older_candidate_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        // The artifact audited candidate@c1; the branch has moved to c2.
        let mut report = branch_report("ci/x", "main", true);
        report.candidate_hash = Some("c1".to_string());
        report.base_hash = Some("b1".to_string());
        write_branch_diff(&cli, &report);

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", Some("c2"), Some("b1"));
        let reason = evidence
            .diff_rejected
            .expect("stale-commit artifact must be rejected");
        assert!(reason.contains("c1"), "{reason}");
        assert!(reason.contains("c2"), "{reason}");
        assert!(!evidence.schema_audited, "stale evidence audits nothing");
    }

    #[test]
    fn audited_diff_rejects_an_artifact_without_commit_binding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        // An artifact written before hash binding cannot prove which commits
        // it audited — rejected once the heads are known.
        write_branch_diff(&cli, &branch_report("ci/x", "main", true));

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", Some("c1"), Some("b1"));
        let reason = evidence
            .diff_rejected
            .expect("unbound artifact must be rejected");
        assert!(reason.contains("does not record"), "{reason}");
        assert!(!evidence.schema_audited);
    }

    #[test]
    fn audited_diff_accepts_a_commit_bound_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        let mut report = branch_report("ci/x", "main", true);
        report.candidate_hash = Some("c1".to_string());
        report.base_hash = Some("b1".to_string());
        write_branch_diff(&cli, &report);

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", Some("c1"), Some("b1"));
        assert_eq!(evidence.diff_rejected, None);
        assert_eq!(evidence.diff_passed, Some(true));
        assert!(evidence.schema_audited);
        // The audited base commit is surfaced as `base`-gate provenance.
        assert_eq!(evidence.audited_base_hash.as_deref(), Some("b1"));
    }

    #[test]
    fn audited_diff_rejects_a_diff_of_an_older_base_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        let mut report = branch_report("ci/x", "main", true);
        report.candidate_hash = Some("c1".to_string());
        report.base_hash = Some("b1".to_string());
        write_branch_diff(&cli, &report);

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", Some("c1"), Some("b2"));
        let reason = evidence
            .diff_rejected
            .expect("stale-base artifact must be rejected");
        assert!(reason.contains("b1"), "{reason}");
        assert!(reason.contains("b2"), "{reason}");
        // The audited base still reports b1 — the `base` gate compares it
        // against the live head and fails "target advanced".
        assert_eq!(evidence.audited_base_hash.as_deref(), Some("b1"));
    }

    #[test]
    fn a_fresh_shallow_diff_still_audits_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let state = SqliteStateStore::in_memory().expect("state");

        // Shallow artifacts are rejected as *data-diff* evidence (no policies
        // evaluated) but the schema comparison genuinely ran — the schema
        // gate may still stand on it.
        let mut report = branch_report("ci/x", "main", false);
        report.candidate_hash = Some("c1".to_string());
        report.base_hash = Some("b1".to_string());
        write_branch_diff(&cli, &report);

        let evidence = audited_diff(&cli, Some(&state), "ci/x", "main", Some("c1"), Some("b1"));
        assert!(
            evidence.diff_rejected.is_some(),
            "shallow cannot satisfy require-diff"
        );
        assert!(evidence.schema_audited, "the schema pass ran");
    }

    #[test]
    fn read_environment_for_prefers_the_per_candidate_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let setup = EnvironmentSetup {
            base: phlo_transform_nessie::ReferenceInfo::branch("main", "aaa"),
            candidate: phlo_transform_nessie::ReferenceInfo::branch("ci/x", "bbb"),
            created_from: Some(phlo_transform_nessie::ReferenceInfo::branch("main", "aaa")),
            created_branch: true,
            catalog: "custom_catalog".to_string(),
        };
        write_environment_artifacts(&cli, &setup).expect("writes");

        // Both files exist: the single-slot record and the per-candidate one.
        assert!(artifact_path(&cli, "environment.json").exists());
        assert!(artifact_path(&cli, &environment_artifact_name("ci/x")).exists());

        // Provisioning a different candidate overwrites the single-slot
        // record but not ci/x's evidence.
        let other = EnvironmentSetup {
            base: phlo_transform_nessie::ReferenceInfo::branch("main", "aaa"),
            candidate: phlo_transform_nessie::ReferenceInfo::branch("ci/y", "ccc"),
            created_from: Some(phlo_transform_nessie::ReferenceInfo::branch("main", "aaa")),
            created_branch: true,
            catalog: "phlo_ci_y".to_string(),
        };
        write_environment_artifacts(&cli, &other).expect("writes");

        let found = read_environment_for(&cli, "ci/x").expect("ci/x evidence");
        assert_eq!(found.catalog, "custom_catalog");
        assert_eq!(found.candidate.hash, "bbb");
        let found = read_environment_for(&cli, "ci/y").expect("ci/y evidence");
        assert_eq!(found.catalog, "phlo_ci_y");
        assert!(read_environment_for(&cli, "ci/unknown").is_none());
    }

    #[test]
    fn environment_artifact_name_sanitizes() {
        let name = environment_artifact_name("ci/pr-1");
        assert!(name.starts_with("environment_ci_pr_1_"), "{name}");
        assert!(name.ends_with(".json"), "{name}");
        let name = environment_artifact_name("feature/ABC-123");
        assert!(name.starts_with("environment_feature_abc_123_"), "{name}");

        // Refs that fold to the same readable name must not share a file.
        assert_ne!(
            environment_artifact_name("ci/pr-1"),
            environment_artifact_name("ci_pr_1")
        );
        // Nor may a degenerate ref collapse onto the single-slot artifact.
        assert_ne!(environment_artifact_name("///"), "environment.json");
    }

    fn contract(names: &[&str]) -> phlo_transform_core::ModelContract {
        phlo_transform_core::ModelContract {
            enforced: true,
            columns: names
                .iter()
                .map(|name| phlo_transform_core::ColumnContract {
                    name: (*name).to_string(),
                    data_type: None,
                    nullable: None,
                })
                .collect(),
            renames: Default::default(),
        }
    }

    #[test]
    fn contract_breaking_changes_reads_recorded_contracts() {
        let state = SqliteStateStore::in_memory().expect("state");
        let mut main_record = record("main.base", Some("main"), "v1");
        main_record.contract = Some(contract(&["id", "legacy"]));
        state.record_materialized(&main_record).expect("record");

        // The workspace now declares a contract without `legacy` — removing
        // it is a breaking change against the recorded base contract.
        let mut base = phlo_transform_core::SemanticModel::in_memory(
            ModelId::parse("main.base").expect("model id"),
            "select 1 as id, 2 as legacy",
        );
        base.contract = Some(contract(&["id"]));
        let compilation = compile(&phlo_transform_core::SemanticProject::in_memory(vec![base]));
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

        let breaks = contract_breaking_changes(Some(&state), &compilation, "main").expect("breaks");
        assert_eq!(breaks.len(), 1, "{breaks:?}");
        assert!(breaks[0].contains("legacy"), "{breaks:?}");

        // The default environment folds into `main`, so a default-env
        // record feeds the same gate.
        let state = SqliteStateStore::in_memory().expect("state");
        let mut default_record = record("main.base", None, "v1");
        default_record.contract = Some(contract(&["id", "legacy"]));
        state.record_materialized(&default_record).expect("record");
        let breaks = contract_breaking_changes(Some(&state), &compilation, "main").expect("breaks");
        assert_eq!(breaks.len(), 1, "{breaks:?}");
    }

    struct FailingStore;
    impl StateStore for FailingStore {
        fn start_run(
            &self,
            _run: &phlo_transform_engine::RunRecord,
            _plan: &phlo_transform_engine::StoredPlan,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn reopen_run(&self, _run_id: &str) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn bind_run_reference_hash(
            &self,
            _run_id: &str,
            _reference_hash: &str,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn finish_run(
            &self,
            _run_id: &str,
            _status: phlo_transform_engine::ExecutionStatus,
            _finished_at: &str,
            _failed_count: usize,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn record_model(
            &self,
            _record: &phlo_transform_engine::ModelRunRecord,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn record_seed_run(
            &self,
            _record: &phlo_transform_engine::SeedRunRecord,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn record_test(
            &self,
            _record: &phlo_transform_engine::TestRunRecord,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn runs(&self) -> Result<Vec<phlo_transform_engine::RunSummary>, EngineError> {
            unimplemented!()
        }
        fn latest_run(
            &self,
            _environment: Option<&str>,
        ) -> Result<Option<phlo_transform_engine::RunSummary>, EngineError> {
            unimplemented!()
        }
        fn run(
            &self,
            _run_id: &str,
        ) -> Result<Option<phlo_transform_engine::StoredRun>, EngineError> {
            unimplemented!()
        }
        fn find_runs(
            &self,
            _prefix: &str,
        ) -> Result<Vec<phlo_transform_engine::RunSummary>, EngineError> {
            unimplemented!()
        }
        fn model_runs(
            &self,
            _run_id: &str,
        ) -> Result<Vec<phlo_transform_engine::ModelRunRecord>, EngineError> {
            unimplemented!()
        }
        fn seed_runs(
            &self,
            _run_id: &str,
        ) -> Result<Vec<phlo_transform_engine::SeedRunRecord>, EngineError> {
            unimplemented!()
        }
        fn test_runs(
            &self,
            _run_id: &str,
        ) -> Result<Vec<phlo_transform_engine::TestRunRecord>, EngineError> {
            unimplemented!()
        }
        fn record_materialized(&self, _record: &MaterializedRecord) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn materialized_version(
            &self,
            _model_id: &str,
            _environment: Option<&str>,
        ) -> Result<Option<MaterializedRecord>, EngineError> {
            unimplemented!()
        }
        fn materialized_by_hash(
            &self,
            _version_hash: &str,
        ) -> Result<Vec<MaterializedRecord>, EngineError> {
            unimplemented!()
        }
        // The one method the promotion evidence path exercises — it fails.
        fn materialized_in(
            &self,
            _environment: Option<&str>,
        ) -> Result<Vec<MaterializedRecord>, EngineError> {
            Err(EngineError::State("state store unreachable".to_string()))
        }
        fn record_promotion(
            &self,
            _record: &phlo_transform_engine::PromotionRecord,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn promotions(&self) -> Result<Vec<phlo_transform_engine::PromotionRecord>, EngineError> {
            unimplemented!()
        }
        fn set_watermark(
            &self,
            _model_id: &str,
            _environment: Option<&str>,
            _value: &str,
            _run_id: &str,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn watermark(
            &self,
            _model_id: &str,
            _environment: Option<&str>,
        ) -> Result<Option<String>, EngineError> {
            unimplemented!()
        }
        fn record_seed(
            &self,
            _record: &phlo_transform_engine::SeedRecord,
        ) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn seed_state(
            &self,
            _name: &str,
            _environment: Option<&str>,
        ) -> Result<Option<phlo_transform_engine::SeedRecord>, EngineError> {
            unimplemented!()
        }
        fn seeds_in(
            &self,
            _environment: Option<&str>,
        ) -> Result<Vec<phlo_transform_engine::SeedRecord>, EngineError> {
            unimplemented!()
        }
    }

    #[test]
    fn contract_breaking_changes_fails_closed_on_state_error() {
        // A store whose read fails must fail promotion — never read as
        // "no contracts recorded".
        let model = phlo_transform_core::SemanticModel::in_memory(
            ModelId::parse("main.base").expect("model id"),
            "select 1 as id",
        );
        let compilation = compile(&phlo_transform_core::SemanticProject::in_memory(vec![
            model,
        ]));
        let result = contract_breaking_changes(Some(&FailingStore), &compilation, "main");
        let error = result.expect_err("a state error must fail promotion");
        assert!(error.contains("unreachable"), "{error}");
    }

    #[test]
    fn contract_breaking_changes_reports_key_changes() {
        // A legacy record — written before effective keys were persisted —
        // still proves its incremental key: `batch_id` recorded → `id`
        // desired is a breaking key change, not a keyless base.
        let state = SqliteStateStore::in_memory().expect("state");
        let mut main_record = record("main.base", Some("main"), "v1");
        main_record.incremental_strategy = Some("key".to_string());
        main_record.incremental_key = Some("batch_id".to_string());
        main_record.effective_key = None; // predates the column
        state.record_materialized(&main_record).expect("record");

        let mut base = phlo_transform_core::SemanticModel::in_memory(
            ModelId::parse("main.base").expect("model id"),
            "select 1 as id",
        );
        base.config.incremental = Some(phlo_transform_core::IncrementalStrategy::Key {
            columns: vec!["id".to_string()],
        });
        let compilation = compile(&phlo_transform_core::SemanticProject::in_memory(vec![base]));
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

        let breaks = contract_breaking_changes(Some(&state), &compilation, "main").expect("breaks");
        assert!(
            breaks.iter().any(|change| change.contains("key changed")),
            "{breaks:?}"
        );
    }

    #[test]
    fn contract_breaking_changes_reports_persisted_key_changes() {
        // The persisted effective key — not the incremental fields — is the
        // historical key: a `unique(sample_id)` assertion alone materialised
        // with `effective_key = [[sample_id]]`, no incremental strategy.
        let mut keyed = phlo_transform_core::SemanticModel::in_memory(
            ModelId::parse("main.base").expect("model id"),
            "select 1 as id",
        );
        keyed.directives.keys.push("batch_id".to_string());
        let compilation = compile(&phlo_transform_core::SemanticProject::in_memory(vec![
            keyed,
        ]));
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

        // Changed: recorded [[sample_id]] vs desired [[batch_id]].
        let state = SqliteStateStore::in_memory().expect("state");
        let mut main_record = record("main.base", Some("main"), "v1");
        main_record.effective_key = Some(vec![vec!["sample_id".to_string()]]);
        state.record_materialized(&main_record).expect("record");
        let breaks = contract_breaking_changes(Some(&state), &compilation, "main").expect("breaks");
        assert!(
            breaks.iter().any(|change| change.contains("key changed")),
            "{breaks:?}"
        );

        // Removed: the workspace model declares no key at all.
        let keyless = phlo_transform_core::SemanticModel::in_memory(
            ModelId::parse("main.base").expect("model id"),
            "select 1 as id",
        );
        let compilation = compile(&phlo_transform_core::SemanticProject::in_memory(vec![
            keyless,
        ]));
        let breaks = contract_breaking_changes(Some(&state), &compilation, "main").expect("breaks");
        assert!(
            breaks
                .iter()
                .any(|change| change.contains("key") && change.contains("dropped")),
            "{breaks:?}"
        );
    }

    #[test]
    fn contract_breaking_changes_treats_incremental_and_unique_keys_equally() {
        // The base materialised with `key = "id"` — a legacy record whose
        // only key evidence is the incremental fields; the workspace now
        // claims the same identity through a `unique(id)` assertion —
        // same effective key, no change.
        let state = SqliteStateStore::in_memory().expect("state");
        let mut main_record = record("main.base", Some("main"), "v1");
        main_record.incremental_strategy = Some("key".to_string());
        main_record.incremental_key = Some("id".to_string());
        main_record.effective_key = None; // predates the column
        state.record_materialized(&main_record).expect("record");

        let mut asserted = phlo_transform_core::SemanticModel::in_memory(
            ModelId::parse("main.base").expect("model id"),
            "select 1 as id",
        );
        asserted.directives.keys.push("id".to_string());
        let compilation = compile(&phlo_transform_core::SemanticProject::in_memory(vec![
            asserted,
        ]));
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

        let breaks = contract_breaking_changes(Some(&state), &compilation, "main").expect("breaks");
        assert!(
            !breaks.iter().any(|change| change.contains("key")),
            "equivalent key declared differently must not report: {breaks:?}"
        );
    }

    #[test]
    fn contract_breaking_changes_fails_closed_on_unverifiable_key() {
        // A legacy record with neither a persisted effective key nor an
        // incremental key cannot prove the base had no key — an unknown
        // historical key must fail closed, not read as keyless.
        let state = SqliteStateStore::in_memory().expect("state");
        let mut main_record = record("main.base", Some("main"), "v1");
        main_record.effective_key = None; // predates the column
        state.record_materialized(&main_record).expect("record");

        // Desired has a key: the record might be hiding a change.
        let mut keyed = phlo_transform_core::SemanticModel::in_memory(
            ModelId::parse("main.base").expect("model id"),
            "select 1 as id",
        );
        keyed.directives.keys.push("id".to_string());
        let compilation = compile(&phlo_transform_core::SemanticProject::in_memory(vec![
            keyed,
        ]));
        let breaks = contract_breaking_changes(Some(&state), &compilation, "main").expect("breaks");
        assert!(
            breaks
                .iter()
                .any(|change| change.contains("key") && change.contains("cannot be verified")),
            "unverifiable record must block, got: {breaks:?}"
        );

        // Desired has no key either: the record might be hiding a removal.
        let keyless = phlo_transform_core::SemanticModel::in_memory(
            ModelId::parse("main.base").expect("model id"),
            "select 1 as id",
        );
        let compilation = compile(&phlo_transform_core::SemanticProject::in_memory(vec![
            keyless,
        ]));
        let breaks = contract_breaking_changes(Some(&state), &compilation, "main").expect("breaks");
        assert!(
            breaks
                .iter()
                .any(|change| change.contains("key") && change.contains("cannot be verified")),
            "unverifiable record must block even against a keyless model: {breaks:?}"
        );
    }

    fn lineage_artifact(
        base_kind: &str,
        base_ref: &str,
        base_commit: &str,
        candidate_head: Option<&str>,
        dirty: bool,
        environment: Option<LineageEnvironment>,
    ) -> LineageDiffArtifact {
        LineageDiffArtifact {
            schema_version: SCHEMA_VERSION,
            base_kind: base_kind.to_string(),
            base_ref: base_ref.to_string(),
            base_commit: base_commit.to_string(),
            candidate: CandidateProvenance {
                git_ref: Some("ci/x".to_string()),
                head: candidate_head.map(str::to_string),
                dirty,
                lineage_hash: Some("fingerprint".to_string()),
                model_versions: BTreeMap::new(),
            },
            environment,
            diff: phlo_transform_core::LineageDiff::default(),
        }
    }

    fn write_lineage_diff(cli: &Cli, artifact: &LineageDiffArtifact) {
        ArtifactWriter::for_workspace(&cli.root)
            .write_lineage_diff(artifact)
            .expect("lineage artifact writes");
    }

    #[test]
    fn audited_lineage_accepts_the_bound_pair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        write_lineage_diff(
            &cli,
            &lineage_artifact(
                "merge-base",
                "main",
                "abc",
                Some("def"),
                false,
                Some(LineageEnvironment {
                    candidate_ref: "ci/x".to_string(),
                    candidate_hash: "h1".to_string(),
                    target_ref: "main".to_string(),
                    target_hash: "h2".to_string(),
                }),
            ),
        );

        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "current");
    }

    /// The Nessie hashes matching is not enough: code edited after the
    /// diff — or a dirty worktree edited again — changes the candidate's
    /// lineage fingerprint, and the artifact stops being current.
    #[test]
    fn audited_lineage_rejects_a_changed_candidate_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        write_lineage_diff(
            &cli,
            &lineage_artifact(
                "merge-base",
                "main",
                "abc",
                Some("def"),
                false,
                Some(LineageEnvironment {
                    candidate_ref: "ci/x".to_string(),
                    candidate_hash: "h1".to_string(),
                    target_ref: "main".to_string(),
                    target_hash: "h2".to_string(),
                }),
            ),
        );

        let evidence =
            audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("edited")).expect("evidence");
        assert_eq!(evidence.status, "stale");
        assert!(
            evidence
                .reason
                .as_deref()
                .expect("reason")
                .contains("lineage changed"),
            "{:?}",
            evidence.reason
        );
    }

    /// An artifact written before candidate fingerprinting cannot prove it
    /// describes this candidate — even when every other identity matches.
    #[test]
    fn audited_lineage_rejects_an_artifact_without_a_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let mut artifact = lineage_artifact(
            "merge-base",
            "main",
            "abc",
            Some("def"),
            false,
            Some(LineageEnvironment {
                candidate_ref: "ci/x".to_string(),
                candidate_hash: "h1".to_string(),
                target_ref: "main".to_string(),
                target_hash: "h2".to_string(),
            }),
        );
        artifact.candidate.lineage_hash = None;
        write_lineage_diff(&cli, &artifact);

        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "stale");
        assert!(
            evidence
                .reason
                .as_deref()
                .expect("reason")
                .contains("fingerprinting"),
            "{:?}",
            evidence.reason
        );
    }

    #[test]
    fn audited_lineage_rejects_an_artifact_for_other_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        write_lineage_diff(
            &cli,
            &lineage_artifact(
                "merge-base",
                "main",
                "abc",
                Some("def"),
                false,
                Some(LineageEnvironment {
                    candidate_ref: "ci/other".to_string(),
                    candidate_hash: "h1".to_string(),
                    target_ref: "main".to_string(),
                    target_hash: "h2".to_string(),
                }),
            ),
        );

        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "stale");
        assert!(
            evidence
                .reason
                .as_deref()
                .expect("reason")
                .contains("ci/other"),
            "{:?}",
            evidence.reason
        );
    }

    #[test]
    fn audited_lineage_rejects_a_moved_candidate_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        write_lineage_diff(
            &cli,
            &lineage_artifact(
                "merge-base",
                "main",
                "abc",
                Some("def"),
                false,
                Some(LineageEnvironment {
                    candidate_ref: "ci/x".to_string(),
                    candidate_hash: "old".to_string(),
                    target_ref: "main".to_string(),
                    target_hash: "h2".to_string(),
                }),
            ),
        );

        let evidence = audited_lineage(&cli, "ci/x", "main", "new", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "stale");
        assert!(
            evidence
                .reason
                .as_deref()
                .expect("reason")
                .contains("moved"),
            "{:?}",
            evidence.reason
        );
    }

    #[test]
    fn audited_lineage_rejects_an_unreadable_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        let directory = dir.path().join(".phlo").join("transform");
        std::fs::create_dir_all(&directory).expect("artifact dir");
        std::fs::write(directory.join("lineage_diff.json"), "not json").expect("write");

        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "stale");
        assert!(
            evidence
                .reason
                .as_deref()
                .expect("reason")
                .contains("unreadable"),
            "{:?}",
            evidence.reason
        );
    }

    #[test]
    fn audited_lineage_absent_artifact_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(dir.path());
        assert!(audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint")).is_none());
    }

    /// A git-bound (no Nessie binding) merge-base artifact is advisory while
    /// HEAD and the merge-base still match, and stale once the worktree or
    /// base moved. A repo the artifact never knew cannot prove currentness.
    #[test]
    fn audited_lineage_git_bound_tracks_the_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("git runs");
            assert!(output.status.success(), "{args:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@phlo.dev"]);
        git(&["config", "user.name", "Phlo Test"]);
        std::fs::write(root.join("phlo.toml"), "[transform]\nroots = []\n").expect("toml");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let head = git(&["rev-parse", "HEAD"]);

        let cli = cli_at(root);
        // Current merge-base, current head, clean — advisory.
        write_lineage_diff(
            &cli,
            &lineage_artifact("merge-base", "main", &head, Some(&head), false, None),
        );
        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "advisory");

        // HEAD recorded differently — the worktree moved on.
        write_lineage_diff(
            &cli,
            &lineage_artifact("merge-base", "main", &head, Some("stale"), false, None),
        );
        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "stale");

        // The same artifact in a non-repository cannot resolve its base.
        let bare = tempfile::tempdir().expect("tempdir");
        let cli = cli_at(bare.path());
        write_lineage_diff(
            &cli,
            &lineage_artifact("merge-base", "main", &head, Some(&head), false, None),
        );
        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "stale");
    }

    /// An exact ref→ref artifact is only evidence while both refs still
    /// resolve to the commits it recorded — deleting either ref is
    /// unverifiable, not "unchanged".
    #[test]
    fn audited_lineage_rejects_deleted_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("git runs");
            assert!(output.status.success(), "{args:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@phlo.dev"]);
        git(&["config", "user.name", "Phlo Test"]);
        std::fs::write(root.join("phlo.toml"), "[transform]\nroots = []\n").expect("toml");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let head = git(&["rev-parse", "HEAD"]);
        git(&["branch", "base"]);
        git(&["branch", "feature"]);

        let cli = cli_at(root);
        let mut artifact = lineage_artifact("ref", "base", &head, Some(&head), false, None);
        artifact.candidate.git_ref = Some("feature".to_string());
        write_lineage_diff(&cli, &artifact);

        // Both refs resolve to the recorded commits — advisory.
        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "advisory");

        // The candidate ref is gone — the artifact can no longer prove it
        // describes `feature`.
        git(&["branch", "-D", "feature"]);
        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "stale");
        assert!(
            evidence
                .reason
                .as_deref()
                .expect("reason")
                .contains("feature"),
            "{:?}",
            evidence.reason
        );

        // Restore the candidate; deleting the base ref must go stale too.
        git(&["branch", "feature"]);
        git(&["branch", "-D", "base"]);
        let evidence = audited_lineage(&cli, "ci/x", "main", "h1", "h2", Some("fingerprint"))
            .expect("evidence");
        assert_eq!(evidence.status, "stale");
        assert!(
            evidence.reason.as_deref().expect("reason").contains("base"),
            "{:?}",
            evidence.reason
        );
    }
}
