use std::collections::{BTreeMap, BTreeSet};
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
    /// Prepared plan hash emitted by a prior dry-run.
    #[arg(long, value_name = "SHA256", requires = "execute")]
    plan_hash: Option<String>,
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
    fn durable_publication_delete_is_idempotent_across_resume() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let pointers = root.join("accepted-publications").join("pointers");
        std::fs::create_dir_all(&pointers).unwrap();
        let pointer = pointers.join("project-a.json");
        std::fs::write(&pointer, b"{}").unwrap();

        durable_remove_file_if_exists(&pointer).unwrap();
        assert!(!pointer.exists());
        durable_remove_file_if_exists(&pointer).unwrap();
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

    #[test]
    fn retirement_ignores_historical_scope_after_new_owner_claims_it() {
        let project_one = ProjectId::parse("project-one").unwrap();
        let scope_a = PublishedScope::try_new("scope-a", ".").unwrap();
        let scope_b = PublishedScope::try_new("scope-b", ".").unwrap();

        assert!(
            !retirement_generation_is_owned(
                Some(&scope_b),
                &project_one,
                &scope_a,
                &"a".repeat(64),
                &BTreeSet::from(["project-two".to_string()]),
            )
            .unwrap()
        );
    }

    #[test]
    fn retirement_refuses_ownerless_generation_in_current_scope() {
        let project_two = ProjectId::parse("project-two").unwrap();
        let scope_a = PublishedScope::try_new("scope-a", ".").unwrap();
        let error = retirement_generation_is_owned(
            Some(&scope_a),
            &project_two,
            &scope_a,
            &"b".repeat(64),
            &BTreeSet::new(),
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_retire_ownerless_generation"
        );
    }

    #[test]
    fn retiring_new_scope_owner_preserves_prior_owners_retained_generation() {
        let project_two = ProjectId::parse("project-two").unwrap();
        let scope_a = PublishedScope::try_new("scope-a", ".").unwrap();
        assert!(
            !retirement_generation_is_owned(
                Some(&scope_a),
                &project_two,
                &scope_a,
                &"c".repeat(64),
                &BTreeSet::from(["project-one".to_string()]),
            )
            .unwrap()
        );
    }

    #[test]
    fn retirement_clears_every_dischargeable_coordination_store_shape() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = "project-retiring".to_string();
        let selectors = vec![project.clone(), "/repo/shared".to_string()];
        for (name, field) in [
            ("knowledge.json", "entries"),
            ("gaps.json", "gaps"),
            ("threads.json", "threads"),
            ("notes.json", "notes"),
            ("pins.json", "pins"),
        ] {
            let path = root.join(name);
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "version": 1,
                    (field): [
                        {"project_id": project, "id": "remove"},
                        {
                            "project_id": "project-retained",
                            "project": "/repo/shared",
                            "id": "keep"
                        }
                    ]
                }))
                .unwrap(),
            )
            .unwrap();
            clear_wrapped_project_rows(&path, &[field], &PROJECT_ROW_KEYS, &project, &selectors)
                .unwrap();
            assert!(matches!(
                count_project_rows(&path, &project, &selectors, &PROJECT_ROW_KEYS),
                ClassProbe::Counted(0)
            ));
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(value[field][0]["id"], "keep");
        }

        let roadmap = root.join("roadmap.json");
        std::fs::write(
            &roadmap,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "items": [
                    {"project_id": project},
                    {"project_id": "project-retained", "project": "/repo/shared"}
                ],
                "edges": [
                    {"project_id": project},
                    {"project_id": "project-retained", "project": "/repo/shared"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        clear_wrapped_project_rows(
            &roadmap,
            &["items", "edges"],
            &PROJECT_ROW_KEYS,
            &project,
            &selectors,
        )
        .unwrap();
        assert!(matches!(
            count_project_rows(&roadmap, &project, &selectors, &PROJECT_ROW_KEYS),
            ClassProbe::Counted(0)
        ));

        let slack = root.join("slack-channel-bindings.json");
        std::fs::write(
            &slack,
            serde_json::to_vec(&serde_json::json!({
                "bindings": {
                    "remove": {"project_id": project},
                    "keep": {
                        "project_id": "project-retained",
                        "project_dir": "/repo/shared"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        clear_slack_channel_bindings(&slack, &project, &selectors).unwrap();
        assert!(matches!(
            count_project_rows(&slack, &project, &selectors, &SLACK_ROW_KEYS),
            ClassProbe::Counted(0)
        ));
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
    config: &config::Config,
    project_id: &bbox_corpus_core::project_catalog::ProjectId,
) -> Result<project_catalog_admin::ScopeBridgeClearEvidence, CommandFailure> {
    // R2F4: resolve the code-source store path from the supplied config,
    // not from a hardcoded sibling directory derivation.
    let code_source_dir = config.paths.state_dir.join("code-sources");
    let root_metadata = match std::fs::symlink_metadata(&code_source_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(project_catalog_admin::ScopeBridgeClearEvidence {
                activation: project_catalog_admin::ScopeBridgeActivationEvidence::Unavailable {
                    diagnostic: "code-source store root is missing".to_string(),
                },
                retained_generations:
                    project_catalog_admin::ScopeBridgeRetainedEvidence::Unavailable {
                        diagnostic: "code-source store root is missing".to_string(),
                    },
            });
        }
        Err(error) => {
            return Ok(project_catalog_admin::ScopeBridgeClearEvidence {
                activation: project_catalog_admin::ScopeBridgeActivationEvidence::Unavailable {
                    diagnostic: format!("code-source store root cannot be inspected: {error}"),
                },
                retained_generations:
                    project_catalog_admin::ScopeBridgeRetainedEvidence::Unavailable {
                        diagnostic: format!("code-source store root cannot be inspected: {error}"),
                    },
            });
        }
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Ok(project_catalog_admin::ScopeBridgeClearEvidence {
            activation: project_catalog_admin::ScopeBridgeActivationEvidence::Unavailable {
                diagnostic: "code-source store root is unavailable".to_string(),
            },
            retained_generations: project_catalog_admin::ScopeBridgeRetainedEvidence::Unavailable {
                diagnostic: "code-source store root is unavailable".to_string(),
            },
        });
    }
    let code_store = bbox_code_source_store::CodeSourceStore::open_existing_for_migration(
        &code_source_dir,
        bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits(config),
    )
    .map_err(|e| {
        CommandFailure::new(
            "error.project_catalog_cli_code_source_open",
            format!(
                "failed to open code-source store at {}: {e}",
                code_source_dir.display()
            ),
        )
    })?
    .ok_or_else(|| {
        CommandFailure::new(
            "error.project_catalog_cli_bridge_clear_evidence",
            "code-source store disappeared during evidence capture",
        )
    })?;
    // Load the activation record for effective generation id and scope.
    let activation = match code_store.load_activation_mixed(project_id.as_str()) {
        Ok(Some(activation)) => match activation.published_scope().cloned() {
            Some(scope) => project_catalog_admin::ScopeBridgeActivationEvidence::Present {
                generation_id: activation.generation_id().to_string(),
                scope,
            },
            None => project_catalog_admin::ScopeBridgeActivationEvidence::Unavailable {
                diagnostic: "activation record has no published scope".to_string(),
            },
        },
        Ok(None) => project_catalog_admin::ScopeBridgeActivationEvidence::VerifiedAbsent,
        Err(error) => project_catalog_admin::ScopeBridgeActivationEvidence::Unavailable {
            diagnostic: format!("activation load failed: {error}"),
        },
    };
    let retained_generations = match code_store.retirement_generation_inventory() {
        Ok(generations) => project_catalog_admin::ScopeBridgeRetainedEvidence::Enumerated(
            generations
                .into_iter()
                .map(|generation| generation.generation_id)
                .collect(),
        ),
        Err(error) => project_catalog_admin::ScopeBridgeRetainedEvidence::Unavailable {
            diagnostic: format!("retained-generation enumeration failed: {error}"),
        },
    };
    Ok(project_catalog_admin::ScopeBridgeClearEvidence {
        activation,
        retained_generations,
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
    let config = load_config(args.config.clone())?;
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
    let evidence = probe_bridge_clear_evidence(&config, &project_id)?;
    let commit =
        project_catalog_admin::clear_scope_bridge(&store, epoch, &project_id, mode, &evidence)?;
    Ok(serde_json::json!({
        "project_id": project_id.as_str(),
        "mode": match mode {
            project_catalog_admin::ScopeBridgeClearMode::DanglingReference => "dangling_reference",
            project_catalog_admin::ScopeBridgeClearMode::DoubleMigrationRepair => {
                "double_migration_repair"
            }
            project_catalog_admin::ScopeBridgeClearMode::AutomaticFirstNewScope => {
                unreachable!("automatic bridge clear is daemon-only")
            }
        },
        "epoch": commit.epoch,
    }))
}

fn execute_retire(args: RetireArgs) -> Result<serde_json::Value, CommandFailure> {
    let config = load_config(args.config.clone())?;
    let (_lock, store) = open_admin_store(&args.store.projects_path)?;
    let project_id = parse_project_id(&args.project)?;
    let probe = probe_retire_evidence(
        &config,
        &args.store.projects_path,
        &store,
        &project_id,
        None,
    )?;
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
    let probe = probe_retire_evidence(
        &config,
        &args.store.projects_path,
        &store,
        &project_id,
        None,
    )?;

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

    let load_journal = |archived: bool| {
        if archived {
            project_catalog_admin::load_archived_retirement_journal(&bro_home, &project_id)
        } else {
            project_catalog_admin::load_retirement_journal(&bro_home, &project_id)
        }
        .map_err(|error| {
            CommandFailure::new("error.project_catalog_retire_journal_io", error.to_string())
        })
    };
    let planned_evidence = if let Some(journal) = load_journal(false)? {
        journal.evidence
    } else if let Some(journal) = load_journal(true)? {
        journal.evidence
    } else {
        capture_retirement_evidence(&config, &args.store.projects_path, &project_id)?
    };
    let plan_hash = project_catalog_admin::retirement_evidence_sha256(&planned_evidence);
    if args.execute {
        let supplied = args.plan_hash.as_deref().ok_or_else(|| {
            CommandFailure::new(
                "error.project_catalog_retire_plan_hash_required",
                "execute requires --plan-hash from a current dry-run",
            )
        })?;
        if supplied != plan_hash {
            return Err(CommandFailure::new(
                "error.project_catalog_retire_plan_drift",
                format!(
                    "retirement plan changed: expected {supplied}, current plan is {plan_hash}"
                ),
            ));
        }
    }

    let mut workers = CliRetirementDischargeWorkers::new(
        &config,
        &args.store.projects_path,
        args.plan_hash.clone(),
    );
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
        "plan_hash": plan_hash,
        "plan": {
            "project_selectors": planned_evidence.project_selectors,
            "owned_generations": planned_evidence.owned_generations,
            "desired_pointers": planned_evidence.desired_pointers,
            "owned_uploads": planned_evidence.owned_uploads,
            "edge_paths": planned_evidence.edge_paths,
            "artifact_targets": planned_evidence.artifact_targets,
            "reference_class_counts": planned_evidence.reference_class_counts,
            "reference_class_commitments": planned_evidence.reference_class_commitments,
            "blob_count": planned_evidence.owned_blob_hashes.len(),
        },
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
    expected_plan_hash: Option<String>,
}

impl<'a> CliRetirementDischargeWorkers<'a> {
    fn new(
        config: &'a config::Config,
        projects_path: &'a Path,
        expected_plan_hash: Option<String>,
    ) -> Self {
        Self {
            config,
            projects_path,
            expected_plan_hash,
        }
    }
}

impl<'a> project_catalog_admin::RetirementDischargeWorkers for CliRetirementDischargeWorkers<'a> {
    fn capture_retirement_evidence(
        &mut self,
        project_id: &ProjectId,
    ) -> project_catalog_admin::AdminResult<project_catalog_admin::RetirementJournalEvidence> {
        let evidence = capture_retirement_evidence(self.config, self.projects_path, project_id)?;
        if let Some(expected) = &self.expected_plan_hash {
            let actual = project_catalog_admin::retirement_evidence_sha256(&evidence);
            if &actual != expected {
                return Err(project_catalog_admin::admin_error(
                    "error.project_catalog_retire_plan_drift",
                    format!(
                        "retirement plan changed after preflight: expected {expected}, current {actual}"
                    ),
                ));
            }
        }
        Ok(evidence)
    }

    fn validate_retirement_evidence(
        &mut self,
        _store: &ProjectCatalogStore,
        project_id: &ProjectId,
        evidence: &project_catalog_admin::RetirementJournalEvidence,
        stage: project_catalog_admin::RetirementJournalStage,
    ) -> project_catalog_admin::AdminResult<()> {
        reconcile_completed_retained_owner_deletions(self.config, project_id, evidence)?;
        if stage.is_at_least(project_catalog_admin::RetirementJournalStage::CatalogPairRemoved) {
            if _store
                .snapshot()?
                .catalog()
                .projects
                .contains_key(project_id)
            {
                return Err(project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_drift",
                    "retirement journal claims the catalog pair was removed but the project remains",
                ));
            }
            return validate_retirement_targets_absent(self.config, evidence);
        }
        let current = capture_retirement_evidence(self.config, self.projects_path, project_id)?;
        let expected_generations = evidence
            .owned_generations
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current_generations = current
            .owned_generations
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_desired = evidence
            .desired_pointers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current_desired = current
            .desired_pointers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_uploads = evidence
            .owned_uploads
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current_uploads = current
            .owned_uploads
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_blobs = evidence
            .owned_blob_hashes
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current_blobs = current
            .owned_blob_hashes
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_edge_paths = evidence
            .edge_paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current_edge_paths = current
            .edge_paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_artifacts = evidence
            .artifact_targets
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current_artifacts = current
            .artifact_targets
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_reference_counts =
            evidence.reference_class_counts.as_ref().ok_or_else(|| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_incomplete",
                    "retirement evidence is missing reference-class counts",
                )
            })?;
        let current_reference_counts =
            current.reference_class_counts.as_ref().ok_or_else(|| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_incomplete",
                    "current retirement evidence is missing reference-class counts",
                )
            })?;
        let reference_count_increased = current_reference_counts.iter().any(|(class, count)| {
            *count > expected_reference_counts.get(class).copied().unwrap_or(0)
        });
        let expected_commitments =
            evidence.reference_class_commitments.as_ref().ok_or_else(|| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_incomplete",
                    "retirement evidence is missing reference-class commitments",
                )
            })?;
        let current_commitments =
            current.reference_class_commitments.as_ref().ok_or_else(|| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_incomplete",
                    "current retirement evidence is missing reference-class commitments",
                )
            })?;
        let reference_commitment_drifted = current_commitments.iter().any(|(class, identities)| {
            let expected = expected_commitments
                .get(class)
                .into_iter()
                .flatten()
                .collect::<BTreeSet<_>>();
            identities
                .iter()
                .any(|identity| !expected.contains(identity))
        });
        let selectors_match = stage
            .is_at_least(project_catalog_admin::RetirementJournalStage::AttachmentsDetached)
            || current.project_selectors == evidence.project_selectors;
        if evidence.owner_project_id.as_ref() != Some(project_id)
            || current.owner_project_id != evidence.owner_project_id
            || current.catalog_scope != evidence.catalog_scope
            || !selectors_match
            || !current_generations.is_subset(&expected_generations)
            || !current_desired.is_subset(&expected_desired)
            || !current_uploads.is_subset(&expected_uploads)
            || !current_edge_paths.is_subset(&expected_edge_paths)
            || !current_artifacts.is_subset(&expected_artifacts)
            || !current_blobs.is_subset(&expected_blobs)
            || reference_count_increased
            || reference_commitment_drifted
        {
            return Err(project_catalog_admin::admin_error(
                "error.project_catalog_retire_evidence_drift",
                "retirement evidence no longer matches owner-validated store state",
            ));
        }
        let code_sources = self.config.paths.state_dir.join("code-sources");
        let code_store = bbox_code_source_store::CodeSourceStore::open(
            &code_sources,
            bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits(
                self.config,
            ),
        )
        .map_err(|error| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_code_source_open",
                format!("failed to open code-source store for evidence validation: {error}"),
            )
        })?;
        let all_generations = code_store
            .retirement_generation_inventory()
            .map_err(|error| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_generations",
                    format!("failed to validate generation evidence: {error}"),
                )
            })?
            .into_iter()
            .map(
                |generation| project_catalog_admin::RetirementGenerationEvidence {
                    published_scope: generation.published_scope,
                    generation_id: generation.generation_id,
                },
            )
            .collect::<BTreeSet<_>>();
        let all_desired = code_store
            .retirement_desired_pointer_inventory()
            .map_err(|error| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_desired",
                    format!("failed to validate desired-pointer evidence: {error}"),
                )
            })?
            .into_iter()
            .map(
                |pointer| project_catalog_admin::RetirementGenerationEvidence {
                    published_scope: pointer.published_scope,
                    generation_id: pointer.generation_id,
                },
            )
            .collect::<BTreeSet<_>>();
        let all_uploads = code_store
            .retirement_upload_inventory()
            .map_err(|error| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_uploads",
                    format!("failed to validate upload evidence: {error}"),
                )
            })?
            .into_iter()
            .map(|upload| project_catalog_admin::RetirementUploadEvidence {
                producer_id: upload.producer_id,
                upload_id: upload.upload_id,
                published_scope: upload.published_scope,
            })
            .collect::<BTreeSet<_>>();
        let still_present_but_not_owner_bound = expected_generations
            .difference(&current_generations)
            .any(|generation| all_generations.contains(generation))
            || expected_desired
                .difference(&current_desired)
                .any(|pointer| all_desired.contains(pointer))
            || expected_uploads
                .difference(&current_uploads)
                .any(|upload| all_uploads.contains(upload));
        if still_present_but_not_owner_bound {
            return Err(project_catalog_admin::admin_error(
                "error.project_catalog_retire_evidence_owner",
                "persisted retirement target is present but no longer owner-bound to the retiring project",
            ));
        }
        Ok(())
    }

    /// Stage CollectedGenerationsDischarged: clear the activation record,
    /// delete source-owned generation records, and clear project-scoped
    /// coordination rows (knowledge, gaps, threads, notes, pins, etc.).
    fn discharge_collected_generations(
        &mut self,
        project_id: &ProjectId,
        evidence: &project_catalog_admin::RetirementJournalEvidence,
    ) -> project_catalog_admin::AdminResult<()> {
        let code_sources = self.config.paths.state_dir.join("code-sources");
        // R2F1: fail on store-open errors instead of silently ignoring them.
        let store = bbox_code_source_store::CodeSourceStore::open(
            &code_sources,
            bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits(
                self.config,
            ),
        )
        .map_err(|e| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_code_source_open",
                format!("failed to open code-source store: {e}"),
            )
        })?;

        for pointer in evidence.desired_pointers.as_deref().ok_or_else(|| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_evidence_incomplete",
                "retirement evidence is missing desired-pointer identities",
            )
        })? {
            store
                .delete_retirement_desired_pointer(&pointer.published_scope, &pointer.generation_id)
                .map_err(|e| {
                    project_catalog_admin::admin_error(
                        "error.project_catalog_retire_discharge_desired",
                        format!("failed to delete exact desired pointer: {e}"),
                    )
                })?;
        }

        for upload in evidence.owned_uploads.as_deref().ok_or_else(|| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_evidence_incomplete",
                "retirement evidence is missing upload identities",
            )
        })? {
            store
                .delete_retirement_upload(
                    &upload.producer_id,
                    &upload.upload_id,
                    &upload.published_scope,
                )
                .map_err(|e| {
                    project_catalog_admin::admin_error(
                        "error.project_catalog_retire_discharge_upload",
                        format!("failed to delete exact upload: {e}"),
                    )
                })?;
        }

        for generation in &evidence.owned_generations {
            store
                .delete_retirement_generation(
                    &generation.published_scope,
                    &generation.generation_id,
                )
                .map_err(|e| {
                    project_catalog_admin::admin_error(
                        "error.project_catalog_retire_discharge_generations",
                        format!("failed to delete exact generation record: {e}"),
                    )
                })?;
            store
                .delete_retained_generation_owner(
                    project_id,
                    &generation.published_scope,
                    &generation.generation_id,
                )
                .map_err(|e| {
                    project_catalog_admin::admin_error(
                        "error.project_catalog_retire_discharge_generations",
                        format!("failed to delete retained-generation ownership: {e}"),
                    )
                })?;
        }

        // Keep activation ownership available until every exact generation
        // identity is gone so a crash between deletions can be resumed.
        store.clear_activation(project_id.as_str()).map_err(|e| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_discharge_activation",
                format!("failed to clear activation record: {e}"),
            )
        })?;

        let selectors = evidence.project_selectors.as_deref().ok_or_else(|| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_evidence_incomplete",
                "retirement evidence is missing project selectors",
            )
        })?;

        for (path, array_fields, keys) in coordination_row_paths(self.config) {
            clear_wrapped_project_rows(&path, array_fields, keys, project_id.as_str(), selectors)
                .map_err(|e| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_discharge_coordination",
                    format!(
                        "failed to clear coordination rows at {}: {e}",
                        path.display()
                    ),
                )
            })?;
        }
        clear_slack_channel_bindings(
            &self
                .config
                .paths
                .bro_home
                .join("slack-channel-bindings.json"),
            project_id.as_str(),
            selectors,
        )
        .map_err(|e| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_discharge_coordination",
                format!("failed to clear Slack channel bindings: {e}"),
            )
        })?;
        let discharge_error = |class: &'static str, error: anyhow::Error| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_discharge_owner_store",
                format!("failed to discharge {class}: {error}"),
            )
        };
        bbox_slack::slack_proposal_links::SlackProposalLinks::open(&self.config.paths.bro_home)
            .and_then(|store| store.discharge_project_refs(project_id.as_str(), &selectors))
            .map_err(|error| discharge_error("slack_proposal_links", error))?;
        bbox_artifacts::artifacts::discharge_project_catalog_targets(
            &self.config.paths.artifacts_dir,
            evidence.artifact_targets.as_deref().ok_or_else(|| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_artifact_evidence",
                    "retirement evidence is missing exact artifact targets",
                )
            })?,
        )
        .map_err(|error| discharge_error("artifact_rows", error))?;
        bbox_whiteboards::whiteboards::discharge_project_catalog_rows(
            &self.config.paths.bro_home.join("whiteboards"),
            project_id.as_str(),
            &selectors,
        )
        .map_err(|error| discharge_error("whiteboard_rows", error))?;
        bbox_packets::discharge_project_catalog_rows(
            &self.config.paths.packets_dir,
            project_id.as_str(),
            &selectors,
        )
        .map_err(|error| discharge_error("packet_rows", error))?;
        let edge_inventory = bbox_edge_sidecar::migration_inventory::EdgeRetirementInventory {
            version: 1,
            project_id: project_id.as_str().to_string(),
            relative_paths: evidence.edge_paths.clone().ok_or_else(|| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_edge_evidence",
                    "retirement evidence is missing its exact edge inventory",
                )
            })?,
        };
        bbox_edge_sidecar::migration_inventory::discharge_project_retirement_inventory(
            &self.config.paths.state_dir.join("edges"),
            &edge_inventory,
        )
        .map_err(|error| discharge_error("edge_sidecar_rows", error))?;
        bbox_corpus_index::index::migration_inventory::discharge_project_rows(
            &self.config.paths.index_path,
            &self.config.paths.state_dir.join("git_meta"),
            project_id.as_str(),
            &selectors,
        )
        .map_err(|error| discharge_error("corpus_index_rows", error))?;
        bbox_vectors::migration_inventory::discharge_project_rows(
            &self.config.paths.state_dir.join("vectors"),
            project_id.as_str(),
        )
        .map_err(|error| discharge_error("vector_entity_refs", error))?;

        Ok(())
    }

    /// Stage PublicationsCleared: delete the accepted-publication pointer.
    fn discharge_publications(
        &mut self,
        project_id: &ProjectId,
    ) -> project_catalog_admin::AdminResult<()> {
        if let Some(pointer) = accepted_publication_pointer(self.projects_path, project_id) {
            durable_remove_file_if_exists(&pointer).map_err(|e| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_discharge_publication",
                    format!("failed to durably delete accepted-publication pointer: {e}"),
                )
            })?;
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
        _project_id: &ProjectId,
        evidence: &project_catalog_admin::RetirementJournalEvidence,
    ) -> project_catalog_admin::AdminResult<()> {
        let code_sources = self.config.paths.state_dir.join("code-sources");
        let store = bbox_code_source_store::CodeSourceStore::open(
            &code_sources,
            bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits(
                self.config,
            ),
        )
        .map_err(|e| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_code_source_open",
                format!("failed to open code-source store: {e}"),
            )
        })?;
        let candidates = evidence.owned_blob_hashes.iter().cloned().collect();
        store.sweep_retirement_blobs(&candidates).map_err(|e| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_sweep_blobs",
                format!("failed to sweep retirement blobs: {e}"),
            )
        })
    }

    /// Re-inventory cross-store reference classes from current state after
    /// all discharge stages. Re-runs the existing probe machinery against
    /// live stores (section 11.3 step 7). Uses the already-open store
    /// handle instead of reopening with an exclusive lock, which would
    /// deadlock against the command's own lifetime lock (F5).
    fn reprobe_evidence(
        &mut self,
        store: &ProjectCatalogStore,
        project_id: &ProjectId,
        _original_evidence: &project_catalog_admin::RetireEvidence,
        retirement_evidence: &project_catalog_admin::RetirementJournalEvidence,
    ) -> project_catalog_admin::AdminResult<project_catalog_admin::RetireEvidence> {
        let selectors = retirement_evidence
            .project_selectors
            .as_deref()
            .ok_or_else(|| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_evidence_incomplete",
                    "retirement evidence is missing project selectors",
                )
            })?;
        let probe = probe_retire_evidence(
            self.config,
            self.projects_path,
            store,
            project_id,
            Some(selectors),
        )
        .map_err(|e| {
            project_catalog_admin::admin_error(
                "error.project_catalog_cli_reprobe_failed",
                format!("{}: {}", e.code, e.message),
            )
        })?;
        // R2F1: carry unprobeable classes through as refusals so they
        // cannot be mistaken for a discharged zero.
        let mut evidence = probe.evidence;
        evidence.unprobeable_classes = probe.unprobeable.clone();
        Ok(evidence)
    }

    /// Verify source authority has quiesced (R3F1): the project must not
    /// hold config-level or catalog-level authority that prevents
    /// retirement. A retained collected activation record is STATE that
    /// will be discharged by stage CollectedGenerationsDischarged, NOT
    /// evidence that blocks this stage. Producer assignments are derived
    /// from the config+catalog only: if the project is present in the
    /// catalog with an active scope, that is one assignment (which the
    /// retire operation will clear). Attachment presence does NOT refuse
    /// quiescence here; attachments are detached in a later journal stage.
    fn verify_source_authority_quiesced(
        &mut self,
        _store: &ProjectCatalogStore,
        project_id: &ProjectId,
        evidence: &project_catalog_admin::RetirementJournalEvidence,
    ) -> project_catalog_admin::AdminResult<()> {
        let granted = evidence.catalog_scope.as_ref().is_some_and(|scope| {
            self.config
                .code_collection
                .producers
                .iter()
                .any(|producer| producer.scopes.iter().any(|grant| grant == scope))
        });
        if granted {
            return Err(project_catalog_admin::admin_error(
                "error.project_catalog_retire_producer_grant",
                format!(
                    "project {project_id} still has a configured producer grant; \
                     revoke it before retirement can advance"
                ),
            ));
        }
        Ok(())
    }

    fn verify_retirement_quiescent(
        &mut self,
        project_id: &ProjectId,
        evidence: &project_catalog_admin::RetirementJournalEvidence,
    ) -> project_catalog_admin::AdminResult<()> {
        let store = ProjectCatalogStore::open_existing(self.projects_path).map_err(|e| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_catalog_open",
                format!("failed to open project catalog during recovery: {e}"),
            )
        })?;
        self.verify_source_authority_quiesced(&store, project_id, evidence)?;
        validate_retirement_targets_absent(self.config, evidence)?;
        let selectors = evidence.project_selectors.as_deref().ok_or_else(|| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_evidence_incomplete",
                "retirement evidence is missing project selectors",
            )
        })?;
        let probe = probe_retire_evidence(
            self.config,
            self.projects_path,
            &store,
            project_id,
            Some(selectors),
        )
        .map_err(|error| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_recovery_reprobe",
                format!("{}: {}", error.code, error.message),
            )
        })?;
        if !probe.unprobeable.is_empty() {
            return Err(project_catalog_admin::admin_error(
                "error.project_catalog_retire_recovery_reprobe",
                format!(
                    "post-cut recovery could not probe: {}",
                    probe.unprobeable.join(", ")
                ),
            ));
        }
        let remaining = probe
            .evidence
            .external_reference_counts
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(class, count)| format!("{class}={count}"))
            .collect::<Vec<_>>();
        if !remaining.is_empty() {
            return Err(project_catalog_admin::admin_error(
                "error.project_catalog_retire_recovery_not_quiescent",
                format!(
                    "post-cut recovery found owner rows that were not discharged: {}",
                    remaining.join(", ")
                ),
            ));
        }
        Ok(())
    }
}

