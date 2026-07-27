use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bbox_config::config::{self, LoadOptions};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope, ScopeMigrationKind};
use bbox_corpus_index::index::migration_inventory as corpus_inventory;
use bbox_indexing::project_catalog_admin;
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationApplyRequestV1, ProjectCatalogMigrationError,
    ProjectCatalogMigrationFacadeV1, ProjectCatalogMigrationLayoutOverridesV1,
    ProjectCatalogMigrationPreflightRequestV1, ProjectCatalogMigrationResolvedLayoutV1,
    ProjectCatalogMigrationVerifyRequestV1,
};
use bbox_indexing::project_catalog_migration_lock::ProjectCatalogMigrationLock;
use bbox_indexing::project_catalog_store::{ProjectCatalogStore, ProjectCatalogStoreError};
use bbox_vectors::migration_inventory as vector_inventory;
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
    /// Clear a stale or broken code bridge on a scope-migrated project.
    ScopeBridgeClear(ScopeBridgeClearArgs),
    /// Inventory and optionally remove one fully discharged project.
    Retire(RetireArgs),
    /// Resume or inspect a forward-only retirement journal (section 11).
    RetirementJournal(RetirementJournalArgs),
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
    /// Catalog epoch the operator read when the nomination was surfaced.
    /// A nomination accepted against a stale epoch refuses and the operator
    /// re-reads (plan §7.6). Omitted, the store's current epoch is used.
    #[arg(long, value_name = "EPOCH")]
    expected_epoch: Option<u64>,
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
    /// Load the same configuration file used by blackboxd. The bridge
    /// generations are probed from the state roots it resolves.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ScopeBridgeClearArgs {
    #[command(flatten)]
    store: StoreArgs,
    #[arg(long, value_name = "PROJECT_ID")]
    project: String,
    /// Mode 1: clear a dangling reference (named generation retired).
    #[arg(long)]
    dangling_reference: bool,
    /// Mode 2: double-migration truthfulness repair (null newest bridge).
    #[arg(long)]
    double_migration_repair: bool,
    /// Load the same configuration file used by blackboxd.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
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

#[derive(Debug, Args)]
struct RetirementJournalArgs {
    #[command(flatten)]
    store: StoreArgs,
    #[arg(long, value_name = "PROJECT_ID")]
    project: String,
    /// Resume or execute the journal discharge. Default reports only.
    #[arg(long)]
    execute: bool,
    /// Load the same configuration file used by blackboxd.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Override BRO_HOME for journal persistence. Defaults to the
    /// config-resolved bro_home.
    #[arg(long, value_name = "PATH")]
    bro_home: Option<PathBuf>,
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
            command: ProjectCatalogCommand::ScopeBridgeClear(_),
        }) => "project_catalog_scope_bridge_clear",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Retire(_),
        }) => "project_catalog_retire",
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::RetirementJournal(_),
        }) => "project_catalog_retirement_journal",
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
            command: ProjectCatalogCommand::ScopeBridgeClear(args),
        }) => execute_scope_bridge_clear(args),
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::Retire(args),
        }) => execute_retire(args),
        TopLevelCommand::ProjectCatalog(ProjectCatalogArgs {
            command: ProjectCatalogCommand::RetirementJournal(args),
        }) => execute_retirement_journal(args),
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

