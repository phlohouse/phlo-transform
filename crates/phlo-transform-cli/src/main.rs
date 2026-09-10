//! `phlo-transform` command line interface.
//!
//! The CLI is a thin consumer of the compiler and engine. Every semantic
//! command supports `--json`; human and JSON output are derived from the same
//! report structures.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use phlo_transform_core::{
    compile, compile_with_options, load_project, select_models, Assertion, CheckReport,
    Compilation, DataType, Diagnostic, IncrementalStrategy, InspectReport, ListReport, ModelId,
    Nullability, Relation, RelationSchema, SchemaColumn, SelectionOptions, StaticSchemaProvider,
};
use phlo_transform_daemon::{serve, spawn_watcher, WorkspaceService};
use phlo_transform_duckdb::DuckDbAdapter;
use phlo_transform_engine::{
    collect_source_states, diff, ensure_environment, promote, relation_for_source, Adapter,
    ArtifactWriter, CancelHandle, DiffPolicy, DiffRequest, DiffStrategy, EnvironmentSetup,
    EnvironmentSpec, ExecutionStatus, Plan, PlanAction, Planner, PromotionRequest, RunOptions,
    RunResult, Runner, SqliteStateStore, StateStore,
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

    /// Select models by exact name or namespace glob (repeatable).
    #[arg(long, global = true)]
    select: Vec<String>,

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

#[derive(Debug, Subcommand)]
enum Command {
    /// Compile the workspace and report diagnostics.
    Check,
    /// List discovered models, sources and tests.
    List,
    /// Show details for a single model.
    Inspect {
        /// Model name (`assay.results`) or URI (`model://assay/results`).
        model: String,
    },
    /// Show the work that would be done, without mutating anything.
    Plan,
    /// Execute the plan.
    Apply,
    /// Convenience: plan + apply.
    Run,
    /// Run custom SQL tests against the current target.
    Test,
    /// Show upstream/downstream model lineage or a column's lineage.
    Lineage {
        /// Model (`assay.results`) or column (`assay.results.concentration`).
        target: String,
    },
    /// Show downstream impact of a column.
    Impact {
        /// Column reference (`assay.results.concentration`).
        column: String,
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

    let environment = match &cli.command {
        Command::Plan | Command::Apply | Command::Run => provision_environment(cli).await?,
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
        if should_enrich(cli) {
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

    match &cli.command {
        Command::Check => run_check(cli, &compilation),
        Command::List => run_list(cli, &compilation),
        Command::Inspect { model } => run_inspect(cli, &compilation, model),
        Command::Plan => run_plan(cli, &compilation).await,
        Command::Apply => run_apply(cli, &compilation, false).await,
        Command::Run => run_apply(cli, &compilation, true).await,
        Command::Test => run_test(cli, &compilation).await,
        Command::Lineage { target } => run_lineage(cli, &compilation, target),
        Command::Impact { column } => run_impact(cli, &compilation, column),
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

fn run_list(cli: &Cli, compilation: &Compilation) -> Result<ExitCode, String> {
    let report = compilation.list_report();
    if cli.json {
        print_json(&report)?;
    } else {
        print_list_human(&report);
    }
    Ok(ExitCode::SUCCESS)
}

fn run_inspect(cli: &Cli, compilation: &Compilation, model: &str) -> Result<ExitCode, String> {
    let id = ModelId::parse(model)
        .map_err(|error| format!("invalid model reference `{model}`: {error}"))?;
    match compilation.inspect_report(&id) {
        Some(report) => {
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

fn selection(cli: &Cli) -> SelectionOptions {
    SelectionOptions {
        select: cli.select.clone(),
        upstream: cli.upstream,
        downstream: cli.downstream,
        tag: cli.tag.clone(),
        workflow: cli.workflow.clone(),
    }
}

async fn build_plan(
    cli: &Cli,
    compilation: &Compilation,
    state: Option<Arc<dyn StateStore>>,
) -> Result<(Plan, ArtifactWriter), String> {
    let adapter = build_adapter(cli)?;
    let selected = select_models(compilation, &selection(cli));
    let planner = Planner::new(adapter, state);
    let plan = planner
        .plan(compilation, &selected, environment(cli))
        .await
        .map_err(|error| error.to_string())?;
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

async fn run_plan(cli: &Cli, compilation: &Compilation) -> Result<ExitCode, String> {
    let (plan, writer) = build_plan(cli, compilation, open_state(cli)).await?;
    writer
        .write_project(compilation)
        .and_then(|_| writer.write_plan(&plan))
        .map_err(|error| error.to_string())?;

    if cli.json {
        print_json(&plan)?;
    } else {
        print_plan_human(&plan);
        render_diagnostics(&plan.diagnostics);
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
    convenience_run: bool,
) -> Result<ExitCode, String> {
    let state = open_state(cli);
    let (plan, writer) = build_plan(cli, compilation, state.clone()).await?;
    writer
        .write_project(compilation)
        .and_then(|_| writer.write_plan(&plan))
        .map_err(|error| error.to_string())?;

    if plan.blocked {
        if cli.json {
            print_json(&plan)?;
        } else {
            print_plan_human(&plan);
            render_diagnostics(&plan.diagnostics);
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

async fn run_test(cli: &Cli, compilation: &Compilation) -> Result<ExitCode, String> {
    let adapter = build_adapter(cli)?;
    let mut results = Vec::new();
    let mut failed = false;
    for test in &compilation.tests {
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

fn run_lineage(cli: &Cli, compilation: &Compilation, target: &str) -> Result<ExitCode, String> {
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
                println!("Direct:     {}", join_or_none(&report.direct));
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

fn run_impact(cli: &Cli, compilation: &Compilation, column: &str) -> Result<ExitCode, String> {
    // A model-only argument reports downstream models/tests (works offline).
    if let Some(model) = compilation.model_by_name(column) {
        let lineage = compilation
            .model_lineage_report(&model.id)
            .expect("model exists");
        let mut tests: Vec<String> = Vec::new();
        for dependent in &lineage.downstream {
            if let Ok(id) = ModelId::parse(dependent) {
                for test in compilation.tests_for(&id) {
                    tests.push(test.id.to_string());
                }
            }
        }
        if cli.json {
            print_json(&serde_json::json!({
                "model": model.id.logical_name(),
                "downstream_models": lineage.downstream,
                "tests": tests,
            }))?;
        } else {
            println!("Model:             {}", model.id.logical_name());
            println!("Downstream models: {}", join_or_none(&lineage.downstream));
            println!("Tests:             {}", join_or_none(&tests));
        }
        return Ok(ExitCode::SUCCESS);
    }

    let Some((model, name)) = column.rsplit_once('.') else {
        return Err(format!("invalid column `{column}`; expected model.column"));
    };
    let id = ModelId::parse(model).map_err(|error| format!("invalid model `{model}`: {error}"))?;
    if compilation.model(&id).is_none() {
        return Err(format!("no such model: {}", id.logical_name()));
    }
    let target = phlo_transform_core::ColumnRef::model(id, name);
    let report = compilation.impact_report(&target);
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

/// Whether a command benefits from catalogue-enriched schemas.
fn should_enrich(cli: &Cli) -> bool {
    cli.catalogue
        || matches!(
            cli.command,
            Command::Inspect { .. }
                | Command::Lineage { .. }
                | Command::Impact { .. }
                | Command::Explain { .. }
                | Command::Plan
                | Command::Apply
                | Command::Run
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
    let default_schema = default_schema.or(match adapter.name() {
        "duckdb" => Some("main"),
        _ => None,
    });
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
    let source_states =
        collect_source_states(adapter.as_ref(), &sources, default_catalog, default_schema)
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
            println!("  --> {path}");
        }
    }
}

fn print_check_human(report: &CheckReport) {
    println!("Workspace: {}", report.workspace_root);
    println!("Roots:     {}", report.roots.len());
    println!("Models:    {}", report.model_count);
    println!("Sources:   {}", report.source_count);
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
    println!();

    if plan.blocked {
        println!("plan blocked by compilation errors");
    }

    println!("Models ({})", plan.models.len());
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
            println!("           reason: {}", reason.label());
        }
        if let Some(incremental) = &model.incremental {
            println!("           strategy: {incremental}");
        }
        if model.full_rebuild {
            println!("           full rebuild required");
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
    for model in &result.models {
        println!(
            "  {:<8} {:<28} {}ms",
            model.status.label(),
            model.model,
            model.duration_ms
        );
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
                "no phlo.toml, transforms/ or workflows/ under {}; run `phlo-transform init`",
                cli.root.display()
            )
        },
    });

    // 2. Project discovery + compilation.
    match load_project(&cli.root) {
        Ok(project) => {
            let compilation = compile(&project);
            let check = compilation.check_report();
            let errors = compilation
                .diagnostics
                .iter()
                .filter(|d| matches!(d.severity, phlo_transform_core::Severity::Error))
                .count();
            record(DoctorCheck {
                name: "compile",
                status: if compilation.is_ok() { "ok" } else { "fail" },
                detail: format!(
                    "{} models, {} sources, {} tests; {} error(s)",
                    check.model_count, check.source_count, check.test_count, errors
                ),
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

/// `explain <model>`: identity, dependencies, state and planned action.
async fn run_explain(
    cli: &Cli,
    compilation: &Compilation,
    model: &str,
) -> Result<ExitCode, String> {
    let id = ModelId::parse(model)
        .map_err(|error| format!("invalid model reference `{model}`: {error}"))?;
    let Some(report) = compilation.inspect_report(&id) else {
        return Err(format!("no such model: {}", id.logical_name()));
    };
    let compiled = compilation.model(&id).expect("inspect report exists");

    // Scope a plan to this model when an adapter is available.
    let mut plan_model: Option<serde_json::Value> = None;
    if build_adapter(cli).is_ok() {
        let mut sel = selection(cli);
        sel.select = vec![id.logical_name()];
        let selected = select_models(compilation, &sel);
        if let Ok(adapter) = build_adapter(cli) {
            let planner = Planner::new(adapter, open_state(cli));
            if let Ok(plan) = planner.plan(compilation, &selected, environment(cli)).await {
                if let Some(entry) = plan
                    .models
                    .iter()
                    .find(|entry| entry.id == id.to_string() || entry.id == id.logical_name())
                {
                    plan_model = Some(
                        serde_json::to_value(entry)
                            .map_err(|error| format!("could not serialise JSON: {error}"))?,
                    );
                }
            }
        }
    }

    if cli.json {
        print_json(&serde_json::json!({
            "model": report,
            "version": compiled.version.hash,
            "plan": plan_model,
        }))?;
    } else {
        print_inspect_human(&report);
        println!("Version:       {}", compiled.version.short());
        if let Some(plan) = &plan_model {
            println!(
                "Action:        {}",
                plan.get("action").and_then(|v| v.as_str()).unwrap_or("?")
            );
            if let Some(reasons) = plan.get("reasons").and_then(|v| v.as_array()) {
                for reason in reasons {
                    if let Some(label) = reason.as_str() {
                        println!("  reason: {label}");
                    }
                }
            }
        } else {
            println!("Action:        (unknown — no adapter configured)");
        }
        println!();
    }
    Ok(ExitCode::SUCCESS)
}
