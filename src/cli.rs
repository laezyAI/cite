//! Standalone CLI surface: argument shapes and per-command dispatch over the core library.
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use tracing::{error, info, instrument, warn};

use crate::core::CiteError;
use crate::core::db::DbManager;
use crate::core::{auth, compiler, deploy, doctor, install, project, scaffold};
use colored::Colorize;

fn print_json<T: serde::Serialize>(value: &T) {
    if let Ok(json) = serde_json::to_string_pretty(value) {
        println!("{json}");
    }
}

fn print_group_header(multi: bool, name: &str) {
    if multi {
        println!("{}", format!("── {name} ──").green());
    }
}

fn report_result(cli: &Cli, result: Result<String, CiteError>, err_prefix: &str) -> bool {
    match result {
        Ok(msg) => {
            if cli.json {
                print_json(&serde_json::json!({"status": "ok", "message": msg}));
            } else {
                eprintln!("{msg}");
            }
            false
        }
        Err(e) => {
            if cli.json {
                print_json(&serde_json::json!({"status": "error", "message": e.to_string()}));
            } else {
                warn!("{err_prefix}: {e}");
            }
            true
        }
    }
}

#[derive(Parser)]
#[command(
    name = "cite",
    version,
    about = "Create, validate, build, and deploy podcast content to Supabase",
    after_help = "Run without a command to open the interactive dashboard."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<CliCommand>,

    #[arg(
        global = true,
        long,
        default_value = ".",
        help = "Project directory, or a folder containing several projects"
    )]
    pub path: PathBuf,

    #[arg(global = true, short, long, help = "Show detailed logs")]
    pub verbose: bool,

    #[arg(
        global = true,
        short,
        long,
        conflicts_with = "verbose",
        help = "Show errors only"
    )]
    pub quiet: bool,

    #[arg(global = true, long, help = "Print machine-readable JSON on stdout")]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum CliCommand {
    #[command(about = "Create a new project with cite.toml, metadata.yml and folders")]
    Init {
        #[arg(help = "Name of the project folder to create")]
        name: String,
    },
    #[command(about = "Compile the project into build/content.json")]
    Build {
        #[arg(long, help = "Rebuild even if nothing changed")]
        force: bool,
    },
    #[command(about = "Validate, build if needed, and publish episodes to Supabase")]
    Deploy {
        #[arg(
            long,
            help = "Show what would be created or updated without contacting Supabase"
        )]
        dry_run: bool,
    },
    #[command(about = "Sign in to Supabase and list the artists you can deploy as")]
    Login {
        #[arg(long)]
        email: Option<String>,
        #[arg(
            long,
            env = "CITE_PASSWORD",
            hide_env_values = true,
            help = "Prefer CITE_PASSWORD or the hidden prompt; flags end up in shell history"
        )]
        password: Option<String>,
    },
    #[command(about = "Check the project for problems before deploying")]
    Doctor,
    #[command(about = "Remove build output and the incremental build cache")]
    Clean,
    #[command(about = "Remove the news items and uploads a deployment created")]
    Rollback {
        #[arg(help = "Deployment id printed by deploy (also shown by doctor)")]
        id: String,
    },
    #[command(about = "Update cite to the latest release")]
    Upgrade,
    #[command(about = "Remove cite and its local data from this machine")]
    Uninstall,
}

#[instrument]
fn load_projects(
    root: &Path,
    empty_msg: &str,
) -> Result<Option<Vec<project::ProjectContext>>, CiteError> {
    let roots = project::discover_projects(root);
    if roots.is_empty() {
        warn!("{empty_msg}");
        return Ok(None);
    }
    let mut projects = Vec::with_capacity(roots.len());
    for root in &roots {
        projects.push(project::ProjectContext::load(root)?);
    }
    Ok(Some(projects))
}

impl CliCommand {
    pub async fn execute(self, cli: &Cli) -> Result<(), CiteError> {
        match self {
            CliCommand::Init { name } => run_init(cli, &name),
            CliCommand::Build { force } => run_build(cli, force).await,
            CliCommand::Deploy { dry_run } => run_deploy(cli, dry_run).await,
            CliCommand::Doctor => run_doctor(cli).await,
            CliCommand::Clean => run_clean(cli).await,
            CliCommand::Rollback { id } => run_rollback(cli, &id).await,
            CliCommand::Login { email, password } => run_login(cli, email, password).await,
            CliCommand::Upgrade => run_upgrade().await,
            CliCommand::Uninstall => install::uninstall(),
        }
    }
}

fn run_init(cli: &Cli, name: &str) -> Result<(), CiteError> {
    let root = std::path::absolute(cli.path.join(name))?;
    scaffold::init_project(name, &root)?;
    if cli.json {
        print_json(
            &serde_json::json!({"status": "ok", "project": name, "root": root.to_string_lossy()}),
        );
    } else {
        println!(
            "{}",
            format!("Project '{name}' ready at {}", root.display()).green()
        );
    }
    Ok(())
}

