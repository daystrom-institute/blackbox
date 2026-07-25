use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use bbox_config::config::{self, LoadOptions};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope, ScopeMigrationKind};
use bbox_indexing::project_catalog_admin;
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationApplyRequestV1, ProjectCatalogMigrationError,
    ProjectCatalogMigrationFacadeV1, ProjectCatalogMigrationLayoutOverridesV1,
    ProjectCatalogMigrationPreflightRequestV1, ProjectCatalogMigrationResolvedLayoutV1,
    ProjectCatalogMigrationVerifyRequestV1,
};
use bbox_indexing::project_catalog_migration_lock::ProjectCatalogMigrationLock;
use bbox_indexing::project_catalog_store::{ProjectCatalogStore, ProjectCatalogStoreError};
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
    /// Create a catalog project by authoritative scope or as legacy-local.
    Add(AddArgs),
    /// List every catalog project, including remote-only projects.
    List(StoreArgs),
    /// Inspect one catalog project.
    Get(GetArgs),
    /// Accept or reject one nominated alias.
    Alias(AliasArgs),
    /// Operator-attested unattached scope migration.
    ScopeMigrate(ScopeMigrateArgs),
    /// Inventory and optionally remove one fully discharged project.
    Retire(RetireArgs),
}

#[derive(Debug, Args)]
struct StoreArgs {
    /// Exact strict v2 projects store to administer.
    #[arg(long, value_name = "PATH")]
    projects_path: PathBuf,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("add_kind")
        .required(true)
        .multiple(false)
        .args(["repo_id", "legacy_local"])
))]
struct AddArgs {
    #[command(flatten)]
    store: StoreArgs,
    /// Recorded repository authority for a published project.
    #[arg(long, value_name = "REPO_ID", requires = "relpath")]
    repo_id: Option<String>,
    /// Monorepo root relpath for a published project (`.` at the root).
    #[arg(long, value_name = "RELPATH")]
    relpath: Option<String>,
    /// Create a legacy-local project instead of a published one.
    #[arg(long)]
    legacy_local: bool,
    #[arg(long, value_name = "NAME")]
    display_name: String,
    /// Initial accepted operator alias. Repeatable.
    #[arg(long = "alias", value_name = "ALIAS")]
    aliases: Vec<String>,
    /// Bounded creation timestamp recorded on the project.
    #[arg(long, value_name = "TIMESTAMP")]
    created_at: String,
}

#[derive(Debug, Args)]
struct GetArgs {
    #[command(flatten)]
    store: StoreArgs,
    /// Exact project id.
    #[arg(long, value_name = "PROJECT_ID")]
    project: String,
}

#[derive(Debug, Args)]
struct AliasArgs {
    #[command(subcommand)]
    decision: AliasDecision,
}

#[derive(Debug, Subcommand)]
enum AliasDecision {
    /// Move one pending nomination into the accepted operator aliases.
    Accept(AliasDecisionArgs),
    /// Drop one pending nomination.
    Reject(AliasDecisionArgs),
}

#[derive(Debug, Args)]
struct AliasDecisionArgs {
    #[command(flatten)]
    store: StoreArgs,
    #[arg(long, value_name = "PROJECT_ID")]
    project: String,
    #[arg(long, value_name = "ALIAS")]
    alias: String,
}

#[derive(Debug, Args)]
struct ScopeMigrateArgs {
    #[command(flatten)]
    store: StoreArgs,
    /// The offline unattached channel. The only supported mode.
    #[arg(long, required = true)]
    operator_attested: bool,
    #[arg(long, value_name = "PROJECT_ID")]
    project: String,
    #[arg(long, value_name = "REPO_ID")]
    expected_old_repo: String,
    #[arg(long, value_name = "RELPATH")]
    expected_old_relpath: String,
    #[arg(long, value_name = "REPO_ID")]
    new_repo: String,
    #[arg(long, value_name = "RELPATH")]
    new_relpath: String,
    /// relpath-move or repo-authority-change.
    #[arg(long, value_name = "KIND")]
    kind: String,
    /// Mandatory operator acknowledgement for the unattached channel.
    #[arg(long)]
    acknowledge_unattached_scope_migration: bool,
    /// Additionally required for a recorded-authority change.
    #[arg(long)]
    acknowledge_repo_authority_change: bool,
    /// Bounded operator reason recorded on the migration.
    #[arg(long, value_name = "REASON")]
    reason: String,
    /// Bounded migration timestamp recorded on the migration.
    #[arg(long, value_name = "TIMESTAMP")]
    migrated_at: String,
}

