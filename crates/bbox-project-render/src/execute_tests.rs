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

thread_local! {
    static FAIL_PUBLICATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(super) fn fail_publication_of(file_name: &str) -> bool {
    FAIL_PUBLICATION.with(|failing| failing.borrow().as_deref() == Some(file_name))
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
                || path.starts_with(".bbox/guidance/"),
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
    let disposition = publish_entrypoint(&root, "CLAUDE.md", "new\n", &observed).unwrap();
    assert_eq!(disposition, ProjectRenderDispositionV1::Conflict);
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "<!-- Generated by blackbox -->\nowner edit\n"
    );

    let absent = root.join("AGENTS.md");
    let observed = observe_target(&absent).unwrap();
    fs::write(&absent, "created concurrently\n").unwrap();
    let disposition = publish_entrypoint(&root, "AGENTS.md", "new\n", &observed).unwrap();
    assert_eq!(disposition, ProjectRenderDispositionV1::Conflict);
    assert_eq!(
        fs::read_to_string(&absent).unwrap(),
        "created concurrently\n"
    );
    assert_eq!(tree(&root).len(), 2, "no staged sibling remains");
}

#[test]
fn a_failed_publication_reports_a_partial_render() {
    let (_directory, root) = temp_root();
    FAIL_PUBLICATION.with(|failing| *failing.borrow_mut() = Some("AGENTS.md".into()));
    let plan = producer_plan(None, false);
    let execution = execute(&plan, &root);
    FAIL_PUBLICATION.with(|failing| *failing.borrow_mut() = None);
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
    assert_eq!(tree(&root).len(), 3);
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