fn reconcile_completed_retained_owner_deletions(
    config: &config::Config,
    project_id: &ProjectId,
    evidence: &project_catalog_admin::RetirementJournalEvidence,
) -> project_catalog_admin::AdminResult<()> {
    let code_sources = config.paths.state_dir.join("code-sources");
    let store = bbox_code_source_store::CodeSourceStore::open(
        &code_sources,
        bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits(config),
    )
    .map_err(|error| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_code_source_open",
            format!("failed to open code-source store for owner reconciliation: {error}"),
        )
    })?;
    let prepared = evidence
        .owned_generations
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for record in store.retained_generation_owner_records().map_err(|error| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_retained_owner",
            format!("failed to enumerate retained-generation ownership: {error}"),
        )
    })? {
        if record.project_id != *project_id {
            continue;
        }
        let identity = project_catalog_admin::RetirementGenerationEvidence {
            published_scope: record.published_scope.clone(),
            generation_id: record.generation_id.clone(),
        };
        if store
            .retirement_generation_exists(&record.published_scope, &record.generation_id)
            .map_err(|error| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_retained_owner",
                    format!("failed to validate retained-generation ownership: {error}"),
                )
            })?
        {
            continue;
        }
        if !prepared.contains(&identity) {
            return Err(project_catalog_admin::admin_error(
                "error.project_catalog_retire_retained_owner",
                "retained owner record lost its generation outside the Prepared retirement plan",
            ));
        }
        store
            .delete_retained_generation_owner(
                project_id,
                &record.published_scope,
                &record.generation_id,
            )
            .map_err(|error| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_retained_owner",
                    format!("failed to clear completed retained-owner deletion: {error}"),
                )
            })?;
    }
    Ok(())
}