#[derive(Debug, Args)]
struct RetireArgs {
    #[command(flatten)]
    store: StoreArgs,
    #[arg(long, value_name = "PROJECT_ID")]
    project: String,
    /// Remove the project after a clean inventory. Default reports only.
    #[arg(long)]
    execute: bool,
    /// Load the same configuration file used by blackboxd.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
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
    #[arg(
        long,
        value_name = "PATH",
        required_if_eq("apply", "true"),
        conflicts_with = "preflight"
    )]
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

impl From<ProjectCatalogStoreError> for CommandFailure {
    fn from(error: ProjectCatalogStoreError) -> Self {
        Self::new(error.code(), error.to_string())
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
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Add(_),
        }) => "project_catalog_add",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::List(_),
        }) => "project_catalog_list",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Get(_),
        }) => "project_catalog_get",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command:
                ProjectCatalogCommand::Alias(AliasArgs {
                    decision: AliasDecision::Accept(_),
                }),
        }) => "project_catalog_alias_accept",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command:
                ProjectCatalogCommand::Alias(AliasArgs {
                    decision: AliasDecision::Reject(_),
                }),
        }) => "project_catalog_alias_reject",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::ScopeMigrate(_),
        }) => "project_catalog_scope_migrate_attested",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Retire(_),
        }) => "project_catalog_retire",
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
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Add(args),
        }) => execute_add(args),
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::List(args),
        }) => execute_list(args),
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Get(args),
        }) => execute_get(args),
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Alias(args),
        }) => execute_alias(args),
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::ScopeMigrate(args),
        }) => execute_scope_migrate(args),
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Retire(args),
        }) => execute_retire(args),
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
            "shared blackbox configuration is invalid or unreadable",
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
        assert!(
            Cli::try_parse_from([
                "blackbox",
                "project-catalog",
                "migrate",
                "--preflight",
                "--report",
                "/tmp/report.json",
                "--resolution",
                "/tmp/resolution.json",
                "--rehearsal-root",
                "/tmp/rehearsal",
            ])
            .is_err()
        );
    }
}

/// Open a strict v2 store for offline administration: the exclusive
/// lifetime lock proves no daemon shares the store (mutations are
/// CLI-only while stopped, plan §7.9), and `open_existing` fails closed on
/// v1 bytes, which is the constructive D-002 boundary: these subcommands
/// cannot create or mutate v2 state at a configured v1 path.
fn open_admin_store(
    projects_path: &PathBuf,
) -> Result<(ProjectCatalogMigrationLock, ProjectCatalogStore), CommandFailure> {
    // Prove no daemon shares this store, then atomically downgrade so the
    // strict open can take its own shared handle on the same lock file
    // (holding exclusive across the open would deadlock against it). The
    // downgraded guard keeps continuous lock coverage for the CLI's
    // lifetime; mutation correctness itself is owned by the pair
    // transaction's locks.
    let exclusive = ProjectCatalogMigrationLock::try_acquire_exclusive(projects_path)
        .map_err(|error| {
            CommandFailure::new("error.project_catalog_cli_lock", format!("{error:#}"))
        })?
        .ok_or_else(|| {
            CommandFailure::new(
                "error.project_catalog_cli_lock",
                "the lifetime migration lock is held; stop the daemon before \
                 offline administration",
            )
        })?;
    let shared = exclusive.downgrade_to_shared().map_err(|error| {
        CommandFailure::new("error.project_catalog_cli_lock", format!("{error:#}"))
    })?;
    let store = ProjectCatalogStore::open_existing(projects_path)?;
    Ok((shared, store))
}

