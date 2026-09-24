#![allow(
    clippy::disallowed_methods,
    reason = "Synchronous fixture IO isolated to canonical tempdirs"
)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use bbox_corpus_core::identity::PublishedScope;

use super::*;
use crate::model::{
    Approval, Category, GuidanceTopic, KnowledgeEntry, Priority, RenderPlacement, Scope, Status,
};
use crate::transport::{
    PROJECT_RENDER_TRANSPORT_SCOPE, ProjectRenderOutcomeV1, ProjectRenderProducerAuthorityV1,
    ProjectRenderViewV1, format_render_operation_id,
};

type Interleave = Box<dyn FnMut(&str, &Path) -> Result<()>>;

thread_local! {
    static INTERLEAVE: RefCell<Option<Interleave>> = const { RefCell::new(None) };
}

/// Deterministic interleaving: the executor calls this at named points on
/// the applying thread; a test installs a closure that edits the checkout
/// or injects a failure there.
pub(super) fn interleave(point: &str, root: &Path) -> Result<()> {
    INTERLEAVE.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(point, root),
        None => Ok(()),
    })
}

fn with_interleave<T>(
    hook: impl FnMut(&str, &Path) -> Result<()> + 'static,
    run: impl FnOnce() -> T,
) -> T {
    INTERLEAVE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    let result = run();
    INTERLEAVE.with(|slot| *slot.borrow_mut() = None);
    result
}

const PROJECT: &str = "p_render_leaf";
const OPERATION: u128 = 0x1234;

fn scope() -> PublishedScope {
    PublishedScope::try_new("repo-render-leaf", ".").unwrap()
}

fn entry(id: &str, content: &str) -> KnowledgeEntry {
    KnowledgeEntry {
        id: id.into(),
        title: id.into(),
        content: content.into(),
        cluster: None,
        variants: HashMap::new(),
        category: Category::Convention,
        scope: Scope::Project,
        project: Some(PROJECT_RENDER_TRANSPORT_SCOPE.into()),
        project_id: Some(PROJECT.into()),
        providers: Vec::new(),
        priority: Priority::Standard,
        weight: 100,
        status: Status::Active,
        approval: Approval::UserConfirmed,
        render: true,
        render_placement: RenderPlacement::Inline,
        decay: false,
        review_at: None,
        supersedes: None,
        links: Vec::new(),
        rationale: None,
        expires_at: None,
        source: "test".into(),
        created_at: "2026-09-01T00:00:00Z".into(),
        updated_at: "2026-09-01T00:00:00Z".into(),
        recall_count: 0,
        last_recalled: None,
    }
}

fn producer_plan(provider: Option<&str>, dry_run: bool) -> ProjectRenderPlanV1 {
    ProjectRenderPlanV1 {
        version: PROJECT_RENDER_TRANSPORT_VERSION,
        project_id: PROJECT.into(),
        scope: scope(),
        workspace_id: String::new(),
        producer: Some(ProjectRenderProducerAuthorityV1 {
            producer_id: "producer-a".into(),
            operation_id: format_render_operation_id(OPERATION),
            sequence: 7,
            issued_at_ms: 1_000,
        }),
        provider: provider.map(str::to_owned),
        dry_run,
        view: ProjectRenderViewV1::Published,
        requested_scope: "project".into(),
        entries: vec![entry("leaf-inline", "LEAF_INLINE_MARKER")],
        diagnostics: None,
    }
}

fn with_satellite(mut plan: ProjectRenderPlanV1) -> ProjectRenderPlanV1 {
    let mut satellite = entry("leaf-satellite", "LEAF_SATELLITE_MARKER");
    satellite.render_placement = RenderPlacement::Satellite {
        topic: GuidanceTopic::Build,
    };
    plan.entries.push(satellite);
    plan
}

fn temp_root() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    (directory, root)
}

fn execute(plan: &ProjectRenderPlanV1, root: &Path) -> Result<ProjectRenderExecutionV1> {
    let operation_id = format_render_operation_id(OPERATION);
    execute_project_render_plan_as(
        plan,
        root,
        &scope(),
        ExpectedRenderAuthority::Producer {
            operation_id: &operation_id,
        },
        Duration::from_millis(200),
    )
}

fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for item in fs::read_dir(&directory).unwrap() {
            let path = item.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                files.push((
                    path.strip_prefix(root).unwrap().display().to_string(),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn producer_plan_writes_fixed_outputs_and_a_validated_path_free_receipt() {
    let (_directory, root) = temp_root();
    let plan = with_satellite(producer_plan(None, false));
    let execution = execute(&plan, &root).unwrap();
    execution.receipt.validate_against(&plan).unwrap();
    assert_eq!(execution.receipt.producer, plan.producer);
    assert_eq!(
        execution.receipt.outcome(),
        ProjectRenderOutcomeV1::Converged
    );
    let receipt = serde_json::to_string(&execution.receipt).unwrap();
    assert!(!receipt.contains(root.to_str().unwrap()), "{receipt}");
    for projection in &execution.receipt.projections {
        let fixed = matches!(
            projection.file_name.as_str(),
            "CLAUDE.md" | "AGENTS.md" | "GEMINI.md"
        );
        let satellite = projection
            .file_name
            .strip_prefix(".bbox/guidance/")
            .is_some_and(|rest| {
                let parts = rest.split('/').collect::<Vec<_>>();
                parts.len() == 2 && parts[0].len() == 64 && parts[1].ends_with(".md")
            });
        assert!(fixed || satellite, "{}", projection.file_name);
        let bytes = fs::read(root.join(&projection.file_name)).unwrap();
        assert_eq!(
            projection.projection_sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(&bytes)).as_str()),
            "guidance and entrypoint hashes are the exact written bytes"
        );
    }
    // Only fixed outputs exist; no staging residue remains.
    for (path, _) in tree(&root) {
        assert!(
            ["CLAUDE.md", "AGENTS.md", "GEMINI.md"].contains(&path.as_str())
                || path.starts_with(".bbox/guidance/")
                || path.starts_with(".bbox/local/"),
            "{path}"
        );
    }
}

#[test]
fn workspace_plans_serialize_without_producer_authority() {
    let mut plan = producer_plan(Some("claude"), false);
    plan.producer = None;
    plan.workspace_id = "workspace-a".into();
    plan.validate().unwrap();
    let wire = serde_json::to_value(&plan).unwrap();
    assert!(wire.get("producer").is_none());
    plan.producer = producer_plan(None, false).producer;
    assert!(plan.validate().is_err(), "two authorities are refused");
    plan.workspace_id.clear();
    plan.producer = None;
    assert!(plan.validate().is_err(), "no authority is refused");
}

#[test]
fn producer_plans_refuse_a_different_operation_or_scope() {
    let (_directory, root) = temp_root();
    let plan = producer_plan(Some("claude"), false);
    let other = format_render_operation_id(OPERATION + 1);
    assert!(
        execute_project_render_plan_as(
            &plan,
            &root,
            &scope(),
            ExpectedRenderAuthority::Producer {
                operation_id: &other
            },
            Duration::from_millis(50),
        )
        .is_err()
    );
    assert!(
        execute_project_render_plan(&plan, &root, &scope(), "workspace-a").is_err(),
        "a producer plan is not a workspace plan"
    );
    let foreign = PublishedScope::try_new("repo-foreign", ".").unwrap();
    let operation_id = format_render_operation_id(OPERATION);
    assert!(
        execute_project_render_plan_as(
            &plan,
            &root,
            &foreign,
            ExpectedRenderAuthority::Producer {
                operation_id: &operation_id
            },
            Duration::from_millis(50),
        )
        .is_err()
    );
    assert!(tree(&root).is_empty());
}

#[test]
fn handwritten_files_are_preserved_and_generated_files_replaced() {
    let (_directory, root) = temp_root();
    fs::write(root.join("CLAUDE.md"), "hand-authored guidance\n").unwrap();
    fs::write(
        root.join("AGENTS.md"),
        "<!-- Generated by blackbox. Do not edit directly. -->\nold body\n",
    )
    .unwrap();
    let execution = execute(&producer_plan(None, false), &root).unwrap();
    let by_file = execution
        .receipt
        .projections
        .iter()
        .map(|projection| (projection.file_name.as_str(), projection.disposition))
        .collect::<HashMap<_, _>>();
    assert_eq!(by_file["CLAUDE.md"], ProjectRenderDispositionV1::Refused);
    assert_eq!(by_file["AGENTS.md"], ProjectRenderDispositionV1::Written);
    assert_eq!(by_file["GEMINI.md"], ProjectRenderDispositionV1::Written);
    assert_eq!(
        execution.receipt.outcome(),
        ProjectRenderOutcomeV1::Preserved
    );
    assert_eq!(
        fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
        "hand-authored guidance\n"
    );
    assert!(
        fs::read_to_string(root.join("AGENTS.md"))
            .unwrap()
            .contains("LEAF_INLINE_MARKER")
    );
}

#[test]
fn dry_run_writes_nothing_and_previews_refusals() {
    let (_directory, root) = temp_root();
    fs::write(root.join("GEMINI.md"), "hand-authored\n").unwrap();
    let plan = with_satellite(producer_plan(None, true));
    let execution = execute(&plan, &root).unwrap();
    execution.receipt.validate_against(&plan).unwrap();
    let dispositions = execution
        .receipt
        .projections
        .iter()
        .map(|projection| projection.disposition)
        .collect::<Vec<_>>();
    assert!(dispositions.contains(&ProjectRenderDispositionV1::DryRunRefused));
    assert!(!dispositions.contains(&ProjectRenderDispositionV1::Written));
    assert_eq!(
        tree(&root),
        vec![("GEMINI.md".to_string(), b"hand-authored\n".to_vec())]
    );
}

#[test]
fn project_doc_presence_is_observed_on_every_execution() {
    let (_directory, root) = temp_root();
    let plan = producer_plan(Some("claude"), false);
    let without = execute(&plan, &root).unwrap();
    assert!(!without.receipt.project_doc_nonempty);
    assert!(
        !fs::read_to_string(root.join("CLAUDE.md"))
            .unwrap()
            .contains("PROJECT.md")
    );
    fs::write(root.join("PROJECT.md"), "# Project\n").unwrap();
    let with = execute(&plan, &root).unwrap();
    assert!(with.receipt.project_doc_nonempty);
    with.receipt.validate_against(&plan).unwrap();
    assert!(
        fs::read_to_string(root.join("CLAUDE.md"))
            .unwrap()
            .contains("PROJECT.md")
    );
    // A fresh execution with unchanged knowledge restores a deleted output.
    fs::remove_file(root.join("CLAUDE.md")).unwrap();
    let restored = execute(&plan, &root).unwrap();
    assert_eq!(restored.receipt, with.receipt);
    assert!(root.join("CLAUDE.md").is_file());
}

#[cfg(unix)]
#[test]
fn symlinked_targets_and_parents_and_special_files_refuse_before_writing() {
    let (_directory, root) = temp_root();
    let (_outside_directory, outside) = temp_root();
    fs::write(outside.join("target.md"), "outside\n").unwrap();
    std::os::unix::fs::symlink(outside.join("target.md"), root.join("CLAUDE.md")).unwrap();
    let error = execute(&producer_plan(None, false), &root).unwrap_err();
    assert!(
        format!("{error:#}").contains("error.render_target_unsafe"),
        "{error:#}"
    );
    assert!(
        !root.join("AGENTS.md").exists(),
        "no output before preflight passes"
    );
    assert_eq!(
        fs::read_to_string(outside.join("target.md")).unwrap(),
        "outside\n"
    );
    fs::remove_file(root.join("CLAUDE.md")).unwrap();

    std::os::unix::fs::symlink(&outside, root.join(".bbox")).unwrap();
    assert!(execute(&with_satellite(producer_plan(None, false)), &root).is_err());
    assert!(!root.join("CLAUDE.md").exists());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    fs::remove_file(root.join(".bbox")).unwrap();

    assert!(
        std::process::Command::new("mkfifo")
            .arg(root.join("GEMINI.md"))
            .status()
            .unwrap()
            .success()
    );
    let error = execute(&producer_plan(None, false), &root).unwrap_err();
    assert!(
        format!("{error:#}").contains("not a regular file"),
        "{error:#}"
    );
    assert!(!root.join("CLAUDE.md").exists());

    let (_linked_directory, linked) = temp_root();
    std::os::unix::fs::symlink(&linked, root.join("linked-root")).unwrap();
    assert!(
        execute(&producer_plan(None, false), &root.join("linked-root")).is_err(),
        "a root reached through a symlink is not the stable bound directory"
    );
    assert_eq!(fs::read_dir(&linked).unwrap().count(), 0);
}

#[test]
fn a_concurrent_owner_edit_after_preflight_is_preserved() {
    let (_directory, root) = temp_root();
    let target = root.join("CLAUDE.md");
    fs::write(&target, "<!-- Generated by blackbox -->\nold\n").unwrap();
    let observed = observe_target(&target).unwrap();
    fs::write(&target, "<!-- Generated by blackbox -->\nowner edit\n").unwrap();
    let published = publish_entrypoint(&root, "CLAUDE.md", "new\n", &observed).unwrap();
    assert_eq!(published.disposition, ProjectRenderDispositionV1::Conflict);
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "<!-- Generated by blackbox -->\nowner edit\n"
    );

    let absent = root.join("AGENTS.md");
    let observed = observe_target(&absent).unwrap();
    fs::write(&absent, "created concurrently\n").unwrap();
    let published = publish_entrypoint(&root, "AGENTS.md", "new\n", &observed).unwrap();
    assert_eq!(published.disposition, ProjectRenderDispositionV1::Conflict);
    assert_eq!(
        fs::read_to_string(&absent).unwrap(),
        "created concurrently\n"
    );
    assert_eq!(tree(&root).len(), 2, "no staged sibling remains");
}