fn validate_retirement_targets_absent(
    config: &config::Config,
    evidence: &project_catalog_admin::RetirementJournalEvidence,
) -> project_catalog_admin::AdminResult<()> {
    let code_sources = config.paths.state_dir.join("code-sources");
    let store = bbox_code_source_store::CodeSourceStore::open(
        &code_sources,
        bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits(config),
    )
    .map_err(|error| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_code_source_open",
            format!("failed to open code-source store for evidence validation: {error}"),
        )
    })?;
    let generations = store.retirement_generation_inventory().map_err(|error| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_evidence_generations",
            format!("failed to validate generation evidence: {error}"),
        )
    })?;
    let desired = store
        .retirement_desired_pointer_inventory()
        .map_err(|error| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_evidence_desired",
                format!("failed to validate desired-pointer evidence: {error}"),
            )
        })?;
    let uploads = store.retirement_upload_inventory().map_err(|error| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_evidence_uploads",
            format!("failed to validate upload evidence: {error}"),
        )
    })?;
    let generation_present = evidence.owned_generations.iter().any(|expected| {
        generations.iter().any(|actual| {
            actual.published_scope == expected.published_scope
                && actual.generation_id == expected.generation_id
        })
    });
    let desired_present = evidence
        .desired_pointers
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|expected| {
            desired.iter().any(|actual| {
                actual.published_scope == expected.published_scope
                    && actual.generation_id == expected.generation_id
            })
        });
    let upload_present = evidence
        .owned_uploads
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|expected| {
            uploads.iter().any(|actual| {
                actual.producer_id == expected.producer_id
                    && actual.upload_id == expected.upload_id
                    && actual.published_scope == expected.published_scope
            })
        });
    let edge_present =
        bbox_edge_sidecar::migration_inventory::capture_project_retirement_inventory(
            &config.paths.state_dir.join("edges"),
            evidence
                .owner_project_id
                .as_ref()
                .map(ProjectId::as_str)
                .unwrap_or_default(),
        )
        .map_err(|error| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_edge_evidence",
                format!("failed to validate edge retirement evidence: {error}"),
            )
        })?
        .relative_paths
        .iter()
        .any(|path| {
            evidence
                .edge_paths
                .as_deref()
                .unwrap_or_default()
                .contains(path)
        });
    if generation_present || desired_present || upload_present || edge_present {
        return Err(project_catalog_admin::admin_error(
            "error.project_catalog_retire_evidence_owner",
            "persisted retirement target remains present after its discharge stage",
        ));
    }
    Ok(())
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
    Committed(Vec<String>),
    Unprobeable,
}

