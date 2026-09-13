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
    Assertion, CheckReport, Compilation, DataType, Diagnostic, GitChanges, IncrementalStrategy,
    InspectReport, ListReport, ModelId, Nullability, Relation, RelationSchema, SchemaColumn,
    Selection, SelectorKind, SelectorSet, StaticSchemaProvider,
};
use phlo_transform_daemon::{serve, spawn_watcher, WorkspaceService};
use phlo_transform_duckdb::DuckDbAdapter;
use phlo_transform_engine::{
    adapter_default_schema, changed_models, collect_source_states, diff, diff_reasons,
    ensure_environment, promote, relation_for_source, Adapter, ArtifactWriter, CancelHandle,
    DiffPolicy, DiffRequest, DiffStrategy, EnvironmentSetup, EnvironmentSpec, ExecutionStatus,
    Membership, Plan, PlanAction, PlanOptions, PlanReason, Planner, PromotionRequest, ReasonKind,
    RunOptions, RunResult, Runner, SqliteStateStore, StateStore,
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

    /// Maximum concurrent model builds.
    #[arg(long, global = true, default_value_t = 4)]
    concurrency: usize,

    /// Environment label recorded in plans and run history.
    #[arg(long, global = true)]
    environment: Option<String>,

    /// Nessie reference (environment) for plan/apply.
    #[arg(long = "ref", global = true)]
    reference: Option<String>,

    /// Base Nessie reference to create a candidate from (default `main`);
    /// for `translate`, the source format (e.g. `--from dbt`).
    #[arg(long, global = true)]
    from: Option<String>,

    /// Iceberg warehouse for provisioned catalogs, e.g. `s3://bucket/wh`.
    #[arg(long, global = true)]
    warehouse: Option<String>,

    /// Physical catalog for model targets (overrides workspace config).
    #[arg(long, global = true)]
    catalog: Option<String>,

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
    },
    /// Show downstream impact of a column or a selection.
    Impact {
        /// Column reference (`assay.results.concentration`) or model
        /// (`assay.results`). Omit and pass `--select` for the impact of a
        /// selection.
        column: Option<String>,
    },
    /// Promote an audited candidate Nessie reference to a target.
    Promote {
        /// Candidate reference, e.g. `ci/pr-1`.
        candidate: String,
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
    /// Compare a model's candidate and base data.
    Diff {
        /// Model name (`assay.results`).
        model: String,
        /// Base reference label (defaults to the candidate reference).
        #[arg(long)]
        base: Option<String>,
        /// Base physical relation (`catalog.schema.table`).
        #[arg(long)]
        base_relation: Option<String>,
        /// Force a full keyed comparison.
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
        ArtifactWriter::for_workspace(&cli.root)
            .write_environment(setup)
            .map_err(|error| error.to_string())?;
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
        Command::Lineage { target, format } => run_lineage(
            cli,
            &compilation,
            target.as_deref(),
            &set,
            git.as_ref(),
            *format,
        ),
        Command::Impact { column } => {
            run_impact(cli, &compilation, column.as_deref(), &set, git.as_ref())
        }
        Command::Explain { model } => run_explain(cli, &compilation, model).await,
        Command::Translate { .. } | Command::Doctor | Command::Init | Command::Manifest => {
            unreachable!("handled before workspace load")
        }
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
                candidate,
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
            base_relation,
            full,
            partition,
            sample,
        } => {
            run_diff(
                cli,
                &compilation,
                model,
                base,
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
            let current = open_state(cli).and_then(|state| {
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
                let state = open_state(cli);
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

fn open_state(cli: &Cli) -> Option<Arc<dyn StateStore>> {
    SqliteStateStore::open(&state_path(cli))
        .ok()
        .map(|store| Arc::new(store) as Arc<dyn StateStore>)
}

/// The effective environment: `--environment`, else `--ref`.
fn environment(cli: &Cli) -> Option<String> {
    cli.environment.clone().or_else(|| cli.reference.clone())
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
    ensure_environment(nessie.as_ref(), adapter.as_ref(), &spec)
        .await
        .map(Some)
        .map_err(|error| error.to_string())
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

fn read_diff(cli: &Cli) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(artifact_path(cli, "diff.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// A diff is stale once the candidate model version it recorded is no longer
/// the candidate's materialised version.
fn diff_is_stale(cli: &Cli, candidate: &str, diff: &serde_json::Value) -> Option<String> {
    let state = open_state(cli)?;
    let diff = diff.get("diff")?;
    let model = diff.get("model")?.as_str()?;
    let version = diff.get("candidate_version")?.as_str()?;
    match state
        .materialized_version(model, Some(candidate))
        .ok()
        .flatten()
    {
        Some(record) if record.version.hash == version => None,
        Some(_) => Some(format!(
            "diff artifact for `{model}` is stale: candidate data changed since the diff"
        )),
        None => Some(format!(
            "diff artifact for `{model}` is stale: candidate is not materialised"
        )),
    }
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

async fn run_promote(
    cli: &Cli,
    candidate: &str,
    to: &str,
    check: bool,
    require_diff: bool,
    allow_breaking_schema: bool,
    cleanup: bool,
) -> Result<ExitCode, String> {
    let nessie = build_nessie(cli)?;
    let state = open_state(cli);
    let gates_passed = match &state {
        Some(state) => state
            .latest_run(Some(candidate))
            .map_err(|error| error.to_string())?
            .map(|run| run.status == ExecutionStatus::Passed && run.failed_count == 0)
            .unwrap_or(false),
        None => false,
    };
    if !gates_passed {
        let message = format!("candidate `{candidate}` has no successful run; apply it first");
        if cli.json {
            print_json(&serde_json::json!({ "ok": false, "error": message }))?;
        } else {
            eprintln!("error: {message}");
        }
        return Ok(ExitCode::FAILURE);
    }

    let environment = read_environment(cli);
    let diff = read_diff(cli);
    let diff_passed = diff
        .as_ref()
        .and_then(|value| value.get("diff")?.get("passed")?.as_bool());

    if require_diff {
        if diff_passed != Some(true) {
            let message = "a passing data diff is required before promotion".to_string();
            if cli.json {
                print_json(&serde_json::json!({ "ok": false, "error": message }))?;
            } else {
                eprintln!("error: {message}");
            }
            return Ok(ExitCode::FAILURE);
        }
        if let Some(value) = &diff {
            if let Some(reason) = diff_is_stale(cli, candidate, value) {
                if cli.json {
                    print_json(&serde_json::json!({ "ok": false, "error": reason }))?;
                } else {
                    eprintln!("error: {reason}");
                }
                return Ok(ExitCode::FAILURE);
            }
        }
    }

    // Breaking schema changes from the audited diff block promotion unless
    // explicitly allowed (enforced by the engine).
    let breaking_schema_changes: Vec<String> = diff
        .as_ref()
        .and_then(|value| value.get("diff"))
        .and_then(|diff| diff.get("schema_changes"))
        .and_then(|changes| changes.as_array())
        .map(|changes| {
            changes
                .iter()
                .filter(|change| {
                    matches!(
                        change.get("safety").and_then(|safety| safety.as_str()),
                        Some("error" | "full_rebuild_required")
                    )
                })
                .map(|change| {
                    format!(
                        "{} ({})",
                        change
                            .get("column")
                            .and_then(|column| column.as_str())
                            .unwrap_or("*"),
                        change
                            .get("detail")
                            .and_then(|detail| detail.as_str())
                            .unwrap_or("schema change")
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let request = PromotionRequest {
        candidate_ref: candidate.to_string(),
        target_ref: to.to_string(),
        candidate_hash: environment
            .as_ref()
            .map(|setup| setup.candidate.hash.clone()),
        // The base hash recorded when the candidate was provisioned pins the
        // target state the audit was performed against.
        expected_target_hash: environment.as_ref().map(|setup| setup.base.hash.clone()),
        plan_id: None,
        run_id: None,
        quality_gates_passed: true,
        diff_passed,
        require_diff,
        breaking_schema_changes,
        allow_breaking_schema,
        dry_run: check,
        actor: None,
    };
    match promote(nessie.as_ref(), &request).await {
        Ok(record) => {
            ArtifactWriter::for_workspace(&cli.root)
                .write_promotion(&record)
                .map_err(|error| error.to_string())?;
            if cli.json {
                print_json(&record)?;
            } else {
                print_promotion_human(&record);
            }
            if cleanup && record.merged {
                cleanup_candidate(cli, &nessie, candidate, environment.as_ref()).await;
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

/// Best-effort removal of a promoted candidate's branch and catalog.
async fn cleanup_candidate(
    cli: &Cli,
    nessie: &Arc<dyn NessieClient>,
    candidate: &str,
    environment: Option<&EnvironmentSetup>,
) {
    let catalog = environment
        .map(|setup| setup.catalog.clone())
        .unwrap_or_else(|| catalog_name(candidate));
    if let Ok(adapter) = build_adapter(cli) {
        let _ = adapter
            .execute(&format!("DROP CATALOG IF EXISTS {}", catalog))
            .await;
    }
    let _ = nessie.delete_branch(candidate).await;
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
    let (plan, writer) = build_plan(cli, compilation, set, open_state(cli), git).await?;
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

async fn run_apply(
    cli: &Cli,
    compilation: &Compilation,
    set: &SelectorSet,
    git: Option<&GitChanges>,
    convenience_run: bool,
) -> Result<ExitCode, String> {
    let state = open_state(cli);
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
    let runner = Runner::new(adapter, state);
    let options = RunOptions {
        environment: environment(cli),
        concurrency: cli.concurrency,
        run_tests: true,
        cancel,
    };
    let result = runner
        .apply(compilation, &plan, &options)
        .await
        .map_err(|error| error.to_string())?;
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

fn run_lineage(
    cli: &Cli,
    compilation: &Compilation,
    target: Option<&str>,
    set: &SelectorSet,
    git: Option<&GitChanges>,
    format: Option<LineageFormat>,
) -> Result<ExitCode, String> {
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
    println!("Run:    {}", result.run_id);
    println!("Status: {}", result.status.label());
    println!();
    for seed in &result.seeds {
        println!(
            "  {:<8} {:<28} {}",
            seed.status.label(),
            seed.seed,
            seed.target
        );
        if let Some(error) = &seed.error {
            println!("           {error}");
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
        if let Some(error) = &model.error {
            println!("           {error}");
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
            if let Some(error) = &test.error {
                println!("           {error}");
            }
        }
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
    model: &str,
    base: &Option<String>,
    base_relation: &Option<String>,
    full: bool,
    partition: Option<&str>,
    sample: Option<f64>,
) -> Result<ExitCode, String> {
    let id = ModelId::parse(model).map_err(|error| format!("invalid model `{model}`: {error}"))?;
    let compiled = compilation
        .model(&id)
        .ok_or_else(|| format!("no such model: {}", id.logical_name()))?;

    let adapter = build_adapter(cli)?;
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

    let candidate_relation = compiled.target.clone();
    let base_relation = match base_relation {
        Some(spec) => parse_relation(spec),
        None => candidate_relation.clone(),
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
        candidate_ref: environment(cli),
        base_ref: base.clone(),
        candidate_version: Some(compiled.version.hash.clone()),
        base_version: None,
        key_columns,
        columns,
        strategy,
        policy: diff_policy(compiled.config.diff.as_ref()),
        sample_fraction: sample,
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

fn model_keys(model: &phlo_transform_core::CompiledModel) -> Vec<String> {
    if let Some(IncrementalStrategy::Key { columns }) = &model.config.incremental {
        return columns.clone();
    }
    for assertion in &model.assertions {
        if let Assertion::Unique { columns } = assertion {
            return columns.clone();
        }
    }
    Vec::new()
}

fn parse_relation(spec: &str) -> Relation {
    let parts: Vec<&str> = spec.split('.').collect();
    match parts.as_slice() {
        [schema, table] => Relation {
            catalog: None,
            schema: (*schema).to_string(),
            table: (*table).to_string(),
        },
        [catalog, schema, table] => Relation {
            catalog: Some((*catalog).to_string()),
            schema: (*schema).to_string(),
            table: (*table).to_string(),
        },
        _ => Relation {
            catalog: None,
            schema: "default".to_string(),
            table: spec.to_string(),
        },
    }
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
    match SqliteStateStore::open(&state_path(cli)) {
        Ok(_) => record(DoctorCheck {
            name: "state",
            status: "ok",
            detail: state_path(cli).display().to_string(),
        }),
        Err(error) => record(DoctorCheck {
            name: "state",
            status: "fail",
            detail: format!("could not open state store: {error}"),
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

    let state = open_state(cli);
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