fn disposition_of(receipt: &ProjectRenderReceiptV1, file_name: &str) -> ProjectRenderDispositionV1 {
    receipt
        .projections
        .iter()
        .find(|projection| projection.file_name == file_name)
        .unwrap()
        .disposition
}

#[test]
fn an_edit_before_the_unchanged_output_is_confirmed_is_preserved() {
    let (_directory, root) = temp_root();
    let plan = producer_plan(Some("claude"), false);
    execute(&plan, &root).unwrap();
    let edited = "<!-- Generated by blackbox -->\nowner edit before confirmation\n";
    let execution = with_interleave(
        move |point, root| {
            if point == "before_noop_confirm" {
                fs::write(root.join("CLAUDE.md"), edited).unwrap();
            }
            Ok(())
        },
        || execute(&plan, &root).unwrap(),
    );
    assert_eq!(
        disposition_of(&execution.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Conflict,
        "an unchanged-output fast path must not claim bytes it did not observe"
    );
    assert_eq!(fs::read_to_string(root.join("CLAUDE.md")).unwrap(), edited);
}

#[test]
fn an_edit_after_the_final_observation_is_restored_not_replaced() {
    let (_directory, root) = temp_root();
    let target = root.join("CLAUDE.md");
    fs::write(
        &target,
        "<!-- Generated by blackbox -->\nolder projection\n",
    )
    .unwrap();
    let edited = "<!-- Generated by blackbox -->\nowner edit before replacement\n";
    let plan = producer_plan(Some("claude"), false);
    let execution = with_interleave(
        move |point, root| {
            if point == "before_replace" {
                fs::write(root.join("CLAUDE.md"), edited).unwrap();
            }
            Ok(())
        },
        || execute(&plan, &root).unwrap(),
    );
    assert_eq!(
        disposition_of(&execution.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Conflict
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), edited);
    assert!(
        tree(&root)
            .iter()
            .all(|(path, _)| !path.contains(".render-")),
        "the restored bytes leave no sibling behind"
    );

    // The path re-created after the old bytes were moved aside is kept.
    fs::write(
        &target,
        "<!-- Generated by blackbox -->\nolder projection\n",
    )
    .unwrap();
    let recreated = "<!-- Generated by blackbox -->\nre-created by the owner\n";
    let execution = with_interleave(
        move |point, root| {
            if point == "before_publish_rename" {
                fs::write(root.join("CLAUDE.md"), recreated).unwrap();
            }
            Ok(())
        },
        || execute(&producer_plan(Some("claude"), false), &root).unwrap(),
    );
    assert_eq!(
        disposition_of(&execution.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Conflict
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), recreated);
}

#[test]
fn a_failed_publication_reports_a_partial_render() {
    let (_directory, root) = temp_root();
    let plan = producer_plan(None, false);
    let execution = with_interleave(
        |point, _| {
            if point == "publish:AGENTS.md" {
                anyhow::bail!("injected publication failure");
            }
            Ok(())
        },
        || execute(&plan, &root),
    );
    let execution = execution.unwrap();
    execution.receipt.validate_against(&plan).unwrap();
    assert_eq!(execution.receipt.outcome(), ProjectRenderOutcomeV1::Partial);
    assert!(
        execution.output.contains("Partial render"),
        "{}",
        execution.output
    );
    assert!(root.join("CLAUDE.md").is_file());
    assert!(!root.join("AGENTS.md").exists());
    assert!(root.join("GEMINI.md").is_file());
}

#[test]
fn a_directory_sync_failure_after_publication_is_incomplete_not_unwritten() {
    let (_directory, root) = temp_root();
    let plan = producer_plan(None, false);
    let execution = with_interleave(
        |point, _| {
            if point == "sync_root" {
                anyhow::bail!("injected directory sync failure");
            }
            Ok(())
        },
        || execute(&plan, &root),
    )
    .expect("a failure after publication is carried in the receipt, not raised");
    execution.receipt.validate_against(&plan).unwrap();
    assert!(execution.receipt.incomplete);
    assert_eq!(execution.receipt.outcome(), ProjectRenderOutcomeV1::Partial);
    for file in ["CLAUDE.md", "AGENTS.md", "GEMINI.md"] {
        assert_eq!(
            disposition_of(&execution.receipt, file),
            ProjectRenderDispositionV1::Written,
            "published outputs stay reported as written"
        );
        assert!(root.join(file).is_file());
    }
}

fn plan_issued_at(content: &str, operation: u128, issued_at_ms: u64) -> ProjectRenderPlanV1 {
    let mut plan = producer_plan(Some("claude"), false);
    plan.entries[0].content = content.into();
    let producer = plan.producer.as_mut().unwrap();
    producer.operation_id = format_render_operation_id(operation);
    producer.issued_at_ms = issued_at_ms;
    plan
}

fn execute_operation(plan: &ProjectRenderPlanV1, root: &Path) -> Result<ProjectRenderExecutionV1> {
    let operation_id = plan.producer.as_ref().unwrap().operation_id.clone();
    execute_project_render_plan_as(
        plan,
        root,
        &scope(),
        ExpectedRenderAuthority::Producer {
            operation_id: &operation_id,
        },
        Duration::from_millis(200),
    )
}

#[test]
fn an_older_plan_never_replaces_output_a_newer_harness_render_produced() {
    let (_directory, root) = temp_root();
    // The collector fetched an older plan, then paused.
    let older = plan_issued_at("OLDER_COLLECTOR_PLAN", 21, 100);
    // Meanwhile the bound harness renders newer knowledge.
    let mut newer = producer_plan(Some("claude"), false);
    newer.producer = None;
    newer.workspace_id = "workspace-a".into();
    newer.entries[0].content = "NEWER_HARNESS_PLAN".into();
    execute_workspace_render_plan(&newer, &root, &scope(), "workspace-a", Some(200)).unwrap();
    let harness_bytes = fs::read(root.join("CLAUDE.md")).unwrap();

    let error = execute_operation(&older, &root).unwrap_err();
    assert!(
        format!("{error:#}").contains("error.render_superseded"),
        "{error:#}"
    );
    assert_eq!(fs::read(root.join("CLAUDE.md")).unwrap(), harness_bytes);

    // A plan issued later still applies, and so does a workspace plan from a
    // daemon that predates the fence.
    execute_operation(&plan_issued_at("LATER_COLLECTOR_PLAN", 22, 300), &root).unwrap();
    assert!(
        fs::read_to_string(root.join("CLAUDE.md"))
            .unwrap()
            .contains("LATER_COLLECTOR_PLAN")
    );
    let stale_harness =
        execute_workspace_render_plan(&newer, &root, &scope(), "workspace-a", Some(250));
    assert!(stale_harness.is_err(), "the harness is fenced too");
    execute_workspace_render_plan(&newer, &root, &scope(), "workspace-a", None).unwrap();
    assert_eq!(
        fs::read_to_string(root.join(".bbox/local/.gitignore")).unwrap(),
        "*\n!.gitignore\n",
        "the fence lives in ignored local state"
    );
}

#[test]
fn an_interrupted_application_is_reconciled_without_replacing_owner_edits() {
    let (_directory, root) = temp_root();
    fs::write(
        root.join("GEMINI.md"),
        "<!-- Generated by blackbox -->\nprevious\n",
    )
    .unwrap();
    let plan = producer_plan(None, false);
    let operation_id = format_render_operation_id(OPERATION);
    let mut recorded = None;
    // The application is interrupted after publishing CLAUDE.md and before
    // publishing anything else or recording its result.
    let interrupted = with_interleave(
        |point, _| {
            if point == "publish:AGENTS.md" {
                panic!("interrupted");
            }
            Ok(())
        },
        || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut hook = |record: &PreflightRecord| -> Result<()> {
                    recorded = Some(record.clone());
                    Ok(())
                };
                execute_project_render_plan_with(
                    &plan,
                    &root,
                    &scope(),
                    ExpectedRenderAuthority::Producer {
                        operation_id: &operation_id,
                    },
                    ExecuteOptions {
                        lock_timeout: Duration::from_millis(200),
                        issued_at_ms: None,
                        before_publish: Some(&mut hook),
                    },
                )
            }))
        },
    );
    assert!(interrupted.is_err());
    let record = recorded.expect("the preflight was recorded before the first write");
    assert!(
        fs::read_to_string(root.join("CLAUDE.md"))
            .unwrap()
            .contains("LEAF_INLINE_MARKER")
    );
    // The owner edits the published file before the applier restarts.
    let edited = "<!-- Generated by blackbox -->\nowner edit after interruption\n";
    fs::write(root.join("CLAUDE.md"), edited).unwrap();

    let reconciled = reconcile_interrupted_render(
        &plan,
        &root,
        &scope(),
        ExpectedRenderAuthority::Producer {
            operation_id: &operation_id,
        },
        &record,
        Duration::from_millis(200),
    )
    .unwrap();
    reconciled.receipt.validate_against(&plan).unwrap();
    assert_eq!(
        disposition_of(&reconciled.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Conflict
    );
    assert_eq!(
        disposition_of(&reconciled.receipt, "AGENTS.md"),
        ProjectRenderDispositionV1::Failed,
        "an output still at its preflight state was never published"
    );
    assert_eq!(
        disposition_of(&reconciled.receipt, "GEMINI.md"),
        ProjectRenderDispositionV1::Failed
    );
    assert_eq!(fs::read_to_string(root.join("CLAUDE.md")).unwrap(), edited);
    assert!(!root.join("AGENTS.md").exists());
    assert_eq!(
        fs::read_to_string(root.join("GEMINI.md")).unwrap(),
        "<!-- Generated by blackbox -->\nprevious\n"
    );

    // Without the edit, the published output is reported as written.
    let unedited = temp_root();
    let unedited_root = unedited.1.clone();
    execute(&plan, &unedited_root).unwrap();
    let record = PreflightRecord {
        project_doc_nonempty: false,
        issued_at_ms: Some(1_000),
        outputs: ["CLAUDE.md", "AGENTS.md", "GEMINI.md"]
            .into_iter()
            .map(|file_name| OutputObservation {
                file_name: file_name.into(),
                state: ObservedOutputState::Absent,
            })
            .collect(),
    };
    let reconciled = reconcile_interrupted_render(
        &plan,
        &unedited_root,
        &scope(),
        ExpectedRenderAuthority::Producer {
            operation_id: &operation_id,
        },
        &record,
        Duration::from_millis(200),
    )
    .unwrap();
    assert_eq!(
        reconciled.receipt.outcome(),
        ProjectRenderOutcomeV1::Converged
    );
}