impl ClassProbe {
    #[allow(dead_code)]
    fn is_present(&self) -> bool {
        matches!(self, ClassProbe::Counted(n) if *n > 0)
            || matches!(self, ClassProbe::Committed(rows) if !rows.is_empty())
    }
}

/// Bounded external-reference evidence plus the classes this host could not
/// answer for.
#[derive(Default)]
struct RetireProbe {
    evidence: project_catalog_admin::RetireEvidence,
    commitments: BTreeMap<String, Vec<String>>,
    unprobeable: Vec<String>,
}

impl RetireProbe {
    fn record(&mut self, class: &str, probe: ClassProbe) {
        match probe {
            ClassProbe::Counted(0) => {
                self.commitments.insert(class.to_string(), Vec::new());
            }
            ClassProbe::Counted(count) => {
                self.evidence
                    .external_reference_counts
                    .insert(class.to_string(), count);
                self.commitments.insert(
                    class.to_string(),
                    vec![retirement_commitment(&(class, count))],
                );
            }
            ClassProbe::Committed(mut identities) => {
                identities.sort();
                identities.dedup();
                self.evidence
                    .external_reference_counts
                    .insert(class.to_string(), identities.len() as u64);
                self.commitments.insert(class.to_string(), identities);
            }
            ClassProbe::Unprobeable => self.unprobeable.push(class.to_string()),
        }
    }
}