/// Load the shared configuration for one offline command.
///
/// The shared loader skips a missing configuration file so the daemon can
/// start on defaults. That is wrong for an operator who named a path on this
/// surface: every offline command derives its state roots from the result, so
/// a typo would silently administer the default roots instead. An explicitly
/// named path must therefore exist, tested without following a symlink; the
/// daemon-facing loader semantics stay unchanged.
fn load_config(path: Option<PathBuf>) -> Result<config::Config, CommandFailure> {
    if let Some(path) = path.as_deref()
        && !matches!(std::fs::symlink_metadata(path), Ok(metadata) if metadata.is_file())
    {
        return Err(CommandFailure::new(
            "error.project_catalog_cli_config",
            format!(
                "--config named {} but no regular configuration file exists there",
                path.display()
            ),
        ));
    }
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

/// Probe the code-source store for bridge-clear evidence (F4).
///
/// Opens the code-source store (sibling of the catalog path) and reads
/// the activation record's effective generation id. For mode 1
/// (dangling-reference), this proves the bridge generation is retired.
fn probe_bridge_clear_evidence(
    projects_path: &PathBuf,
    project_id: &bbox_corpus_core::project_catalog::ProjectId,
) -> Result<project_catalog_admin::ScopeBridgeClearEvidence, CommandFailure> {
    let code_source_dir = projects_path
        .parent()
        .ok_or_else(|| {
            CommandFailure::new(
                "error.project_catalog_cli_store_path",
                "cannot resolve code-source directory from the catalog path",
            )
        })?
        .join("code-sources");
    if !code_source_dir.is_dir() {
        // No code-source store: no evidence available. Mode 1 will
        // refuse with missing-evidence; mode 2 does not need it.
        return Ok(project_catalog_admin::ScopeBridgeClearEvidence::default());
    }
    let code_store = bbox_code_source_store::CodeSourceStore::open(
        &code_source_dir,
        bbox_code_source_store::StoreLimits::default(),
    )
    .map_err(|e| {
        CommandFailure::new(
            "error.project_catalog_cli_code_source_open",
            format!("failed to open code-source store at {}: {e}", code_source_dir.display()),
        )
    })?;
    let effective_generation_id = code_store
        .load_activation_mixed(project_id.as_str())
        .ok()
        .flatten()
        .map(|a| a.generation_id().to_string());
    Ok(project_catalog_admin::ScopeBridgeClearEvidence {
        effective_generation_id,
    })
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
    // The epoch the operator read when the nomination was surfaced is the
    // authority: `alias_decide` compares-and-swaps on it, so a nomination
    // accepted against a stale read surfaces the store's typed stale-epoch
    // refusal (plan §7.6). Omitting the flag keeps the pre-existing
    // read-then-decide behaviour for operators driving the store by hand.
    let epoch = match inner.expected_epoch {
        Some(expected) => expected,
        None => current_epoch(&store)?,
    };
    let commit =
        project_catalog_admin::alias_decide(&store, epoch, &project_id, &inner.alias, accept)?;
    Ok(serde_json::json!({
        "alias": inner.alias,
        "accepted": accept,
        "epoch": commit.epoch,
    }))
}

fn execute_scope_migrate(args: ScopeMigrateArgs) -> Result<serde_json::Value, CommandFailure> {
    let config = load_config(args.config.clone())?;
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
    let project_id = parse_project_id(&args.project)?;
    // Bridge generations are recorded on the attested record exactly as the
    // attachment-proved channel records them (plan §7.5): an active collected
    // generation or an accepted publication pointer must survive the scope
    // change as evidence, and a channel that leaves them unset would write a
    // record that cannot be told apart from a project holding neither.
    let request = project_catalog_admin::ScopeMigrationRequest {
        expected_old_scope: parse_scope(&args.expected_old_repo, &args.expected_old_relpath)?,
        new_scope: parse_scope(&args.new_repo, &args.new_relpath)?,
        kind,
        designated_attachment: bbox_corpus_core::project_catalog::AttachmentId::mint(),
        acknowledge_repo_authority_change: args.acknowledge_repo_authority_change,
        attachment_probes: Default::default(),
        code_bridge_generation: code_bridge_generation(&config.paths.state_dir, &project_id)?,
        publication_bridge_generation: publication_bridge_generation(
            &args.store.projects_path,
            &project_id,
        )?,
        project_id,
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

fn execute_scope_bridge_clear(
    args: ScopeBridgeClearArgs,
) -> Result<serde_json::Value, CommandFailure> {
    let (_lock, store) = open_admin_store(&args.store.projects_path)?;
    let project_id = parse_project_id(&args.project)?;
    let mode = match (args.dangling_reference, args.double_migration_repair) {
        (true, false) => project_catalog_admin::ScopeBridgeClearMode::DanglingReference,
        (false, true) => project_catalog_admin::ScopeBridgeClearMode::DoubleMigrationRepair,
        (false, false) => {
            return Err(CommandFailure::new(
                "error.project_catalog_cli_arguments",
                "specify exactly one of --dangling-reference or --double-migration-repair",
            ));
        }
        (true, true) => {
            return Err(CommandFailure::new(
                "error.project_catalog_cli_arguments",
                "--dangling-reference and --double-migration-repair are mutually exclusive",
            ));
        }
    };
    let epoch = current_epoch(&store)?;
    // F4: probe code-source evidence for the bridge-clear precondition.
    // For mode 1 (dangling-reference), the effective generation id must
    // differ from the bridge generation (proving the bridge is retired).
    let evidence = probe_bridge_clear_evidence(&args.store.projects_path, &project_id)?;
    let commit =
        project_catalog_admin::clear_scope_bridge(&store, epoch, &project_id, mode, &evidence)?;
    Ok(serde_json::json!({
        "project_id": project_id.as_str(),
        "mode": match mode {
            project_catalog_admin::ScopeBridgeClearMode::DanglingReference => "dangling_reference",
            project_catalog_admin::ScopeBridgeClearMode::DoubleMigrationRepair => {
                "double_migration_repair"
            }
        },
        "epoch": commit.epoch,
    }))
}

fn execute_retire(args: RetireArgs) -> Result<serde_json::Value, CommandFailure> {
    let config = load_config(args.config.clone())?;
    let (_lock, store) = open_admin_store(&args.store.projects_path)?;
    let project_id = parse_project_id(&args.project)?;
    let probe = probe_retire_evidence(&config, &args.store.projects_path, &store, &project_id)?;
    // An unprobeable class is not a discharged class. Removal is permanent
    // and strict cross-validation forbids partial removal, so a class the
    // probe could not read refuses the destructive arm by name instead of
    // being counted as zero (plan §7.8).
    if args.execute && !probe.unprobeable.is_empty() {
        return Err(CommandFailure::new(
            "error.project_catalog_cli_unprobeable_reference_class",
            format!(
                "these reference classes could not be probed and may still hold references: {}",
                probe.unprobeable.join(", ")
            ),
        ));
    }
    let epoch = current_epoch(&store)?;
    let (inventory, commit) = project_catalog_admin::retire_project(
        &store,
        epoch,
        &project_id,
        &probe.evidence,
        args.execute,
    )?;
    Ok(serde_json::json!({
        "blocking": inventory.blocking,
        "probed_reference_classes": RETIRE_REFERENCE_CLASSES,
        "unprobeable_reference_classes": probe.unprobeable,
        "removable_attachments": inventory.removable_attachments,
        "removable_migrations": inventory.removable_migrations,
        "removable_bindings": inventory.removable_bindings,
        "removed": commit.is_some(),
        "epoch": commit.map(|c| c.epoch),
    }))
}

/// Execute the forward-only retirement journal (section 11).
///
/// This is the CLI-only, offline execution lane. The caller must have
/// already stopped the daemon (the exclusive lifetime lock from
/// open_admin_store guarantees no concurrent daemon holds the catalog).
/// The discharge ordering is:
///
/// 1. Preflight (section 11.2): resolve the project, probe evidence,
///    inventory blocking classes, detect Ready-materialization refusal.
/// 2. If --execute is absent, report preflight only.
/// 3. If --execute is set, discharge blocking classes to zero, then
///    call retire_project_journaled which advances the journal
///    forward to Complete.
/// 4. Archive the completed journal so the P4-F startup probe does
///    not refuse the next boot.
fn execute_retirement_journal(
    args: RetirementJournalArgs,
) -> Result<serde_json::Value, CommandFailure> {
    let config = load_config(args.config.clone())?;
    let (_lock, store) = open_admin_store(&args.store.projects_path)?;
    let project_id = parse_project_id(&args.project)?;
    let probe = probe_retire_evidence(&config, &args.store.projects_path, &store, &project_id)?;

    // Unprobeable classes refuse the destructive arm (same rule as retire).
    if args.execute && !probe.unprobeable.is_empty() {
        return Err(CommandFailure::new(
            "error.project_catalog_cli_unprobeable_reference_class",
            format!(
                "these reference classes could not be probed and may still hold references: {}",
                probe.unprobeable.join(", ")
            ),
        ));
    }

    let bro_home = args
        .bro_home
        .unwrap_or_else(|| config.paths.bro_home.clone());

    let mut workers = CliRetirementDischargeWorkers::new(&config, &args.store.projects_path);
    let (preflight, journal) = project_catalog_admin::retire_project_journaled_with(
        &store,
        &bro_home,
        &project_id,
        &probe.evidence,
        args.execute,
        &mut workers,
    )?;

    Ok(serde_json::json!({
        "project_id": project_id.as_str(),
        "execute": args.execute,
        "blocking": preflight.blocking,
        "history_ready_refusal": preflight.history_ready_refusal,
        "source_owned_records": preflight.source_owned_records,
        "project_exists": preflight.project_exists,
        "catalog_epoch": preflight.catalog_epoch,
        "probed_reference_classes": RETIRE_REFERENCE_CLASSES,
        "unprobeable_reference_classes": probe.unprobeable,
        "journal": journal.as_ref().map(|j| serde_json::json!({
            "current_stage": format!("{:?}", j.current_stage),
            "completed_steps": j.completed_steps.len(),
            "stages": j.completed_steps.iter().map(|s| serde_json::json!({
                "stage": format!("{:?}", s.stage),
                "completed_at": s.completed_at,
            })).collect::<Vec<_>>(),
        })),
    }))
}

/// CLI-level discharge workers for the retirement journal (section 11.3).
///
/// Each method is a single-attempt library-level primitive with no retry
/// loops (section 11.1). The CLI has all roots offline under the
/// exclusive lifetime lock, so these workers open read/write store
/// handles directly.
struct CliRetirementDischargeWorkers<'a> {
    config: &'a config::Config,
    projects_path: &'a Path,
}

impl<'a> CliRetirementDischargeWorkers<'a> {
    fn new(config: &'a config::Config, projects_path: &'a Path) -> Self {
        Self {
            config,
            projects_path,
        }
    }
}

impl<'a> project_catalog_admin::RetirementDischargeWorkers for CliRetirementDischargeWorkers<'a> {
    /// Stage CollectedGenerationsDischarged: clear the activation record,
    /// delete source-owned generation records, and clear project-scoped
    /// coordination rows (knowledge, gaps, threads, notes, pins, etc.).
    fn discharge_collected_generations(
        &mut self,
        project_id: &ProjectId,
    ) -> project_catalog_admin::AdminResult<()> {
        let code_sources = self.config.paths.state_dir.join("code-sources");
        if let Ok(store) = bbox_code_source_store::CodeSourceStore::open(
            &code_sources,
            bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits(
                self.config,
            ),
        ) {
            // Clear the activation record (single-attempt, idempotent).
            let _ = store.clear_activation(project_id.as_str());

            // Delete generation records for the project's scopes.
            for scope_hash in scope_dirs(&code_sources) {
                let _ = delete_generations_for_project_in_scope(
                    &code_sources,
                    &scope_hash,
                    project_id.as_str(),
                );
            }
        }

        // Clear project-scoped coordination rows.
        for (path, keys) in coordination_row_paths(self.config) {
            let _ = clear_project_rows(&path, keys, project_id.as_str());
        }

        Ok(())
    }

    /// Stage PublicationsCleared: delete the accepted-publication pointer.
    fn discharge_publications(
        &mut self,
        project_id: &ProjectId,
    ) -> project_catalog_admin::AdminResult<()> {
        if let Some(pointer) = accepted_publication_pointer(self.projects_path, project_id) {
            match std::fs::remove_file(&pointer) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(project_catalog_admin::admin_error(
                        "error.project_catalog_retire_discharge_publication",
                        format!("failed to delete accepted-publication pointer: {e}"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Stage AttachmentsDetached: detach the project's active attachments
    /// through a catalog pair transact.
    fn discharge_attachments(
        &mut self,
        store: &ProjectCatalogStore,
        project_id: &ProjectId,
    ) -> project_catalog_admin::AdminResult<()> {
        use bbox_corpus_core::project_catalog::AttachmentStatus;
        let state = store.snapshot()?;
        let epoch = state.epoch();
        let pid = project_id.clone();
        store.transact(epoch, move |_catalog, attachments| {
            attachments
                .attachments
                .values_mut()
                .filter(|row| row.project_id == pid && row.status == AttachmentStatus::Attached)
                .for_each(|row| {
                    row.status = AttachmentStatus::Detached;
                });
            Ok(())
        })?;
        Ok(())
    }

    /// Stage MaterializationSwept: delete blobs only when shared-history
    /// reference accounting reaches zero. When other projects still
    /// reference shared blobs, the sweep skips them.
    fn sweep_materialization(
        &mut self,
        project_id: &ProjectId,
    ) -> project_catalog_admin::AdminResult<()> {
        let code_sources = self.config.paths.state_dir.join("code-sources");
        let Ok(paths) = bbox_code_source_store::CodeSourceStorePaths::new(&code_sources) else {
            return Ok(());
        };
        let blob_dir = paths.root().join("blobs/sha256");
        if !blob_dir.is_dir() {
            return Ok(());
        }

        let project_blobs = collect_project_blob_hashes(&code_sources, project_id);
        if project_blobs.is_empty() {
            return Ok(());
        }

        let other_referenced = collect_other_project_blob_hashes(&code_sources, project_id);

        for hash in &project_blobs {
            if !other_referenced.contains(hash) {
                let prefix = &hash[..2];
                let blob_path = blob_dir.join(prefix).join(hash);
                let _ = std::fs::remove_file(&blob_path);
            }
        }

        Ok(())
    }

    /// Re-inventory cross-store reference classes from current state after
    /// all discharge stages. Re-runs the existing probe machinery against
    /// live stores (section 11.3 step 7).
    fn reprobe_evidence(
        &mut self,
        project_id: &ProjectId,
        _original_evidence: &project_catalog_admin::RetireEvidence,
    ) -> project_catalog_admin::AdminResult<project_catalog_admin::RetireEvidence> {
        let (_lock, store) = open_admin_store(&self.projects_path.to_path_buf()).map_err(|e| {
            project_catalog_admin::admin_error(
                "error.project_catalog_cli_reprobe_store_open",
                format!("{}: {}", e.code, e.message),
            )
        })?;
        let probe = probe_retire_evidence(self.config, self.projects_path, &store, project_id)
            .map_err(|e| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_cli_reprobe_failed",
                    format!("{}: {}", e.code, e.message),
                )
            })?;
        Ok(probe.evidence)
    }
}

/// Every external-reference class the offline retire probe covers (plan
/// §7.8). These names are operator-facing vocabulary: a refusal names the
/// exact classes that still hold references, and the response repeats the
/// complete list so a class that was never probed cannot be mistaken for a
/// discharged one. Attachment rows are inventoried by the domain layer and
/// are deliberately absent here.
///
/// Two bro-owned stores are deliberately absent as well. Bro tasks
/// (`bro/tasks.json`) and Badgey proposals (`bro/badgey/proposals`) are
/// dispatch execution state keyed by an execution path, the §8.3
/// execution-target class, not owners of logical project identity: a retired
/// project's finished dispatch record references where work ran, and holding
/// retirement on it would make the class undischargeable without deleting
/// audit history. Slack rows are the opposite case and are included: both
/// slack stores key their rows to a project by id and by project directory,
/// so they are logical-identity references like any other coordination row.
const RETIRE_REFERENCE_CLASSES: [&str; 20] = [
    "code_source_activation",
    "code_source_generations",
    "producer_assignments",
    "accepted_publication_pointer",
    "knowledge_rows",
    "gap_rows",
    "thread_rows",
    "note_rows",
    "pin_rows",
    "roadmap_rows",
    "artifact_rows",
    "whiteboard_rows",
    "packet_rows",
    "slack_channel_bindings",
    "slack_proposal_links",
    "edge_sidecar_rows",
    "index_entity_refs",
    "index_code_metadata_rows",
    "git_ingest_cursors",
    "vector_entity_refs",
];

/// JSON keys naming a project in the shared coordination stores.
const PROJECT_ROW_KEYS: [&str; 2] = ["project", "project_id"];

/// The slack stores additionally key each row by the legacy project
/// directory, and their owner-snapshot capture surfaces expose only that
/// directory selector, so both stores are read with the wider key set.
const SLACK_ROW_KEYS: [&str; 3] = ["project", "project_id", "project_dir"];

/// Outcome of one reference-class probe. A class the probe could not read is
/// never folded into a count: `Unprobeable` keeps it distinguishable from a
/// genuinely discharged zero.
enum ClassProbe {
    Counted(u64),
    Unprobeable,
}

/// Bounded external-reference evidence plus the classes this host could not
/// answer for.
#[derive(Default)]
struct RetireProbe {
    evidence: project_catalog_admin::RetireEvidence,
    unprobeable: Vec<String>,
}

impl RetireProbe {
    fn record(&mut self, class: &str, probe: ClassProbe) {
        match probe {
            ClassProbe::Counted(0) => {}
            ClassProbe::Counted(count) => {
                self.evidence
                    .external_reference_counts
                    .insert(class.to_string(), count);
            }
            ClassProbe::Unprobeable => self.unprobeable.push(class.to_string()),
        }
    }
}

/// Bounded external-reference probing for retire (plan §7.8), covering every
/// class in [`RETIRE_REFERENCE_CLASSES`]: code-source activation, retained
/// generations across every scope the project has ever owned, producer
/// assignments from the effective source manifest, the accepted-publication
/// pointer, project-selector rows in the shared coordination stores,
/// artifacts, the project's edge sidecar lane, and the entity refs the corpus
/// index and vector partitions still carry. Rows are matched by exact project
/// id or any of the project's recorded attachment directories.
///
/// Every filesystem probe reads no-follow metadata: a symlink standing where
/// a store-owned record belongs answers for state this project does not own,
/// so it is reported as unprobeable rather than as presence or absence. The
/// index and vector owners are read through their no-create Phase 1 capture
/// surfaces, which never open, repair, or create either store.
fn probe_retire_evidence(
    config: &config::Config,
    projects_path: &Path,
    store: &ProjectCatalogStore,
    project_id: &ProjectId,
) -> Result<RetireProbe, CommandFailure> {
    let mut probe = RetireProbe::default();
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
    match bbox_code_source_store::CodeSourceStorePaths::new(&code_sources) {
        Ok(paths) => {
            let activation = paths.activation(project_id);
            probe.record("code_source_activation", presence_probe(&activation));
            let assignments = probe_producer_assignments(&paths.anchor(), project_id);
            probe.record("producer_assignments", assignments.class);
            // Generations are scope-keyed, and a migrated project keeps them
            // under every scope hash it has owned, so the walk covers each
            // scope directory in the store rather than the current hash
            // alone. An activation record or manifest naming an exact
            // generation id attributes it whichever scope directory holds it.
            let named_generations = match read_json_field(&activation, "generation_id") {
                Ok(active) => {
                    let mut named = assignments.generations;
                    named.extend(active);
                    Some(named)
                }
                Err(_) => None,
            };
            probe.record(
                "code_source_generations",
                match named_generations {
                    Some(named) => probe_code_source_generations(
                        &code_sources,
                        &owned_scope_hashes(&state, project_id),
                        &named,
                    ),
                    None => ClassProbe::Unprobeable,
                },
            );
        }
        Err(_) => {
            for class in [
                "code_source_activation",
                "producer_assignments",
                "code_source_generations",
            ] {
                probe.record(class, ClassProbe::Unprobeable);
            }
        }
    }

    probe.record(
        "accepted_publication_pointer",
        match accepted_publication_pointer(projects_path, project_id) {
            Some(pointer) => presence_probe(&pointer),
            None => ClassProbe::Unprobeable,
        },
    );

    for (class, path) in [
        ("knowledge_rows", &config.paths.knowledge_path),
        ("gap_rows", &config.paths.gaps_path),
        ("thread_rows", &config.paths.threads_path),
        ("note_rows", &config.paths.notes_path),
        ("pin_rows", &config.paths.pins_path),
        ("roadmap_rows", &config.paths.roadmap_path),
    ] {
        probe.record(
            class,
            count_project_rows(path, &selectors, &PROJECT_ROW_KEYS),
        );
    }
    for (class, path) in [
        (
            "slack_channel_bindings",
            config.paths.bro_home.join("slack-channel-bindings.json"),
        ),
        (
            "slack_proposal_links",
            config.paths.bro_home.join("slack-proposal-links.json"),
        ),
    ] {
        probe.record(
            class,
            count_project_rows(&path, &selectors, &SLACK_ROW_KEYS),
        );
    }

    // Tree-shaped coordination owners, read through their Phase 1 no-create
    // capture surfaces. Each returns rows keyed either by project id or by a
    // legacy project selector, matched against the same selector set.
    let owner_limits = bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1::default();
    for (class, snapshot) in [
        (
            "artifact_rows",
            bbox_artifacts::artifacts::capture_project_catalog_owner_snapshot(
                &config.paths.artifacts_dir,
                owner_limits,
            ),
        ),
        (
            "whiteboard_rows",
            bbox_whiteboards::whiteboards::capture_project_catalog_owner_snapshot(
                &config.paths.bro_home.join("whiteboards"),
                owner_limits,
            ),
        ),
        (
            "packet_rows",
            bbox_packets::capture_project_catalog_owner_snapshot(
                &config.paths.packets_dir,
                owner_limits,
            ),
        ),
    ] {
        probe.record(class, probe_owner_snapshot_rows(snapshot, &selectors));
    }

    probe.record(
        "edge_sidecar_rows",
        probe_edge_sidecar(&config.paths.state_dir.join("edges"), project_id),
    );

    // Both derived corpora are read through their no-create Phase 1 capture
    // surfaces: neither call opens, repairs, resets, or creates a store.
    let corpus = corpus_inventory::capture_owner_migration_snapshot_no_create(
        &config.paths.index_path,
        &config.paths.state_dir.join("git_meta"),
        Default::default(),
    );
    probe.record(
        "index_entity_refs",
        probe_index_entity_refs(&corpus.index, &selectors),
    );
    probe.record(
        "index_code_metadata_rows",
        probe_index_code_metadata_rows(&corpus.code_metadata, &selectors),
    );
    probe.record(
        "git_ingest_cursors",
        probe_git_ingest_cursors(&corpus.git_cursors, &selectors),
    );

    let vectors = vector_inventory::capture_migration_snapshot_no_create(
        &config.paths.state_dir.join("vectors"),
        Default::default(),
    );
    probe.record(
        "vector_entity_refs",
        probe_vector_entity_refs(&vectors, &selectors),
    );

    Ok(probe)
}

/// Every code-source scope hash this project has owned: the scope the catalog
/// records now plus both endpoints of each of its own migration records.
fn owned_scope_hashes(
    state: &bbox_indexing::project_catalog_store::ProjectCatalogState,
    project_id: &ProjectId,
) -> BTreeSet<String> {
    let mut hashes = BTreeSet::new();
    let add = |scope: &ProjectScope, hashes: &mut BTreeSet<String>| {
        if let ProjectScope::Published(scope) = scope {
            hashes.insert(bbox_code_source::scope_hash(scope));
        }
    };
    if let Some(project) = state.catalog().projects.get(project_id) {
        add(&project.scope, &mut hashes);
    }
    for record in state.catalog().scope_migrations.values() {
        if &record.project_id != project_id {
            continue;
        }
        add(&record.old_scope, &mut hashes);
        add(&record.new_scope, &mut hashes);
    }
    hashes
}

/// Producer assignments for the project, read from the effective source
/// manifest anchor. The manifest is also the only offline evidence naming the
/// project's generation ids across scope directories.
struct ProducerAssignments {
    class: ClassProbe,
    generations: BTreeSet<String>,
}

fn probe_producer_assignments(anchor: &Path, project_id: &ProjectId) -> ProducerAssignments {
    let bytes = match read_regular_nofollow(anchor) {
        Ok(None) => {
            return ProducerAssignments {
                class: ClassProbe::Counted(0),
                generations: BTreeSet::new(),
            };
        }
        Ok(Some(bytes)) => bytes,
        Err(()) => {
            return ProducerAssignments {
                class: ClassProbe::Unprobeable,
                generations: BTreeSet::new(),
            };
        }
    };
    let decoded = bbox_code_source_store::decode_migration_effective_source_manifest_v1(&bytes);
    let Ok(manifest) = decoded else {
        return ProducerAssignments {
            class: ClassProbe::Unprobeable,
            generations: BTreeSet::new(),
        };
    };
    let mut generations = BTreeSet::new();
    let mut count = 0_u64;
    for selection in &manifest.selections {
        if &selection.project_id == project_id {
            count += 1;
            generations.insert(selection.generation_id.clone());
        }
    }
    ProducerAssignments {
        class: ClassProbe::Counted(count),
        generations,
    }
}

/// Retained generations attributable to the project across every scope
/// directory present in the store.
fn probe_code_source_generations(
    code_sources: &Path,
    owned_scopes: &BTreeSet<String>,
    named_generations: &BTreeSet<String>,
) -> ClassProbe {
    let scopes_root = code_sources.join("scopes");
    match std::fs::symlink_metadata(&scopes_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ClassProbe::Counted(0);
        }
        Ok(metadata) if metadata.is_dir() => {}
        _ => return ClassProbe::Unprobeable,
    }
    let Ok(scopes) = std::fs::read_dir(&scopes_root) else {
        return ClassProbe::Unprobeable;
    };
    let mut count = 0_u64;
    for scope in scopes {
        let Ok(scope) = scope else {
            return ClassProbe::Unprobeable;
        };
        let Ok(kind) = scope.file_type() else {
            return ClassProbe::Unprobeable;
        };
        if kind.is_symlink() {
            return ClassProbe::Unprobeable;
        }
        if !kind.is_dir() {
            continue;
        }
        let owned = scope
            .file_name()
            .to_str()
            .is_some_and(|hash| owned_scopes.contains(hash));
        let generations = match std::fs::read_dir(scope.path().join("generations")) {
            Ok(generations) => generations,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return ClassProbe::Unprobeable,
        };
        for generation in generations {
            let Ok(generation) = generation else {
                return ClassProbe::Unprobeable;
            };
            let Ok(kind) = generation.file_type() else {
                return ClassProbe::Unprobeable;
            };
            if kind.is_symlink() {
                return ClassProbe::Unprobeable;
            }
            if !kind.is_dir() {
                continue;
            }
            let named = generation
                .file_name()
                .to_str()
                .is_some_and(|id| named_generations.contains(id));
            if owned || named {
                count += 1;
            }
        }
    }
    ClassProbe::Counted(count)
}

/// Edge rows the project still owns in its own sidecar lane. The sidecar is
/// one JSONL file per project id, so presence and line count answer the class
/// exactly.
fn probe_edge_sidecar(edges_dir: &Path, project_id: &ProjectId) -> ClassProbe {
    let bytes = match read_regular_nofollow(&edges_dir.join(format!("{project_id}.jsonl"))) {
        Ok(None) => return ClassProbe::Counted(0),
        Ok(Some(bytes)) => bytes,
        Err(()) => return ClassProbe::Unprobeable,
    };
    let Ok(body) = String::from_utf8(bytes) else {
        return ClassProbe::Unprobeable;
    };
    ClassProbe::Counted(body.lines().filter(|line| !line.trim().is_empty()).count() as u64)
}

/// Rows naming the project in one Phase 1 owner snapshot. The capture
/// surfaces never open or create their store, so a missing owner root with no
/// rows is a discharged zero; any other non-present state is unprobeable.
fn probe_owner_snapshot_rows(
    snapshot: Result<
        bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
    >,
    selectors: &[String],
) -> ClassProbe {
    use bbox_corpus_core::project_catalog_snapshot::{
        OwnerSnapshotRowValueV1, OwnerSnapshotStateV1,
    };

    let Ok(snapshot) = snapshot else {
        return ClassProbe::Unprobeable;
    };
    match snapshot.state {
        OwnerSnapshotStateV1::Missing { .. } if snapshot.rows.is_empty() => {
            return ClassProbe::Counted(0);
        }
        OwnerSnapshotStateV1::Present { .. } => {}
        _ => return ClassProbe::Unprobeable,
    }
    ClassProbe::Counted(
        snapshot
            .rows
            .iter()
            .filter(|row| match &row.value {
                OwnerSnapshotRowValueV1::InventoryTarget { project_id, .. } => {
                    selectors.iter().any(|selector| selector == project_id)
                }
                OwnerSnapshotRowValueV1::LegacyProjectSelector {
                    literal_selector, ..
                } => selectors
                    .iter()
                    .any(|selector| selector == literal_selector),
            })
            .count() as u64,
    )
}

/// Committed corpus-index documents still exposing an entity ref for the
/// project. Multiplicity is retained: two documents naming the same ref are
/// two references to discharge.
fn probe_index_entity_refs(
    index: &corpus_inventory::CorpusIndexMigrationSnapshotV1,
    selectors: &[String],
) -> ClassProbe {
    use corpus_inventory::CorpusMigrationSourceStateV1;

    match index.state {
        CorpusMigrationSourceStateV1::Missing => ClassProbe::Counted(0),
        CorpusMigrationSourceStateV1::Present => ClassProbe::Counted(
            index
                .project_scoped_refs
                .iter()
                .filter(|row| selectors.iter().any(|selector| selector == &row.project_id))
                .map(|row| row.document_count)
                .sum(),
        ),
        _ => ClassProbe::Unprobeable,
    }
}

/// Code-index metadata rows keyed to the project by id or legacy selector.
fn probe_index_code_metadata_rows(
    metadata: &corpus_inventory::CodeIndexMetadataMigrationSnapshotV1,
    selectors: &[String],
) -> ClassProbe {
    use corpus_inventory::CorpusMigrationSourceStateV1;

    match metadata.state {
        CorpusMigrationSourceStateV1::Missing => ClassProbe::Counted(0),
        CorpusMigrationSourceStateV1::Present => ClassProbe::Counted(
            metadata
                .rows
                .iter()
                .filter(|row| {
                    let named = |value: &Option<String>| {
                        value
                            .as_deref()
                            .is_some_and(|value| selectors.iter().any(|s| s == value))
                    };
                    named(&row.project_id) || named(&row.selector)
                })
                .count() as u64,
        ),
        _ => ClassProbe::Unprobeable,
    }
}

/// Legacy Git ingest cursors still recorded for the project.
fn probe_git_ingest_cursors(
    cursors: &corpus_inventory::GitCursorMigrationSnapshotV1,
    selectors: &[String],
) -> ClassProbe {
    use corpus_inventory::CorpusMigrationSourceStateV1;

    match cursors.state {
        CorpusMigrationSourceStateV1::Missing => ClassProbe::Counted(0),
        CorpusMigrationSourceStateV1::Present => ClassProbe::Counted(
            cursors
                .rows
                .iter()
                .filter(|row| selectors.iter().any(|selector| selector == &row.project_id))
                .count() as u64,
        ),
        _ => ClassProbe::Unprobeable,
    }
}

/// Project-scoped entity refs still held by the vector partitions.
fn probe_vector_entity_refs(
    snapshot: &vector_inventory::VectorMigrationSnapshotV1,
    selectors: &[String],
) -> ClassProbe {
    use vector_inventory::VectorMigrationSourceStateV1;

    match snapshot.state {
        VectorMigrationSourceStateV1::Missing => ClassProbe::Counted(0),
        VectorMigrationSourceStateV1::Present => ClassProbe::Counted(
            snapshot
                .project_scoped_refs
                .iter()
                .filter(|row| selectors.iter().any(|selector| selector == &row.project_id))
                .count() as u64,
        ),
        _ => ClassProbe::Unprobeable,
    }
}

/// Count rows in one JSON coordination store whose project-naming field
/// (`keys`) names the retiring project. A store that exists but cannot be
/// read or parsed is unprobeable, never zero.
fn count_project_rows(path: &Path, selectors: &[String], keys: &[&str]) -> ClassProbe {
    let bytes = match read_regular_nofollow(path) {
        Ok(None) => return ClassProbe::Counted(0),
        Ok(Some(bytes)) => bytes,
        Err(()) => return ClassProbe::Unprobeable,
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return ClassProbe::Unprobeable;
    };
    let mut count = 0_u64;
    let mut stack = vec![&value];
    while let Some(node) = stack.pop() {
        match node {
            serde_json::Value::Array(items) => stack.extend(items.iter()),
            serde_json::Value::Object(map) => {
                let hit = keys.iter().any(|key| {
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
    ClassProbe::Counted(count)
}

/// No-follow presence test for one store-owned record. `Path::exists`
/// follows symlinks, which would let a link into unrelated state answer an
/// evidence question about this project's own store.
fn presence_probe(path: &Path) -> ClassProbe {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ClassProbe::Counted(0),
        Ok(metadata) if metadata.is_file() => ClassProbe::Counted(1),
        _ => ClassProbe::Unprobeable,
    }
}

/// Read one small store-owned record without following a symlink at the
/// leaf. An absent record reads as `Ok(None)`; anything present that cannot
/// be read as a regular file is an error, never an empty read.
fn read_regular_nofollow(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(metadata) if metadata.is_file() => std::fs::read(path).map(Some).map_err(|_| ()),
        _ => Err(()),
    }
}

/// Read one JSON field from a small store-owned record. Returns `Ok(None)`
/// when the record is absent, and an error when it exists but cannot be read
/// or does not carry the field: a bridge generation is never invented.
fn read_json_field(path: &Path, field: &str) -> Result<Option<String>, CommandFailure> {
    let unreadable = || {
        CommandFailure::new(
            "error.project_catalog_cli_bridge_generation",
            format!("{} carries no readable {field}", path.display()),
        )
    };
    let Some(bytes) = read_regular_nofollow(path).map_err(|()| unreadable())? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| unreadable())?;
    match value.get(field).and_then(|found| found.as_str()) {
        Some(found) => Ok(Some(found.to_string())),
        None => Err(unreadable()),
    }
}

/// Active collected generation for the project, read from the code-source
/// activation record (plan §7.5).
fn code_bridge_generation(
    state_dir: &Path,
    project_id: &ProjectId,
) -> Result<Option<String>, CommandFailure> {
    let root = state_dir.join("code-sources");
    let Ok(paths) = bbox_code_source_store::CodeSourceStorePaths::new(&root) else {
        return Ok(None);
    };
    read_json_field(&paths.activation(project_id), "generation_id")
}

/// The pointer record that would carry an accepted publication for this
/// project, derived from the administered projects path.
fn accepted_publication_pointer(projects_path: &Path, project_id: &ProjectId) -> Option<PathBuf> {
    projects_path.parent().map(|parent| {
        parent
            .join("accepted-publications")
            .join("pointers")
            .join(format!("{project_id}.json"))
    })
}

// ---- Discharge worker helpers (section 11.3) ----

/// Enumerate scope directory names in the code-source store. Each
/// directory under `scopes/` is a scope hash.
fn scope_dirs(code_sources: &Path) -> Vec<String> {
    let scopes_dir = code_sources.join("scopes");
    match std::fs::read_dir(&scopes_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .filter(|s| !s.starts_with('.'))
                    .map(|s| s.to_string())
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Delete generation records owned by `project_id` in one scope
/// directory. Generation records are JSON files under
/// `scopes/{scope_hash}/generations/`. Each contains a `producer_id`
/// field that identifies the owning project. This is a single-attempt
/// operation: if the directory or a record is absent, it is skipped.
fn delete_generations_for_project_in_scope(
    code_sources: &Path,
    scope_hash: &str,
    project_id: &str,
) -> std::io::Result<()> {
    let gen_dir = code_sources
        .join("scopes")
        .join(scope_hash)
        .join("generations");
    let entries = match std::fs::read_dir(&gen_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if json.get("producer_id").and_then(|v| v.as_str()) == Some(project_id) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    Ok(())
}

/// Collect coordination row store paths and their key fields. Each tuple
/// is `(path, [key_fields])` where `key_fields` are the JSON field names
/// that carry a project id or selector. The discharge worker reads each
/// file and removes rows matching the retired project.
fn coordination_row_paths(config: &config::Config) -> Vec<(PathBuf, &'static [&'static str])> {
    vec![
        (config.paths.knowledge_path.clone(), &PROJECT_ROW_KEYS[..]),
        (config.paths.gaps_path.clone(), &PROJECT_ROW_KEYS[..]),
        (config.paths.threads_path.clone(), &PROJECT_ROW_KEYS[..]),
        (config.paths.notes_path.clone(), &PROJECT_ROW_KEYS[..]),
        (config.paths.pins_path.clone(), &PROJECT_ROW_KEYS[..]),
        (config.paths.roadmap_path.clone(), &PROJECT_ROW_KEYS[..]),
    ]
}

/// Clear project rows from a JSON-lines or JSON-array file. Reads the
/// file, filters out rows matching the project id in any of the key
/// fields, and writes the remaining rows back. If the file does not
/// exist, this is a no-op.
fn clear_project_rows(path: &Path, keys: &[&str], project_id: &str) -> std::io::Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // If the file is a JSON array of objects, filter it.
    if let Ok(arr) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
        let original_len = arr.len();
        let filtered: Vec<_> = arr
            .into_iter()
            .filter(|row| {
                !keys.iter().any(|key| {
                    row.get(*key)
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s == project_id)
                })
            })
            .collect();
        if filtered.len() < original_len {
            std::fs::write(path, serde_json::to_vec(&filtered).unwrap_or_default())?;
        }
    }
    Ok(())
}

/// Collect blob hashes from the retired project's generation records.
/// These are the only candidates for deletion in the blob sweep.
fn collect_project_blob_hashes(code_sources: &Path, project_id: &ProjectId) -> BTreeSet<String> {
    let mut hashes = BTreeSet::new();
    for scope_hash in scope_dirs(code_sources) {
        let gen_dir = code_sources
            .join("scopes")
            .join(&scope_hash)
            .join("generations");
        let Ok(entries) = std::fs::read_dir(&gen_dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if json.get("producer_id").and_then(|v| v.as_str()) == Some(project_id.as_str()) {
                if let Some(entries) = json.get("manifest_entries").and_then(|v| v.as_array()) {
                    for entry in entries {
                        if let Some(hash) = entry.get("content_sha256").and_then(|v| v.as_str()) {
                            hashes.insert(hash.to_string());
                        }
                    }
                }
            }
        }
    }
    hashes
}

/// Collect blob hashes referenced by projects OTHER than the retired
/// project. These blobs must be preserved.
fn collect_other_project_blob_hashes(
    code_sources: &Path,
    retired_project_id: &ProjectId,
) -> BTreeSet<String> {
    let mut hashes = BTreeSet::new();
    for scope_hash in scope_dirs(code_sources) {
        let gen_dir = code_sources
            .join("scopes")
            .join(&scope_hash)
            .join("generations");
        let Ok(entries) = std::fs::read_dir(&gen_dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let is_retired = json
                .get("producer_id")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == retired_project_id.as_str());
            if !is_retired {
                if let Some(entries) = json.get("manifest_entries").and_then(|v| v.as_array()) {
                    for entry in entries {
                        if let Some(hash) = entry.get("content_sha256").and_then(|v| v.as_str()) {
                            hashes.insert(hash.to_string());
                        }
                    }
                }
            }
        }
    }
    hashes
}

/// Accepted publication generation for the project, read from the pointer
/// (plan §7.5).
fn publication_bridge_generation(
    projects_path: &Path,
    project_id: &ProjectId,
) -> Result<Option<String>, CommandFailure> {
    let Some(pointer) = accepted_publication_pointer(projects_path, project_id) else {
        return Ok(None);
    };
    read_json_field(&pointer, "accepted_generation")
}