async fn run_build(cli: &Cli, force: bool) -> Result<(), CiteError> {
    let db = DbManager::open().await?;
    let Some(projects) = load_projects(&cli.path, "No projects found (no cite.toml found)")? else {
        return Ok(());
    };
    let multi = projects.len() > 1;
    let mut has_errors = false;
    for ctx in &projects {
        print_group_header(multi, &ctx.manifest.project.name);
        match compiler::compile(&db, ctx, force).await {
            Ok(outcome) => emit_build(cli, &outcome),
            Err(e) => {
                error!("Build failed: {e}");
                has_errors = true;
            }
        }
    }
    if has_errors {
        return Err(CiteError::Config(
            "Build failed in one or more projects".to_string(),
        ));
    } else if !cli.json {
        println!("{}", "Build complete".green());
    }
    Ok(())
}

fn emit_build(cli: &Cli, outcome: &compiler::CompileOutcome) {
    if cli.json {
        match outcome {
            compiler::CompileOutcome::UpToDate => {
                print_json(&serde_json::json!({"status": "uptodate"}));
            }
            compiler::CompileOutcome::Complete { stats, artifact } => {
                let mut v = serde_json::to_value(stats).unwrap_or_default();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("status".into(), "complete".into());
                    obj.insert("artifact".into(), artifact.to_string_lossy().into());
                }
                print_json(&v);
            }
        }
    } else {
        outcome.emit();
    }
}

async fn run_deploy(cli: &Cli, dry_run: bool) -> Result<(), CiteError> {
    let db = DbManager::open().await?;
    let Some(projects) = load_projects(&cli.path, "No projects found (no cite.toml found)")? else {
        return Ok(());
    };
    let multi = projects.len() > 1;
    let mut has_errors = false;
    for ctx in &projects {
        print_group_header(multi, &ctx.manifest.project.name);
        let result = deploy::deploy(&db, ctx, dry_run).await;
        if report_result(cli, result, "Deploy failed") {
            has_errors = true;
        }
    }
    if has_errors {
        return Err(CiteError::Deploy(
            "Deploy failed in one or more projects".to_string(),
        ));
    } else if !cli.json {
        let done = if dry_run {
            "Dry run complete"
        } else {
            "Deploy complete"
        };
        println!("{}", done.green());
    }
    Ok(())
}

async fn run_doctor(cli: &Cli) -> Result<(), CiteError> {
    let db = DbManager::open().await?;
    let Some(projects) = load_projects(&cli.path, "")? else {
        if cli.json {
            print_json(
                &serde_json::json!({"status": "noproject", "errors": ["No cite.toml found"]}),
            );
        } else {
            info!("Running diagnostics");
            info!("cite.toml: missing (run 'cite init')");
            info!("metadata.yml: missing");
        }
        return Ok(());
    };
    let multi = projects.len() > 1;
    let mut all_outcomes: Vec<serde_json::Value> = Vec::new();
    let mut has_errors = false;
    let mut has_warnings = false;
    for ctx in &projects {
        print_group_header(multi, &ctx.manifest.project.name);
        let outcome = doctor::run(&db, ctx).await;
        if cli.json {
            let mut value = serde_json::to_value(&outcome)?;
            value["project"] = ctx.manifest.project.name.clone().into();
            all_outcomes.push(value);
        } else {
            outcome.emit();
        }
        has_errors |= outcome.has_errors();
        has_warnings |= outcome.has_warnings();
    }
    if cli.json {
        print_json(&all_outcomes);
    }
    if has_errors {
        return Err(CiteError::Config(
            "Doctor found validation errors".to_string(),
        ));
    }
    if !cli.json && !has_warnings {
        println!("{}", "Doctor check complete — no issues found".green());
    }
    Ok(())
}

async fn run_clean(cli: &Cli) -> Result<(), CiteError> {
    let db = DbManager::open().await?;
    let Some(projects) = load_projects(&cli.path, "No projects found")? else {
        return Ok(());
    };
    let multi = projects.len() > 1;
    for ctx in &projects {
        print_group_header(multi, &ctx.manifest.project.name);
        ctx.clean(&db).await?;
        if cli.json {
            print_json(&serde_json::json!({"status": "ok", "project": ctx.manifest.project.name}));
        } else {
            println!("{}", "Cleaned build artifacts".green());
        }
    }
    Ok(())
}

async fn run_rollback(cli: &Cli, id: &str) -> Result<(), CiteError> {
    let ctx = project::ProjectContext::load(&cli.path)?;
    let msg = deploy::rollback(&ctx, id).await?;
    if cli.json {
        print_json(&serde_json::json!({"status": "ok", "message": msg}));
    } else {
        info!("{msg}");
    }
    Ok(())
}

async fn run_login(
    cli: &Cli,
    email: Option<String>,
    password: Option<String>,
) -> Result<(), CiteError> {
    let backend = project::ProjectContext::load(&cli.path)
        .ok()
        .and_then(|ctx| ctx.manifest.backend);
    auth::login(backend, email, password).await?;
    println!("{}", "Login complete".green());
    Ok(())
}

async fn run_upgrade() -> Result<(), CiteError> {
    let msg = install::upgrade().await?;
    info!("{msg}");
    println!("{}", "Upgrade complete".green());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn test_dry_run_belongs_to_deploy() {
        let cli = Cli::try_parse_from(["cite", "deploy", "--dry-run", "--path", "p"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(CliCommand::Deploy { dry_run: true })
        ));
        assert_eq!(cli.path, PathBuf::from("p"));
        assert!(Cli::try_parse_from(["cite", "build", "--dry-run"]).is_err());
    }
}
