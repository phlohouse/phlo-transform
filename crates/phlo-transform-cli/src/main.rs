//! `phlo-transform` command line interface.
//!
//! The CLI is a thin consumer of the compiler core. Every semantic command
//! supports `--json`; human and JSON output are both derived from the same
//! report structures.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use phlo_transform_core::{
    compile, load_project, CheckReport, Diagnostic, InspectReport, ListReport, ModelId,
};

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

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compile the workspace and report diagnostics.
    Check,
    /// List discovered models and external sources.
    List,
    /// Show details for a single model.
    Inspect {
        /// Model name (`assay.results`) or URI (`model://assay/results`).
        model: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
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

fn run(cli: &Cli) -> Result<ExitCode, String> {
    let project = match load_project(&cli.root) {
        Ok(project) => project,
        Err(diagnostics) => {
            if cli.json {
                let payload = serde_json::json!({
                    "ok": false,
                    "diagnostics": diagnostics,
                });
                print_json(&payload)?;
            } else {
                render_diagnostics(&diagnostics);
            }
            return Ok(ExitCode::FAILURE);
        }
    };
    let compilation = compile(&project);

    match &cli.command {
        Command::Check => {
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
        Command::List => {
            let report = compilation.list_report();
            if cli.json {
                print_json(&report)?;
            } else {
                print_list_human(&report);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Inspect { model } => {
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
    }
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
    println!();
    if report.ok {
        println!("check passed");
    } else {
        println!("check failed");
    }
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
        if dependencies.is_empty() {
            println!("  {}", model.name);
        } else {
            println!("  {:<28} <- {}", model.name, dependencies.join(", "));
        }
    }

    println!();
    println!("Sources ({})", report.sources.len());
    for source in &report.sources {
        println!("  {}", source.name);
    }
}

fn print_inspect_human(report: &InspectReport) {
    let model = &report.model;
    println!("Model:  {}", model.name);
    println!("ID:     {}", model.id);
    println!("Path:   {}", model.path.as_deref().unwrap_or("(in memory)"));
    println!("Pinned: {}", model.pinned_id.as_deref().unwrap_or("(none)"));
    println!();

    print_section("Depends on", &model.depends_on);
    print_section("Sources", &model.sources);
    print_section("Used by", &model.used_by);
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
