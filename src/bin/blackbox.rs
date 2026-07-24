use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use bbox_config::config::{self, LoadOptions};
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationApplyRequestV1, ProjectCatalogMigrationError,
    ProjectCatalogMigrationFacadeV1, ProjectCatalogMigrationLayoutOverridesV1,
    ProjectCatalogMigrationPreflightRequestV1, ProjectCatalogMigrationResolvedLayoutV1,
    ProjectCatalogMigrationVerifyRequestV1,
};
use clap::{ArgGroup, Args, Parser, Subcommand};
use serde::Serialize;

const ENVELOPE_VERSION: u32 = 1;

#[derive(Debug, Parser)]
#[command(name = "blackbox", version, about = "Offline Blackbox administration")]
struct Cli {
    #[command(subcommand)]
    command: TopLevelCommand,
}

#[derive(Debug, Subcommand)]
enum TopLevelCommand {
    /// Inspect or rehearse the durable project-catalog migration.
    ProjectCatalog(ProjectCatalogArgs),
}

#[derive(Debug, Args)]
struct ProjectCatalogArgs {
    #[command(subcommand)]
    command: ProjectCatalogCommand,
}

#[derive(Debug, Subcommand)]
enum ProjectCatalogCommand {
    /// Produce reviewed migration artifacts or apply them to an isolated root.
    Migrate(MigrateArgs),
    /// Verify exact installed migration state in an isolated root.
    Verify(VerifyArgs),
}

#[derive(Debug, Clone, Args)]
struct ConfigArgs {
    /// Load the same configuration file used by blackboxd.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override the complete conventional source-state bundle.
    #[arg(long, value_name = "PATH")]
    state_dir: Option<PathBuf>,

    /// Override only the source projects.json path. This wins over --state-dir.
    #[arg(long, value_name = "PATH")]
    projects_path: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("mode")
        .required(true)
        .multiple(false)
        .args(["preflight", "apply"])
))]
struct MigrateArgs {
    /// Capture the source inventory and write reviewed artifacts.
    #[arg(long)]
    preflight: bool,

    /// Apply an exact clean artifact pair into an isolated rehearsal root.
    #[arg(long)]
    apply: bool,

    /// Path to the path-redacted migration report.
    #[arg(long, value_name = "PATH")]
    report: PathBuf,

    /// Path to the exact operator resolution artifact.
    #[arg(long, value_name = "PATH")]
    resolution: PathBuf,

    /// Isolated rehearsal root. Required with --apply.
    #[arg(long, value_name = "PATH", required_if_eq("apply", "true"))]
    rehearsal_root: Option<PathBuf>,

    /// Explicit owner-only local-path review artifact. Preflight only.
    #[arg(
        long,
        value_name = "PATH",
        requires = "preflight",
        conflicts_with = "apply"
    )]
    include_local_paths: Option<PathBuf>,

    #[command(flatten)]
    config: ConfigArgs,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Isolated rehearsal state root, not a projects.json path.
    #[arg(long, value_name = "PATH")]
    root: PathBuf,

    /// Load the same configuration file used by blackboxd.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Debug)]
struct CommandFailure {
    code: String,
    message: String,
}

impl CommandFailure {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl From<ProjectCatalogMigrationError> for CommandFailure {
    fn from(error: ProjectCatalogMigrationError) -> Self {
        Self::new(error.code, error.message)
    }
}

#[derive(Serialize)]
struct SuccessEnvelope<T> {
    version: u32,
    command: &'static str,
    result: T,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    version: u32,
    command: &'static str,
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: String,
    message: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let command = command_name(&cli);
    match execute(cli) {
        Ok(result) => {
            if write_json(&SuccessEnvelope {
                version: ENVELOPE_VERSION,
                command,
                result,
            })
            .is_err()
            {
                eprintln!("blackbox: failed to write the result envelope");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("blackbox: {}: {}", error.code, error.message);
            let envelope = ErrorEnvelope {
                version: ENVELOPE_VERSION,
                command,
                error: ErrorBody {
                    code: error.code,
                    message: error.message,
                },
            };
            let _ = write_json(&envelope);
            ExitCode::FAILURE
        }
    }
}

fn command_name(cli: &Cli) -> &'static str {
    match &cli.command {
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Migrate(args),
        }) if args.preflight => "project_catalog_migrate_preflight",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Migrate(_),
        }) => "project_catalog_migrate_apply",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Verify(_),
        }) => "project_catalog_verify",
    }
}