#[test]
fn an_entry_expiring_after_publication_is_validated_at_the_plan_issuance() {
    let (_directory, root) = temp_root();
    // The entry is live at issuance and expires after the output is
    // published, before the receipt is validated.
    let issued_at_ms = now_unix_ms();
    let expires_at = crate::transport::iso_from_unix_ms(issued_at_ms);
    let mut plan = plan_issued_at("EXPIRING_LEAF_MARKER", OPERATION, issued_at_ms);
    plan.entries[0].expires_at = Some(expires_at.clone());
    let execution = with_interleave(
        move |point, _| {
            if point == "sync_root" {
                while crate::transport::iso_from_unix_ms(now_unix_ms()) <= expires_at {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            Ok(())
        },
        || execute_operation(&plan, &root),
    )
    .unwrap();
    assert_eq!(
        disposition_of(&execution.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Written
    );
    assert!(
        fs::read_to_string(root.join("CLAUDE.md"))
            .unwrap()
            .contains("EXPIRING_LEAF_MARKER")
    );
    // A delayed submission, long after the expiry, validates the same way.
    execution.receipt.validate_against(&plan).unwrap();
}

#[test]
fn receipts_refuse_entrypoints_published_ahead_of_failed_satellites() {
    let (_directory, root) = temp_root();
    let plan = with_satellite(producer_plan(Some("claude"), false));
    let mut receipt = execute(&plan, &root).unwrap().receipt;
    receipt.validate_against(&plan).unwrap();
    receipt.projections[1].disposition = ProjectRenderDispositionV1::Failed;
    assert!(receipt.validate_against(&plan).is_err());
    receipt.projections[0].disposition = ProjectRenderDispositionV1::Failed;
    receipt.validate_against(&plan).unwrap();
    assert_eq!(receipt.outcome(), ProjectRenderOutcomeV1::Partial);
    let mut foreign = receipt.clone();
    foreign.producer.as_mut().unwrap().operation_id = format_render_operation_id(OPERATION + 9);
    assert!(foreign.validate_against(&plan).is_err());
}

#[test]
fn renders_of_one_checkout_are_serialized() {
    let (_directory, root) = temp_root();
    let held = lock_checkout_for_render(&root, Duration::from_millis(50)).unwrap();
    let error = execute(&producer_plan(None, false), &root).unwrap_err();
    assert!(
        format!("{error:#}").contains("error.render_busy"),
        "{error:#}"
    );
    assert!(tree(&root).is_empty());
    drop(held);

    let plan = producer_plan(None, false);
    std::thread::scope(|threads| {
        let handles = (0..4)
            .map(|_| {
                threads.spawn(|| {
                    execute_project_render_plan_as(
                        &plan,
                        &root,
                        &scope(),
                        ExpectedRenderAuthority::Producer {
                            operation_id: &format_render_operation_id(OPERATION),
                        },
                        Duration::from_secs(10),
                    )
                })
            })
            .collect::<Vec<_>>();
        let receipts = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap().receipt)
            .collect::<Vec<_>>();
        assert!(receipts.windows(2).all(|pair| pair[0] == pair[1]));
    });
    assert_eq!(
        tree(&root)
            .iter()
            .filter(|(path, _)| !path.starts_with(".bbox/local/"))
            .count(),
        3
    );
}

#[test]
fn render_lane_wire_contracts_are_bounded() {
    use crate::wire::*;
    let mut poll = RenderLanePollRequestV1 {
        schema_version: RENDER_LANE_SCHEMA_VERSION,
        render_transport_versions: vec![PROJECT_RENDER_TRANSPORT_VERSION],
        covered_scopes: vec![scope()],
        collector_version: "0.0.1".into(),
    };
    poll.validate().unwrap();
    assert!(poll.supports_current_transport());
    poll.render_transport_versions = vec![1];
    assert!(!poll.supports_current_transport());
    poll.render_transport_versions.clear();
    assert!(poll.validate().is_err());
    poll.render_transport_versions = vec![PROJECT_RENDER_TRANSPORT_VERSION];
    poll.covered_scopes = vec![scope(); MAX_RENDER_LANE_COVERED_SCOPES + 1];
    assert!(poll.validate().is_err());
    assert!(
        serde_json::from_value::<RenderLanePollRequestV1>(serde_json::json!({
            "schema_version": 1, "render_transport_versions": [2], "covered_scopes": [],
            "collector_version": "x", "unexpected": true
        }))
        .is_err()
    );

    let mut delivery = RenderOperationDeliveryV1 {
        operation_id: format_render_operation_id(OPERATION),
        kind: RENDER_PROJECT_COMMAND_KIND.into(),
        scope: scope(),
        sequence: 1,
        plan_sha256: "a".repeat(64),
        plan_bytes: 10,
    };
    delivery.validate().unwrap();
    delivery.kind = "enroll".into();
    assert!(delivery.validate().is_err());
    delivery.kind = RENDER_PROJECT_COMMAND_KIND.into();
    delivery.operation_id = "ro-../../etc".into();
    assert!(delivery.validate().is_err());

    let (_directory, root) = temp_root();
    let plan = producer_plan(Some("claude"), false);
    let receipt = execute(&plan, &root).unwrap().receipt;
    let mut result = RenderOperationResultRequestV1 {
        schema_version: RENDER_LANE_SCHEMA_VERSION,
        operation_id: format_render_operation_id(OPERATION),
        plan_sha256: "b".repeat(64),
        outcome: "applied".into(),
        receipt: Some(receipt),
        error: None,
    };
    result.validate().unwrap();
    result.operation_id = format_render_operation_id(OPERATION + 1);
    assert!(
        result.validate().is_err(),
        "receipt must name its operation"
    );
    result.operation_id = format_render_operation_id(OPERATION);
    result.error = Some(RenderOperationErrorV1 {
        code: "x".into(),
        message: "y".into(),
    });
    assert!(result.validate().is_err());
}

fn run_interrupted_at(point: &'static str, run: impl FnOnce() -> Result<ProjectRenderExecutionV1>) {
    let interrupted = with_interleave(
        move |current, _| {
            if current == point {
                panic!("interrupted at {point}");
            }
            Ok(())
        },
        || std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)),
    );
    assert!(interrupted.is_err(), "the application was not interrupted");
}