fn current_epoch(store: &ProjectCatalogStore) -> Result<u64, CommandFailure> {
    Ok(store.snapshot()?.epoch())
}

fn parse_project_id(raw: &str) -> Result<ProjectId, CommandFailure> {
    ProjectId::parse(raw)
        .map_err(|error| CommandFailure::new(error.code(), "project id is malformed"))
}

fn parse_scope(repo: &str, relpath: &str) -> Result<PublishedScope, CommandFailure> {
    PublishedScope::try_new(repo, relpath)
        .map_err(|_| CommandFailure::new("error.project_catalog_cli_arguments", "invalid scope"))
}

fn scope_json(scope: &ProjectScope) -> serde_json::Value {
    match scope {
        ProjectScope::LegacyLocal => serde_json::json!({"kind": "legacy_local"}),
        ProjectScope::Published(scope) => serde_json::json!({
            "kind": "published",
            "repo_id": scope.repo_id(),
            "bbox_root_relpath": scope.bbox_root_relpath(),
        }),
    }
}

fn execute_add(args: AddArgs) -> Result<serde_json::Value, CommandFailure> {
    let (_lock, store) = open_admin_store(&args.store.projects_path)?;
    let kind = if args.legacy_local {
        project_catalog_admin::CatalogAddKind::LegacyLocal
    } else {
        let repo = args.repo_id.as_deref().ok_or_else(|| {
            CommandFailure::new(
                "error.project_catalog_cli_arguments",
                "--repo-id is required",
            )
        })?;
        let relpath = args.relpath.as_deref().ok_or_else(|| {
            CommandFailure::new(
                "error.project_catalog_cli_arguments",
                "--relpath is required",
            )
        })?;
        project_catalog_admin::CatalogAddKind::Published(parse_scope(repo, relpath)?)
    };
    let epoch = current_epoch(&store)?;
    let (project_id, commit) = project_catalog_admin::catalog_add(
        &store,
        epoch,
        &kind,
        &args.display_name,
        &args.aliases,
        &args.created_at,
    )?;
    Ok(serde_json::json!({
        "project_id": project_id.as_str(),
        "epoch": commit.epoch,
        "catalog_sha256": commit.catalog_sha256,
    }))
}