fn retirement_commitment(value: &impl serde::Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("retirement commitment serialization is infallible");
    bbox_corpus_core::project_catalog_snapshot::sha256_hex(&bytes)
}

fn owner_snapshot_row_commitment(
    row: &bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotRowV1,
) -> String {
    use bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotRowValueV1;

    match &row.value {
        OwnerSnapshotRowValueV1::InventoryTarget {
            project_id,
            target_sha256,
        } => retirement_commitment(&(
            row.stable_row_id.as_str(),
            "inventory_target",
            project_id,
            target_sha256,
        )),
        OwnerSnapshotRowValueV1::LegacyProjectSelector {
            selector_kind,
            literal_selector,
        } => retirement_commitment(&(
            row.stable_row_id.as_str(),
            "legacy_project_selector",
            format!("{selector_kind:?}"),
            literal_selector,
        )),
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
    selector_override: Option<&[String]>,
) -> Result<RetireProbe, CommandFailure> {
    let mut probe = RetireProbe::default();
    let state = store.snapshot()?;
    let selectors: Vec<String> = selector_override
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| {
            std::iter::once(project_id.as_str().to_string())
                .chain(
                    state
                        .attachments()
                        .attachments
                        .values()
                        .filter(|row| &row.project_id == project_id)
                        .map(|row| row.checkout_project_dir.clone()),
                )
                .collect()
        });

    // Code-source state roots on the configured state dir (the same
    // derivation the daemon uses), never the projects-path parent.
    let code_sources = config.paths.state_dir.join("code-sources");
    match bbox_code_source_store::CodeSourceStorePaths::new(&code_sources) {
        Ok(paths) => {
            let activation = paths.activation(project_id);
            probe.record("code_source_activation", presence_probe(&activation));
            // R3F1: producer assignments come from the config grants only.
            // A producer assignment is a config-level grant where a producer
            // is authorized to publish to one of the project's scopes. The
            // migration-era effective-source manifest is NOT read. If the
            // project's scope is no longer in any producer's grant list,
            // the assignment count is zero.
            let current_scopes = current_scope_hashes(&state, project_id);
            let assignment_count = count_config_producer_assignments(config, &current_scopes);
            probe.record("producer_assignments", assignment_count);
            // R3F1: generation evidence comes from the activation record's
            // generation_id (if present) plus store-owned enumeration of
            // generations under the project's owned scope hashes. No
            // migration-era manifest reads, no producer_id comparisons.
            let named_generations = match read_json_field(&activation, "generation_id") {
                Ok(active) => active,
                Err(_) => None,
            };
            probe.record(
                "code_source_generations",
                probe_code_source_generations_store_owned(
                    &code_sources,
                    &current_scopes,
                    named_generations.as_deref(),
                ),
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
            Some(pointer) => file_commitment_probe(&pointer),
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
            count_project_rows(path, project_id.as_str(), &selectors, &PROJECT_ROW_KEYS),
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
            count_project_rows(&path, project_id.as_str(), &selectors, &SLACK_ROW_KEYS),
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

fn current_scope_hashes(
    state: &bbox_indexing::project_catalog_store::ProjectCatalogState,
    project_id: &ProjectId,
) -> BTreeSet<String> {
    state
        .catalog()
        .projects
        .get(project_id)
        .and_then(|project| match &project.scope {
            ProjectScope::Published(scope) => Some(bbox_code_source::scope_hash(scope)),
            ProjectScope::LegacyLocal => None,
        })
        .into_iter()
        .collect()
}

fn capture_retirement_evidence(
    config: &config::Config,
    projects_path: &Path,
    project_id: &ProjectId,
) -> project_catalog_admin::AdminResult<project_catalog_admin::RetirementJournalEvidence> {
    let catalog_store = ProjectCatalogStore::open_existing(projects_path).map_err(|e| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_catalog_open",
            format!("failed to open project catalog for retirement evidence: {e}"),
        )
    })?;
    let state = catalog_store.snapshot().map_err(|e| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_catalog_snapshot",
            format!("failed to snapshot project catalog for retirement evidence: {e}"),
        )
    })?;
    let project = state.catalog().projects.get(project_id).ok_or_else(|| {
        project_catalog_admin::admin_error(
            "error.project_catalog_admin_unknown_project",
            format!("project {project_id} is not in the catalog"),
        )
    })?;
    let project_selectors = std::iter::once(project_id.as_str().to_string())
        .chain(
            state
                .attachments()
                .attachments
                .values()
                .filter(|row| &row.project_id == project_id)
                .map(|row| row.checkout_project_dir.clone()),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let current_scope = match &project.scope {
        ProjectScope::Published(scope) => Some(scope),
        ProjectScope::LegacyLocal => None,
    };

    let code_sources = config.paths.state_dir.join("code-sources");
    let store = bbox_code_source_store::CodeSourceStore::open(
        &code_sources,
        bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits(config),
    )
    .map_err(|e| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_code_source_open",
            format!("failed to open code-source store for retirement evidence: {e}"),
        )
    })?;
    store
        .ensure_retained_generation_ownership(project_id)
        .map_err(|error| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_retained_owner",
                format!("failed to persist retained-generation ownership: {error}"),
            )
        })?;

    let mut activation_owners = BTreeMap::<String, String>::new();
    for activation in store.activation_records_mixed().map_err(|e| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_evidence_activation",
            format!("failed to enumerate activation ownership: {e}"),
        )
    })? {
        if let Some(existing) = activation_owners.insert(
            activation.generation_id().to_string(),
            activation.project_id().to_string(),
        ) {
            if existing != activation.project_id() {
                return Err(project_catalog_admin::admin_error(
                    "error.project_catalog_retire_ambiguous_generation_owner",
                    format!(
                        "generation {} is activated by both {} and {}",
                        activation.generation_id(),
                        existing,
                        activation.project_id()
                    ),
                ));
            }
        }
    }
    let mut durable_generation_owners = BTreeMap::<String, BTreeSet<String>>::new();
    for (generation_id, owner) in &activation_owners {
        durable_generation_owners
            .entry(generation_id.clone())
            .or_default()
            .insert(owner.clone());
    }
    for migration in state.catalog().scope_migrations.values() {
        if let Some(generation_id) = &migration.code_bridge_generation {
            durable_generation_owners
                .entry(generation_id.clone())
                .or_default()
                .insert(migration.project_id.as_str().to_string());
        }
    }
    for retained in store.retained_generation_owner_records().map_err(|error| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_retained_owner",
            format!("failed to enumerate retained-generation ownership: {error}"),
        )
    })? {
        let generation = store
            .load_generation_mixed(&retained.published_scope, &retained.generation_id)
            .map_err(|error| {
                project_catalog_admin::admin_error(
                    "error.project_catalog_retire_retained_owner",
                    format!("retained-generation owner record has no exact generation: {error}"),
                )
            })?;
        if generation.generation_id() != retained.generation_id
            || generation.published_scope() != &retained.published_scope
        {
            return Err(project_catalog_admin::admin_error(
                "error.project_catalog_retire_retained_owner",
                "retained-generation owner record does not match generation metadata",
            ));
        }
        durable_generation_owners
            .entry(retained.generation_id)
            .or_default()
            .insert(retained.project_id.as_str().to_string());
    }
    let desired_inventory = store.retirement_desired_pointer_inventory().map_err(|e| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_evidence_desired",
            format!("failed to enumerate desired-pointer ownership: {e}"),
        )
    })?;
    let mut desired_pointers = Vec::new();
    for pointer in desired_inventory {
        if current_scope == Some(&pointer.published_scope) {
            durable_generation_owners
                .entry(pointer.generation_id.clone())
                .or_default()
                .insert(project_id.as_str().to_string());
            desired_pointers.push(project_catalog_admin::RetirementGenerationEvidence {
                published_scope: pointer.published_scope,
                generation_id: pointer.generation_id,
            });
        }
    }

    let mut owned_blob_hashes = BTreeSet::new();
    let mut owned_uploads = Vec::new();
    for upload in store.retirement_upload_inventory().map_err(|e| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_evidence_uploads",
            format!("failed to enumerate upload ownership: {e}"),
        )
    })? {
        if current_scope == Some(&upload.published_scope) {
            owned_blob_hashes.extend(upload.blob_hashes);
            owned_uploads.push(project_catalog_admin::RetirementUploadEvidence {
                producer_id: upload.producer_id,
                upload_id: upload.upload_id,
                published_scope: upload.published_scope,
            });
        }
    }

    let mut owned_generations = Vec::new();
    for generation in store.retirement_generation_inventory().map_err(|e| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_evidence_generations",
            format!("failed to enumerate generation ownership: {e}"),
        )
    })? {
        let owner_claims = durable_generation_owners
            .get(&generation.generation_id)
            .cloned()
            .unwrap_or_default();
        if !retirement_generation_is_owned(
            current_scope,
            project_id,
            &generation.published_scope,
            &generation.generation_id,
            &owner_claims,
        )? {
            continue;
        }
        owned_blob_hashes.extend(generation.blob_hashes);
        owned_generations.push(project_catalog_admin::RetirementGenerationEvidence {
            published_scope: generation.published_scope,
            generation_id: generation.generation_id,
        });
    }

    let edge_inventory =
        bbox_edge_sidecar::migration_inventory::capture_project_retirement_inventory(
            &config.paths.state_dir.join("edges"),
            project_id.as_str(),
        )
        .map_err(|error| {
            project_catalog_admin::admin_error(
                "error.project_catalog_retire_edge_evidence",
                format!("failed to capture exact edge retirement inventory: {error}"),
            )
        })?;
    let artifact_targets = bbox_artifacts::artifacts::capture_project_catalog_retirement_targets(
        &config.paths.artifacts_dir,
        project_id.as_str(),
        &project_selectors,
    )
    .map_err(|error| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_artifact_evidence",
            format!("failed to capture exact artifact retirement targets: {error}"),
        )
    })?;
    let reference_probe = probe_retire_evidence(
        config,
        projects_path,
        &catalog_store,
        project_id,
        Some(&project_selectors),
    )
    .map_err(|error| {
        project_catalog_admin::admin_error(
            "error.project_catalog_retire_plan_evidence",
            format!("{}: {}", error.code, error.message),
        )
    })?;
    if !reference_probe.unprobeable.is_empty() {
        return Err(project_catalog_admin::admin_error(
            "error.project_catalog_retire_plan_evidence",
            format!(
                "cannot capture a complete retirement plan; unprobeable classes: {}",
                reference_probe.unprobeable.join(", ")
            ),
        ));
    }
    let reference_class_counts = RETIRE_REFERENCE_CLASSES
        .iter()
        .map(|class| {
            (
                (*class).to_string(),
                reference_probe
                    .evidence
                    .external_reference_counts
                    .get(*class)
                    .copied()
                    .unwrap_or(0),
            )
        })
        .collect();

    Ok(project_catalog_admin::RetirementJournalEvidence {
        owner_project_id: Some(project_id.clone()),
        catalog_scope: current_scope.cloned(),
        project_selectors: Some(project_selectors),
        owned_generations,
        desired_pointers: Some(desired_pointers),
        owned_uploads: Some(owned_uploads),
        edge_paths: Some(edge_inventory.relative_paths),
        artifact_targets: Some(artifact_targets),
        reference_class_counts: Some(reference_class_counts),
        reference_class_commitments: Some(reference_probe.commitments),
        blob_inventory: None,
        owned_blob_hashes: owned_blob_hashes.into_iter().collect(),
    })
}