fn execute(cli: Cli) -> Result<serde_json::Value, CommandFailure> {
    match cli.command {
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Migrate(args),
        }) => execute_migrate(args),
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Verify(args),
        }) => execute_verify(args),
    }
}

fn execute_migrate(args: MigrateArgs) -> Result<serde_json::Value, CommandFailure> {
    let config = load_config(args.config.config)?;
    let source_layout = ProjectCatalogMigrationResolvedLayoutV1::from_config(
        &config,
        ProjectCatalogMigrationLayoutOverridesV1 {
            projects_path: args.config.projects_path,
            state_dir: args.config.state_dir,
        },
    )?;
    if args.preflight {
        let result = ProjectCatalogMigrationFacadeV1::preflight(
            ProjectCatalogMigrationPreflightRequestV1 {
                layout: source_layout,
                report_path: args.report,
                resolution_path: args.resolution,
                sensitive_report_path: args.include_local_paths,
            },
        )?;
        return serialize_result(&result.receipt);
    }

    debug_assert!(args.apply, "clap requires exactly one migration mode");
    let rehearsal_root = args.rehearsal_root.ok_or_else(|| {
        CommandFailure::new(
            "error.project_catalog_cli_arguments",
            "--apply requires --rehearsal-root",
        )
    })?;
    let rehearsal_layout =
        ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(rehearsal_root, &config)?;
    let result =
        ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
            rehearsal_layout,
            protected_layout: source_layout,
            report_path: args.report,
            resolution_path: args.resolution,
        })?;
    serialize_result(&result.receipt)
}

fn execute_verify(args: VerifyArgs) -> Result<serde_json::Value, CommandFailure> {
    let config = load_config(args.config)?;
    let rehearsal_layout =
        ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(args.root, &config)?;
    let result = ProjectCatalogMigrationFacadeV1::verify(ProjectCatalogMigrationVerifyRequestV1 {
        rehearsal_layout,
    })?;
    serialize_result(result.receipt())
}

fn load_config(path: Option<PathBuf>) -> Result<config::Config, CommandFailure> {
    config::load_with(LoadOptions {
        config_path: path,
        ..Default::default()
    })
    .map_err(|_| {
        CommandFailure::new(
            "error.project_catalog_cli_config",
            "shared blackbox configuration could not be loaded",
        )
    })
}

fn serialize_result(value: &impl Serialize) -> Result<serde_json::Value, CommandFailure> {
    serde_json::to_value(value).map_err(|_| {
        CommandFailure::new(
            "error.project_catalog_cli_result",
            "migration result could not be serialized",
        )
    })
}

fn write_json(value: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(&bytes)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_definition_is_self_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parser_selects_each_documented_command() {
        let preflight = Cli::try_parse_from([
            "blackbox",
            "project-catalog",
            "migrate",
            "--preflight",
            "--report",
            "/tmp/report.json",
            "--resolution",
            "/tmp/resolution.json",
        ])
        .unwrap();
        assert_eq!(
            command_name(&preflight),
            "project_catalog_migrate_preflight"
        );

        let apply = Cli::try_parse_from([
            "blackbox",
            "project-catalog",
            "migrate",
            "--apply",
            "--report",
            "/tmp/report.json",
            "--resolution",
            "/tmp/resolution.json",
            "--rehearsal-root",
            "/tmp/rehearsal",
        ])
        .unwrap();
        assert_eq!(command_name(&apply), "project_catalog_migrate_apply");

        let verify = Cli::try_parse_from([
            "blackbox",
            "project-catalog",
            "verify",
            "--root",
            "/tmp/rehearsal",
        ])
        .unwrap();
        assert_eq!(command_name(&verify), "project_catalog_verify");
    }

    #[test]
    fn parser_refuses_ambiguous_or_unsafe_mode_combinations() {
        assert!(
            Cli::try_parse_from([
                "blackbox",
                "project-catalog",
                "migrate",
                "--preflight",
                "--apply",
                "--report",
                "/tmp/report.json",
                "--resolution",
                "/tmp/resolution.json",
                "--rehearsal-root",
                "/tmp/rehearsal",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "blackbox",
                "project-catalog",
                "migrate",
                "--apply",
                "--report",
                "/tmp/report.json",
                "--resolution",
                "/tmp/resolution.json",
            ])
            .is_err()
        );
    }
}