fn execute_list(args: StoreArgs) -> Result<serde_json::Value, CommandFailure> {
    let store = ProjectCatalogStore::open_existing(&args.projects_path)?;
    let state = store.snapshot()?;
    let projects: Vec<serde_json::Value> = state
        .catalog()
        .projects
        .values()
        .map(|project| {
            let attached = state
                .attachments()
                .attachments
                .values()
                .filter(|row| {
                    row.project_id == project.project_id
                        && row.status
                            == bbox_corpus_core::project_catalog::AttachmentStatus::Attached
                })
                .count();
            serde_json::json!({
                "project_id": project.project_id.as_str(),
                "display_name": project.display_name,
                "scope": scope_json(&project.scope),
                "operator_aliases": project.operator_aliases,
                "nominated_aliases": project.nominated_aliases,
                "active_attachments": attached,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "epoch": state.epoch(),
        "projects": projects,
    }))
}

fn execute_get(args: GetArgs) -> Result<serde_json::Value, CommandFailure> {
    let store = ProjectCatalogStore::open_existing(&args.store.projects_path)?;
    let state = store.snapshot()?;
    let project_id = parse_project_id(&args.project)?;
    let Some(project) = state.catalog().projects.get(&project_id) else {
        return Err(CommandFailure::new(
            "error.project_catalog_admin_unknown_project",
            "project is not in the catalog",
        ));
    };
    // Attachment paths are host-local operator data on this explicitly
    // host-local surface (plan §7.2); the catalog section stays path-free.
    let attachments: Vec<serde_json::Value> = state
        .attachments()
        .attachments
        .values()
        .filter(|row| row.project_id == project_id)
        .map(|row| {
            serde_json::json!({
                "attachment_id": row.attachment_id.as_str(),
                "status": format!("{:?}", row.status),
                "kind": format!("{:?}", row.kind),
                "checkout_project_dir": row.checkout_project_dir,
                "project_root_relpath": row.project_root_relpath,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "epoch": state.epoch(),
        "project": {
            "project_id": project.project_id.as_str(),
            "display_name": project.display_name,
            "scope": scope_json(&project.scope),
            "operator_aliases": project.operator_aliases,
            "nominated_aliases": project.nominated_aliases,
            "repo_history": project.repo_history.as_ref().map(|id| id.as_str()),
        },
        "host_local_attachments": attachments,
    }))
}

fn execute_alias(args: AliasArgs) -> Result<serde_json::Value, CommandFailure> {
    let (accept, inner) = match args.decision {
        AliasDecision::Accept(inner) => (true, inner),
        AliasDecision::Reject(inner) => (false, inner),
    };
    let (_lock, store) = open_admin_store(&inner.store.projects_path)?;
    let project_id = parse_project_id(&inner.project)?;
    let epoch = current_epoch(&store)?;
    let commit =
        project_catalog_admin::alias_decide(&store, epoch, &project_id, &inner.alias, accept)?;
    Ok(serde_json::json!({
        "alias": inner.alias,
        "accepted": accept,
        "epoch": commit.epoch,
    }))
}

fn execute_scope_migrate(args: ScopeMigrateArgs) -> Result<serde_json::Value, CommandFailure> {
    let (_lock, store) = open_admin_store(&args.store.projects_path)?;
    let kind = match args.kind.as_str() {
        "relpath-move" => ScopeMigrationKind::RelpathMove,
        "repo-authority-change" => ScopeMigrationKind::RepoAuthorityChange,
        other => {
            return Err(CommandFailure::new(
                "error.project_catalog_cli_arguments",
                format!("unsupported migration kind: {other}"),
            ));
        }
    };
    let request = project_catalog_admin::ScopeMigrationRequest {
        project_id: parse_project_id(&args.project)?,
        expected_old_scope: parse_scope(&args.expected_old_repo, &args.expected_old_relpath)?,
        new_scope: parse_scope(&args.new_repo, &args.new_relpath)?,
        kind,
        designated_attachment: bbox_corpus_core::project_catalog::AttachmentId::mint(),
        acknowledge_repo_authority_change: args.acknowledge_repo_authority_change,
        attachment_probes: Default::default(),
        code_bridge_generation: None,
        publication_bridge_generation: None,
        operator_invocation: "cli:project-catalog scope-migrate --operator-attested".into(),
        operator_reason: Some(args.reason.clone()),
        migrated_at: args.migrated_at.clone(),
    };
    let epoch = current_epoch(&store)?;
    let receipt = project_catalog_admin::scope_migrate_attested(
        &store,
        epoch,
        &request,
        args.acknowledge_unattached_scope_migration,
    )?;
    Ok(serde_json::json!({
        "scope_migration_id": receipt.scope_migration_id.as_str(),
        "epoch": receipt.commit.epoch,
    }))
}

fn execute_retire(args: RetireArgs) -> Result<serde_json::Value, CommandFailure> {
    let (_lock, store) = open_admin_store(&args.store.projects_path)?;
    let project_id = parse_project_id(&args.project)?;
    let evidence = probe_retire_evidence(&args, &store, &project_id)?;
    let epoch = current_epoch(&store)?;
    let (inventory, commit) =
        project_catalog_admin::retire_project(&store, epoch, &project_id, &evidence, args.execute)?;
    Ok(serde_json::json!({
        "blocking": inventory.blocking,
        "removable_attachments": inventory.removable_attachments,
        "removable_migrations": inventory.removable_migrations,
        "removable_bindings": inventory.removable_bindings,
        "removed": commit.is_some(),
        "epoch": commit.map(|c| c.epoch),
    }))
}

/// Bounded external-reference probing for retire (plan §7.8): code-source
/// activation and retained generations, the accepted-publication pointer,
/// and project-selector rows in the shared coordination stores. Rows are
/// matched by exact project id or any of the project's recorded attachment
/// directories.
fn probe_retire_evidence(
    args: &RetireArgs,
    store: &ProjectCatalogStore,
    project_id: &ProjectId,
) -> Result<project_catalog_admin::RetireEvidence, CommandFailure> {
    let config = config::load_with(LoadOptions {
        config_path: args.config.clone(),
        ..Default::default()
    })
    .map_err(|error| {
        CommandFailure::new("error.project_catalog_cli_config", format!("{error:#}"))
    })?;
    let mut evidence = project_catalog_admin::RetireEvidence::default();
    let state = store.snapshot()?;
    let selectors: Vec<String> = std::iter::once(project_id.as_str().to_string())
        .chain(
            state
                .attachments()
                .attachments
                .values()
                .filter(|row| &row.project_id == project_id)
                .map(|row| row.checkout_project_dir.clone()),
        )
        .collect();

    // Code-source state roots on the configured state dir (the same
    // derivation the daemon uses), never the projects-path parent.
    let code_sources = config.paths.state_dir.join("code-sources");
    if let Ok(paths) = bbox_code_source_store::CodeSourceStorePaths::new(&code_sources) {
        if paths.activation(project_id).exists() {
            evidence
                .external_reference_counts
                .insert("code_source_activation".into(), 1);
        }
        // Retained generations are scope-keyed; a published project's
        // scope directory holds them.
        if let Some(project) = state.catalog().projects.get(project_id)
            && let ProjectScope::Published(scope) = &project.scope
        {
            let scope_dir = code_sources
                .join("scopes")
                .join(bbox_code_source::scope_hash(scope));
            if scope_dir.exists() {
                evidence
                    .external_reference_counts
                    .insert("code_source_generations".into(), 1);
            }
        }
    }
    let pointer = args
        .store
        .projects_path
        .parent()
        .map(|parent| {
            parent
                .join("accepted-publications")
                .join("pointers")
                .join(format!("{project_id}.json"))
        })
        .unwrap_or_default();
    if pointer.exists() {
        evidence
            .external_reference_counts
            .insert("accepted_publication_pointer".into(), 1);
    }

    for (class, path) in [
        ("knowledge_rows", &config.paths.knowledge_path),
        ("gap_rows", &config.paths.gaps_path),
        ("thread_rows", &config.paths.threads_path),
        ("note_rows", &config.paths.notes_path),
        ("pin_rows", &config.paths.pins_path),
        ("roadmap_rows", &config.paths.roadmap_path),
    ] {
        let count = count_project_rows(path, &selectors);
        if count > 0 {
            evidence
                .external_reference_counts
                .insert(class.to_string(), count);
        }
    }
    Ok(evidence)
}

/// Count rows in one JSON coordination store whose `project` or
/// `project_id` field names the retiring project. Loose by design: an
/// unreadable store counts as one blocking reference rather than zero.
fn count_project_rows(path: &std::path::Path, selectors: &[String]) -> u64 {
    if !path.exists() {
        return 0;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return 1;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return 1;
    };
    let mut count = 0_u64;
    let mut stack = vec![&value];
    while let Some(node) = stack.pop() {
        match node {
            serde_json::Value::Array(items) => stack.extend(items.iter()),
            serde_json::Value::Object(map) => {
                let hit = ["project", "project_id"].iter().any(|key| {
                    map.get(*key)
                        .and_then(|v| v.as_str())
                        .is_some_and(|v| selectors.iter().any(|s| s == v))
                });
                if hit {
                    count += 1;
                } else {
                    stack.extend(map.values());
                }
            }
            _ => {}
        }
    }
    count
}