fn retirement_generation_is_owned(
    current_scope: Option<&PublishedScope>,
    project_id: &ProjectId,
    generation_scope: &PublishedScope,
    generation_id: &str,
    owner_claims: &BTreeSet<String>,
) -> project_catalog_admin::AdminResult<bool> {
    let current_scope_owned = current_scope == Some(generation_scope);
    if owner_claims.len() > 1 {
        return Err(project_catalog_admin::admin_error(
            "error.project_catalog_retire_ambiguous_generation_owner",
            format!(
                "generation {generation_id} has conflicting durable owner claims: {owner_claims:?}"
            ),
        ));
    }
    if let Some(owner) = owner_claims.first() {
        return Ok(owner == project_id.as_str());
    }
    if current_scope_owned {
        return Err(project_catalog_admin::admin_error(
            "error.project_catalog_retire_ownerless_generation",
            format!(
                "generation {generation_id} is retained in project {project_id} current scope \
                 without durable owner-bound evidence"
            ),
        ));
    }
    Ok(false)
}

/// R3F1: count producer assignments from the config grants. A producer
/// assignment is a config-level grant where a producer is authorized to
/// publish to a scope owned by the project. Does NOT read the migration-era
/// effective-source manifest.
fn count_config_producer_assignments(
    config: &config::Config,
    owned_scope_hashes: &BTreeSet<String>,
) -> ClassProbe {
    let mut count = 0_u64;
    for producer in &config.code_collection.producers {
        for scope in &producer.scopes {
            let hash = bbox_code_source::scope_hash(scope);
            if owned_scope_hashes.contains(&hash) {
                count += 1;
            }
        }
    }
    ClassProbe::Counted(count)
}

