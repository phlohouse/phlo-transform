//! `phlo-transform` command line interface.
//!
//! The CLI is a thin consumer of the compiler and engine. Every semantic
//! command supports `--json`; human and JSON output are derived from the same
//! report structures.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use phlo_transform_core::{
    compile, load_project, select_models, CheckReport, Compilation, Diagnostic, InspectReport,
    ListReport, ModelId, SelectionOptions,
};
use phlo_transform_engine::{
    Adapter, ArtifactWriter, ExecutionStatus, Plan, PlanAction, Planner, RunOptions, RunResult,
    Runner, SqliteStateStore,
};
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
}

#[tokio::main]
async fn main() -> ExitCode {
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
    let project = match load_project(&cli.root) {
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
    let compilation = compile(&project);

    match &cli.command {
        Command::Check => run_check(cli, &compilation),
        Command::List => run_list(cli, &compilation),
        Command::Inspect { model } => run_inspect(cli, &compilation, model),
        Command::Plan => run_plan(cli, &compilation).await,
        Command::Apply => run_apply(cli, &compilation, false).await,
        Command::Run => run_apply(cli, &compilation, true).await,
        Command::Test => run_test(cli, &compilation).await,
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
            if cli.json {
                print_json(&report)?;
            } else {
                print_inspect_human(&report);
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
) -> Result<(Plan, ArtifactWriter), String> {
    let adapter = build_adapter(cli)?;
    let selected = select_models(compilation, &selection(cli));
    let planner = Planner::new(adapter);
    let plan = planner
        .plan(compilation, &selected, cli.environment.clone())
        .await
        .map_err(|error| error.to_string())?;
    Ok((plan, ArtifactWriter::for_workspace(&cli.root)))
}

async fn run_plan(cli: &Cli, compilation: &Compilation) -> Result<ExitCode, String> {
    let (plan, writer) = build_plan(cli, compilation).await?;
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
    let (plan, writer) = build_plan(cli, compilation).await?;
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
    let state = SqliteStateStore::open(&state_path(cli)).map_err(|error| error.to_string())?;
    let runner = Runner::new(adapter, Some(Arc::new(state)));
    let options = RunOptions {
        environment: cli.environment.clone(),
        concurrency: cli.concurrency,
        run_tests: true,
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
    let endpoint = cli
        .trino_endpoint
        .clone()
        .or_else(|| std::env::var("PHLO_TRINO_ENDPOINT").ok())
        .ok_or_else(|| {
            "no target configured: pass --trino-endpoint or set PHLO_TRINO_ENDPOINT".to_string()
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

fn state_path(cli: &Cli) -> PathBuf {
    cli.root.join(".phlo").join("transform").join("state.db")
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
            PlanAction::Create => "CREATE",
            PlanAction::Replace => "REPLACE",
            PlanAction::NoOp => "NOOP",
            PlanAction::Unknown => "UNKNOWN",
        };
        println!(
            "  {:<6} {:<28} [{}] {}",
            action, model.id, model.materialization, model.target
        );
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