#[test]
fn an_interrupted_newer_render_still_fences_an_already_fetched_older_plan() {
    let (_directory, root) = temp_root();
    // The collector already fetched this older plan and paused.
    let older = plan_issued_at("OLDER_COLLECTOR_PLAN", 31, 100);
    // A newer harness render publishes CLAUDE.md, then is interrupted
    // before any later step.
    let mut newer = producer_plan(None, false);
    newer.producer = None;
    newer.workspace_id = "workspace-a".into();
    newer.entries[0].content = "NEWER_INTERRUPTED_PLAN".into();
    run_interrupted_at("publish:AGENTS.md", || {
        execute_workspace_render_plan(&newer, &root, &scope(), "workspace-a", Some(200))
    });
    let newer_bytes = fs::read(root.join("CLAUDE.md")).unwrap();
    assert!(String::from_utf8_lossy(&newer_bytes).contains("NEWER_INTERRUPTED_PLAN"));

    let error = execute_operation(&older, &root).unwrap_err();
    assert!(
        format!("{error:#}").contains("error.render_superseded"),
        "{error:#}"
    );
    assert_eq!(fs::read(root.join("CLAUDE.md")).unwrap(), newer_bytes);
}

#[test]
fn a_failure_after_the_move_aside_restores_the_original() {
    let (_directory, root) = temp_root();
    let original = "<!-- Generated by blackbox -->\nprevious projection\n";
    fs::write(root.join("CLAUDE.md"), original).unwrap();
    let plan = producer_plan(Some("claude"), false);
    let execution = with_interleave(
        |point, _| {
            if point == "after_move_aside" {
                anyhow::bail!("injected failure after the move-aside");
            }
            Ok(())
        },
        || execute(&plan, &root),
    )
    .unwrap();
    assert!(execution.receipt.incomplete);
    assert_eq!(
        disposition_of(&execution.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Failed
    );
    assert_eq!(
        fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
        original
    );
    assert!(
        tree(&root)
            .iter()
            .all(|(path, _)| !path.contains(".render-")),
        "the original was restored in place"
    );
}

#[test]
fn writes_through_an_open_descriptor_during_replacement_are_retained() {
    use std::io::Write as _;
    let (_directory, root) = temp_root();
    let original = "<!-- Generated by blackbox -->\nprevious projection\n";
    fs::write(root.join("CLAUDE.md"), original).unwrap();
    // The owner's editor holds the old file open and writes after the
    // render confirmed the moved bytes.
    let mut editor = fs::OpenOptions::new()
        .append(true)
        .open(root.join("CLAUDE.md"))
        .unwrap();
    let execution = with_interleave(
        move |point, _| {
            if point == "before_publish_rename" {
                editor
                    .write_all(b"owner line through an open descriptor\n")
                    .unwrap();
            }
            Ok(())
        },
        || execute(&producer_plan(Some("claude"), false), &root).unwrap(),
    );
    assert_eq!(
        disposition_of(&execution.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Conflict
    );
    let retained = tree(&root)
        .into_iter()
        .find(|(path, _)| path.ends_with(".render-prev"))
        .expect("the owner's bytes are retained beside the target");
    assert!(String::from_utf8_lossy(&retained.1).contains("owner line through an open descriptor"));

    // The same write combined with a re-created path keeps both copies.
    let (_other_directory, other) = temp_root();
    fs::write(other.join("CLAUDE.md"), original).unwrap();
    let mut editor = fs::OpenOptions::new()
        .append(true)
        .open(other.join("CLAUDE.md"))
        .unwrap();
    let recreated = "<!-- Generated by blackbox -->\nre-created\n";
    let execution = with_interleave(
        move |point, root| {
            if point == "before_publish_rename" {
                editor.write_all(b"owner line\n").unwrap();
                fs::write(root.join("CLAUDE.md"), recreated).unwrap();
            }
            Ok(())
        },
        || execute(&producer_plan(Some("claude"), false), &other).unwrap(),
    );
    assert_eq!(
        disposition_of(&execution.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Conflict
    );
    assert_eq!(
        fs::read_to_string(other.join("CLAUDE.md")).unwrap(),
        recreated
    );
    assert!(
        tree(&other)
            .iter()
            .any(|(path, bytes)| path.ends_with(".render-prev")
                && String::from_utf8_lossy(bytes).contains("owner line"))
    );
}

#[cfg(unix)]
#[test]
fn reconciliation_reports_uninspectable_outputs_as_unconfirmed() {
    let (_directory, root) = temp_root();
    let plan = producer_plan(None, false);
    let operation_id = format_render_operation_id(OPERATION);
    let recorded = std::rc::Rc::new(RefCell::new(None));
    let sink = recorded.clone();
    run_interrupted_at("publish:AGENTS.md", || {
        let mut hook = |record: &PreflightRecord| -> Result<()> {
            *sink.borrow_mut() = Some(record.clone());
            Ok(())
        };
        execute_project_render_plan_with(
            &plan,
            &root,
            &scope(),
            ExpectedRenderAuthority::Producer {
                operation_id: &operation_id,
            },
            ExecuteOptions {
                lock_timeout: Duration::from_millis(200),
                issued_at_ms: None,
                before_publish: Some(&mut hook),
            },
        )
    });
    let record = recorded.borrow().clone().unwrap();
    // The second output becomes unsafe before the retry.
    let (_outside_directory, outside) = temp_root();
    std::os::unix::fs::symlink(outside.join("elsewhere.md"), root.join("AGENTS.md")).unwrap();

    let reconciled = reconcile_interrupted_render(
        &plan,
        &root,
        &scope(),
        ExpectedRenderAuthority::Producer {
            operation_id: &operation_id,
        },
        &record,
        Duration::from_millis(200),
    )
    .expect("an uninspectable output does not discard the evidence of written ones");
    reconciled.receipt.validate_against(&plan).unwrap();
    assert!(reconciled.receipt.incomplete);
    assert_eq!(
        reconciled.receipt.outcome(),
        ProjectRenderOutcomeV1::Partial
    );
    assert_eq!(
        disposition_of(&reconciled.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Written
    );
    assert_eq!(
        disposition_of(&reconciled.receipt, "AGENTS.md"),
        ProjectRenderDispositionV1::Conflict
    );
}

#[test]
fn a_write_after_the_final_check_stays_reachable_in_the_render_backups() {
    use std::io::Write as _;
    let (_directory, root) = temp_root();
    let original = "<!-- Generated by blackbox -->\nprevious projection\n";
    fs::write(root.join("CLAUDE.md"), original).unwrap();
    let mut editor = fs::OpenOptions::new()
        .append(true)
        .open(root.join("CLAUDE.md"))
        .unwrap();
    // The editor writes after the renderer's last read of the moved
    // original and before that original is disposed of.
    let execution = with_interleave(
        move |point, _| {
            if point == "before_backup" {
                editor
                    .write_all(b"owner line after the final check\n")
                    .unwrap();
            }
            Ok(())
        },
        || execute(&producer_plan(Some("claude"), false), &root).unwrap(),
    );
    assert_eq!(
        disposition_of(&execution.receipt, "CLAUDE.md"),
        ProjectRenderDispositionV1::Written
    );
    let backups = tree(&root)
        .into_iter()
        .filter(|(path, _)| path.starts_with(".bbox/local/render-backups/"))
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    assert!(
        String::from_utf8_lossy(&backups[0].1).contains("owner line after the final check"),
        "the edit is still reachable"
    );
    assert!(
        tree(&root)
            .iter()
            .all(|(path, _)| !path.ends_with(".render-prev") || path.contains("render-backups")),
        "nothing is left beside the target"
    );
}

#[test]
fn render_backups_are_bounded() {
    let (_directory, root) = temp_root();
    let plan_a = plan_issued_at("BACKUP_A", 41, 1_000);
    let plan_b = plan_issued_at("BACKUP_B", 42, 1_000);
    for round in 0..(MAX_RENDER_BACKUPS + 5) {
        let plan = if round % 2 == 0 { &plan_a } else { &plan_b };
        execute_operation(plan, &root).unwrap();
    }
    let backups = fs::read_dir(root.join(".bbox/local/render-backups"))
        .unwrap()
        .count();
    assert_eq!(backups, MAX_RENDER_BACKUPS);
}