/// R3F1: retained generations attributable to the project, enumerated
/// through the store's validated scope/generation walk. No hand-rolled
/// directory reads, no producer_id/project_id comparison. A generation
/// belongs to the project when its published_scope matches one of the
/// project's owned scope hashes, or when its generation_id matches the
/// activation record's named generation.
fn probe_code_source_generations_store_owned(
    code_sources: &Path,
    owned_scope_hashes: &BTreeSet<String>,
    named_generation: Option<&str>,
) -> ClassProbe {
    let scopes_root = code_sources.join("scopes");
    let scope_entries = match std::fs::read_dir(&scopes_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ClassProbe::Counted(0);
        }
        Err(_) => return ClassProbe::Unprobeable,
    };
    let mut count = 0_u64;
    for scope_entry in scope_entries {
        let Ok(scope_entry) = scope_entry else {
            return ClassProbe::Unprobeable;
        };
        let Ok(kind) = scope_entry.file_type() else {
            return ClassProbe::Unprobeable;
        };
        if kind.is_symlink() || !kind.is_dir() {
            continue;
        }
        let scope_hash = match scope_entry.file_name().to_str() {
            Some(h) => h.to_string(),
            None => continue,
        };
        let owned = owned_scope_hashes.contains(&scope_hash);
        let generations_dir = scope_entry.path().join("generations");
        let gen_entries = match std::fs::read_dir(&generations_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return ClassProbe::Unprobeable,
        };
        for gen_entry in gen_entries {
            let Ok(gen_entry) = gen_entry else {
                return ClassProbe::Unprobeable;
            };
            let Ok(kind) = gen_entry.file_type() else {
                return ClassProbe::Unprobeable;
            };
            if kind.is_symlink() || !kind.is_dir() {
                continue;
            }
            let gen_id = match gen_entry.file_name().to_str() {
                Some(id) => id.to_string(),
                None => continue,
            };
            // R3F1: match by scope ownership or named generation id.
            // No producer_id comparison.
            if owned || named_generation == Some(gen_id.as_str()) {
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
    match bbox_edge_sidecar::migration_inventory::capture_project_retirement_inventory(
        edges_dir,
        project_id.as_str(),
    ) {
        Ok(inventory) => ClassProbe::Committed(
            inventory
                .relative_paths
                .iter()
                .map(retirement_commitment)
                .collect(),
        ),
        Err(_) => ClassProbe::Unprobeable,
    }
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
            return ClassProbe::Committed(Vec::new());
        }
        OwnerSnapshotStateV1::Present { .. } => {}
        _ => return ClassProbe::Unprobeable,
    }
    ClassProbe::Committed(
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
            .map(owner_snapshot_row_commitment)
            .collect(),
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
        CorpusMigrationSourceStateV1::Missing => ClassProbe::Committed(Vec::new()),
        CorpusMigrationSourceStateV1::Present => ClassProbe::Committed(
            index
                .project_scoped_refs
                .iter()
                .filter(|row| selectors.iter().any(|selector| selector == &row.project_id))
                .map(|row| {
                    retirement_commitment(&(
                        &row.project_id,
                        &row.entity_ref,
                        row.document_count,
                    ))
                })
                .collect(),
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
        CorpusMigrationSourceStateV1::Missing => ClassProbe::Committed(Vec::new()),
        CorpusMigrationSourceStateV1::Present => ClassProbe::Committed(
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
                .map(|row| {
                    retirement_commitment(&(
                        &row.source_key_sha256,
                        match row.source_kind {
                            corpus_inventory::CodeIndexMetadataSourceKindV1::LegacyFilesystem => {
                                "legacy_filesystem"
                            }
                            corpus_inventory::CodeIndexMetadataSourceKindV1::LocalProjectFile => {
                                "local_project_file"
                            }
                        },
                        row.mtime,
                        row.size,
                        &row.materialization_version,
                        &row.project_id,
                        &row.selector,
                        &row.relative_path,
                        &row.entry_key,
                    ))
                })
                .collect(),
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
        CorpusMigrationSourceStateV1::Missing => ClassProbe::Committed(Vec::new()),
        CorpusMigrationSourceStateV1::Present => ClassProbe::Committed(
            cursors
                .rows
                .iter()
                .filter(|row| selectors.iter().any(|selector| selector == &row.project_id))
                .map(|row| retirement_commitment(&(&row.project_id, &row.last_ingested_sha)))
                .collect(),
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
        VectorMigrationSourceStateV1::Missing => ClassProbe::Committed(Vec::new()),
        VectorMigrationSourceStateV1::Present => ClassProbe::Committed(
            snapshot
                .project_scoped_refs
                .iter()
                .filter(|row| selectors.iter().any(|selector| selector == &row.project_id))
                .map(|row| {
                    retirement_commitment(&(
                        &row.route,
                        &row.project_id,
                        &row.entity_ref,
                        &row.content_hash,
                    ))
                })
                .collect(),
        ),
        _ => ClassProbe::Unprobeable,
    }
}

/// Count rows in one JSON coordination store whose project-naming field
/// (`keys`) names the retiring project. A store that exists but cannot be
/// read or parsed is unprobeable, never zero.
fn count_project_rows(
    path: &Path,
    project_id: &str,
    selectors: &[String],
    keys: &[&str],
) -> ClassProbe {
    let bytes = match read_regular_nofollow(path) {
        Ok(None) => return ClassProbe::Committed(Vec::new()),
        Ok(Some(bytes)) => bytes,
        Err(()) => return ClassProbe::Unprobeable,
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return ClassProbe::Unprobeable;
    };
    let mut commitments = Vec::new();
    let mut stack = vec![&value];
    while let Some(node) = stack.pop() {
        match node {
            serde_json::Value::Array(items) => stack.extend(items.iter()),
            serde_json::Value::Object(map) => {
                let hit = match map
                    .get("project_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    Some(owner) => owner == project_id,
                    None => keys.iter().filter(|key| **key != "project_id").any(|key| {
                        map.get(*key)
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|value| selectors.iter().any(|s| s == value))
                    }),
                };
                if hit {
                    commitments.push(retirement_commitment(node));
                } else {
                    stack.extend(map.values());
                }
            }
            _ => {}
        }
    }
    ClassProbe::Committed(commitments)
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

fn file_commitment_probe(path: &Path) -> ClassProbe {
    match read_regular_nofollow(path) {
        Ok(None) => ClassProbe::Committed(Vec::new()),
        Ok(Some(bytes)) => ClassProbe::Committed(vec![retirement_commitment(&bytes)]),
        Err(()) => ClassProbe::Unprobeable,
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

fn durable_remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Collect coordination row store paths and their key fields. Each tuple
/// is `(path, [key_fields])` where `key_fields` are the JSON field names
/// that carry a project id or selector. The discharge worker reads each
/// file and removes rows matching the retired project.
fn coordination_row_paths(
    config: &config::Config,
) -> Vec<(PathBuf, &'static [&'static str], &'static [&'static str])> {
    vec![
        (
            config.paths.knowledge_path.clone(),
            &["entries"],
            &PROJECT_ROW_KEYS,
        ),
        (config.paths.gaps_path.clone(), &["gaps"], &PROJECT_ROW_KEYS),
        (
            config.paths.threads_path.clone(),
            &["threads"],
            &PROJECT_ROW_KEYS,
        ),
        (
            config.paths.notes_path.clone(),
            &["notes"],
            &PROJECT_ROW_KEYS,
        ),
        (config.paths.pins_path.clone(), &["pins"], &PROJECT_ROW_KEYS),
        (
            config.paths.roadmap_path.clone(),
            &["items", "edges"],
            &PROJECT_ROW_KEYS,
        ),
    ]
}

fn row_matches_project(
    row: &serde_json::Value,
    keys: &[&str],
    project_id: &str,
    selectors: &[String],
) -> bool {
    match row
        .get("project_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
    {
        Some(owner) => owner == project_id,
        None => keys.iter().filter(|key| **key != "project_id").any(|key| {
            row.get(*key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| selectors.iter().any(|selector| selector == value))
        }),
    }
}

fn clear_wrapped_project_rows(
    path: &Path,
    array_fields: &[&str],
    keys: &[&str],
    project_id: &str,
    selectors: &[String],
) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    bbox_corpus_core::json_store::with_store_lock(path, || {
        let bytes = std::fs::read(path)?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        for field in array_fields {
            let rows = value
                .get_mut(*field)
                .and_then(serde_json::Value::as_array_mut)
                .ok_or_else(|| anyhow::anyhow!("missing array field {field}"))?;
            rows.retain(|row| !row_matches_project(row, keys, project_id, selectors));
        }
        bbox_corpus_core::json_store::atomic_write_json_locked(path, &value)
    })
}

fn clear_slack_channel_bindings(
    path: &Path,
    project_id: &str,
    selectors: &[String],
) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    bbox_corpus_core::json_store::with_store_lock(path, || {
        let bytes = std::fs::read(path)?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let bindings = value
            .get_mut("bindings")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| anyhow::anyhow!("missing object field bindings"))?;
        bindings.retain(|_, row| !row_matches_project(row, &SLACK_ROW_KEYS, project_id, selectors));
        bbox_corpus_core::json_store::atomic_write_json_locked(path, &value)
    })
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
