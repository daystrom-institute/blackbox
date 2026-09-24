//! Checkout-owner project render: a catalog daemon with no checkout of its
//! own completes unbound project renders through the collector that owns
//! each checkout. The simulated owner below drives the same render-lane
//! runtime calls the HTTP routes make and applies plans with the shared
//! executor the collector links.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bbox_corpus_core::identity::PublishedScope;
use bbox_project_render::execute::execute_project_render_plan_as;
use bbox_project_render::transport::{
    ExpectedRenderAuthority, PROJECT_RENDER_TRANSPORT_VERSION, ProjectRenderDispositionV1,
    ProjectRenderPlanAssemblerV1, ProjectRenderReceiptV1, ProjectRenderViewV1,
};
use bbox_project_render::wire::{RENDER_LANE_SCHEMA_VERSION, RenderLanePollRequestV1};
use rmcp::handler::server::wrapper::Parameters;
use serde_json::Value;

use crate::knowledge::RenderParams;
use crate::server::BlackboxServer;
use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};
use crate::server::state::catalog_fixture::{
    COMMIT_ONE, COMMIT_TWO, CatalogFixture, knowledge_entry,
};

const UNCOVERED: &str = "p_000000000000000000000000000000b1";
const COVERED: &str = "p_000000000000000000000000000000b2";
const PRODUCER: &str = "producer-owner";

fn scopes() -> (PublishedScope, PublishedScope) {
    (CatalogFixture::scope("."), CatalogFixture::scope("nested"))
}

fn marker_entry(project: &str, id: &str, content: &str) -> crate::knowledge::KnowledgeEntry {
    let mut entry = knowledge_entry(id, content);
    entry.scope = bbox_knowledge::knowledge::Scope::Project;
    entry.project_id = Some(project.into());
    entry
}

struct OwnerFixture {
    fixture: CatalogFixture,
    roots: BTreeMap<PublishedScope, PathBuf>,
}

impl OwnerFixture {
    fn new() -> Self {
        let fixture = CatalogFixture::new();
        let (uncovered, covered) = scopes();
        fixture.add_published_project(UNCOVERED, &uncovered);
        fixture.add_published_project(COVERED, &covered);
        fixture.install_publication(
            UNCOVERED,
            &uncovered,
            COMMIT_ONE,
            &[marker_entry(
                UNCOVERED,
                "owner-uncovered",
                "UNCOVERED_OWNER_MARKER",
            )],
            &[],
        );
        fixture.install_publication(
            COVERED,
            &covered,
            COMMIT_ONE,
            &[marker_entry(
                COVERED,
                "owner-covered",
                "COVERED_OWNER_MARKER",
            )],
            &[],
        );
        let mut roots = BTreeMap::new();
        for (scope, name) in [(uncovered, "owner-uncovered"), (covered, "owner-covered")] {
            let root = fixture.root().join(name);
            std::fs::create_dir_all(&root).unwrap();
            roots.insert(scope, root.canonicalize().unwrap());
        }
        Self { fixture, roots }
    }

    fn root(&self, scope: &PublishedScope) -> &Path {
        &self.roots[scope]
    }

    /// A server whose producer grant maps both scopes to the owner. The
    /// covered project is under the render cutover marker; neither has a
    /// daemon checkout.
    fn server(&self) -> BlackboxServer {
        let server = self.fixture.server_with_render_locality_cutover(COVERED);
        self.install_grant(&server, PRODUCER, true);
        server
    }

    fn install_grant(&self, server: &BlackboxServer, producer: &str, both: bool) {
        let (uncovered, covered) = scopes();
        let mut projects = BTreeMap::from([(uncovered, UNCOVERED.to_string())]);
        if both {
            projects.insert(covered, COVERED.to_string());
        }
        let snapshot = self.fixture.store().snapshot().unwrap();
        let auth = ProducerAuthRuntime::for_test_catalog(
            vec![(
                bro_rpc::ServiceToken::parse("8".repeat(64)).unwrap(),
                ProducerGrant {
                    producer_id: producer.into(),
                    projects,
                },
            )],
            snapshot.catalog(),
        );
        server
            .state
            .code_sources
            .install_auth_for_test(Arc::new(auth));
    }
}

fn grant_of(server: &BlackboxServer, producer: &str) -> BTreeMap<PublishedScope, String> {
    server
        .state
        .code_sources
        .producer_auth()
        .scope_grant_rows()
        .into_iter()
        .filter(|(_, _, owner)| owner == producer)
        .map(|(scope, project, _)| (scope, project))
        .collect()
}

fn poll_request(covered: Vec<PublishedScope>, versions: Vec<u32>) -> RenderLanePollRequestV1 {
    RenderLanePollRequestV1 {
        schema_version: RENDER_LANE_SCHEMA_VERSION,
        render_transport_versions: versions,
        covered_scopes: covered,
        collector_version: "0.0.1-test".into(),
    }
}

/// Announce the owner's render lane without applying anything.
fn announce(server: &BlackboxServer, covered: Vec<PublishedScope>) {
    let grant = grant_of(server, PRODUCER);
    let response = server
        .state
        .render_operations
        .poll(
            PRODUCER,
            &poll_request(covered, vec![PROJECT_RENDER_TRANSPORT_VERSION]),
            &grant,
        )
        .unwrap();
    assert!(
        response.operations.is_empty(),
        "announce found pending work"
    );
}

/// Apply exactly one delivered operation the way the collector does and
/// report its receipt.
fn apply_one(
    server: &BlackboxServer,
    roots: &BTreeMap<PublishedScope, PathBuf>,
    edit: impl Fn(&mut ProjectRenderReceiptV1),
) -> Option<(String, ProjectRenderReceiptV1)> {
    let grant = grant_of(server, PRODUCER);
    let runtime = &server.state.render_operations;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let page = runtime
            .poll(
                PRODUCER,
                &poll_request(
                    roots.keys().cloned().collect(),
                    vec![PROJECT_RENDER_TRANSPORT_VERSION],
                ),
                &grant,
            )
            .unwrap();
        page.validate().unwrap();
        let Some(operation) = page.operations.into_iter().next() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        let mut assembler = ProjectRenderPlanAssemblerV1::default();
        let mut offset = 0;
        let plan = loop {
            let chunk = runtime
                .plan_page(PRODUCER, &operation.operation_id, offset, &grant)
                .unwrap();
            let next = chunk.next_offset;
            if let Some(assembled) = assembler.push(chunk).unwrap() {
                assert_eq!(assembled.plan_sha256, operation.plan_sha256);
                break assembled.plan;
            }
            offset = next.unwrap();
        };
        let mut receipt = execute_project_render_plan_as(
            &plan,
            &roots[&operation.scope],
            &operation.scope,
            ExpectedRenderAuthority::Producer {
                operation_id: &operation.operation_id,
            },
            Duration::from_secs(5),
        )
        .unwrap()
        .receipt;
        edit(&mut receipt);
        runtime
            .settle(
                PRODUCER,
                &operation.operation_id,
                &operation.plan_sha256,
                &grant,
                Ok(receipt.clone()),
            )
            .unwrap();
        return Some((operation.operation_id, receipt));
    }
    None
}

/// Run one bbox_render call while a simulated owner applies its operation.
async fn render_with_owner(
    server: &BlackboxServer,
    owner: &OwnerFixture,
    params: RenderParams,
) -> Value {
    let owner_server = server.clone();
    let roots = owner.roots.clone();
    let applier = std::thread::spawn(move || apply_one(&owner_server, &roots, |_| {}));
    let result = server.bbox_render(Parameters(params)).await;
    let applied = applier.join().unwrap();
    assert!(applied.is_some(), "the owner never received an operation");
    parse(&result)
}

fn text(result: &rmcp::model::CallToolResult) -> String {
    let value = serde_json::to_value(result).unwrap();
    value["content"][0]["text"].as_str().unwrap().to_string()
}

fn parse(result: &rmcp::model::CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{}", text(result));
    serde_json::from_str(&text(result)).unwrap()
}

fn project_params(project: &str) -> RenderParams {
    RenderParams {
        project: Some(project.into()),
        scope: Some("project".into()),
        ..Default::default()
    }
}

fn leases(server: &BlackboxServer) -> u64 {
    server
        .state
        .checkout_access
        .health()
        .operations
        .into_iter()
        .map(|operation| operation.granted + operation.denied)
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbound_callers_render_covered_and_uncovered_remote_projects_through_the_owner() {
    let owner = OwnerFixture::new();
    let server = owner.server();
    let (uncovered, covered) = scopes();
    for (project, scope, marker) in [
        (UNCOVERED, &uncovered, "UNCOVERED_OWNER_MARKER"),
        (COVERED, &covered, "COVERED_OWNER_MARKER"),
    ] {
        let response = render_with_owner(&server, &owner, project_params(project)).await;
        assert_eq!(response["status"], "render_complete", "{response}");
        assert_eq!(response["owner"], PRODUCER);
        assert_eq!(
            response["view"], "published",
            "unbound callers default to published"
        );
        assert_eq!(response["validation"]["status"], "current");
        assert_eq!(response["current"], true);
        assert_eq!(response["outcome"], "converged");
        let receipt: ProjectRenderReceiptV1 =
            serde_json::from_value(response["receipt"].clone()).unwrap();
        assert_eq!(receipt.project_id, project);
        assert_eq!(&receipt.scope, scope);
        assert!(
            !response
                .to_string()
                .contains(owner.root(scope).to_str().unwrap())
        );
        for file in ["CLAUDE.md", "AGENTS.md", "GEMINI.md"] {
            assert!(
                std::fs::read_to_string(owner.root(scope).join(file))
                    .unwrap()
                    .contains(marker)
            );
        }
    }
    assert_eq!(leases(&server), 0, "no daemon checkout was opened");
    let observations = server.state.render_locality_observations.snapshot();
    assert_eq!(observations.completions.len(), 2);
    assert!(
        observations
            .completions
            .iter()
            .all(|completion| completion.view == ProjectRenderViewV1::Published)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_renders_keep_view_provider_project_doc_and_file_semantics() {
    let owner = OwnerFixture::new();
    let server = owner.server();
    let (scope, _) = scopes();
    let root = owner.root(&scope).to_path_buf();

    // Explicit own still requires authoritative checkout context, and the
    // refusal issues no operation.
    announce(&server, owner.roots.keys().cloned().collect());
    let refused = server
        .bbox_render(Parameters(RenderParams {
            provisional: Some("own".into()),
            ..project_params(UNCOVERED)
        }))
        .await;
    assert_eq!(refused.is_error, Some(true));
    assert!(text(&refused).contains("own requires authoritative checkout context"));
    assert_eq!(
        server.state.render_operations.latest_sequence(UNCOVERED),
        None
    );
    // A global render plan applies only to scope=global, owner or not.
    let global_plan = server
        .bbox_render(Parameters(RenderParams {
            global_plan: Some(bbox_knowledge::knowledge::GlobalRenderPlanRequestV1 {
                offset: None,
                plan_sha256: None,
                host_common_target: "/host/.blackbox/BLACKBOX.md".into(),
            }),
            ..project_params(UNCOVERED)
        }))
        .await;
    assert_eq!(global_plan.is_error, Some(true));
    assert_eq!(
        server.state.render_operations.latest_sequence(UNCOVERED),
        None
    );

    let all = render_with_owner(
        &server,
        &owner,
        RenderParams {
            provisional: Some("all".into()),
            provider: Some("claude".into()),
            ..project_params(UNCOVERED)
        },
    )
    .await;
    assert_eq!(all["view"], "all");
    assert!(root.join("CLAUDE.md").is_file());
    assert!(
        !root.join("AGENTS.md").exists(),
        "provider selection is honored"
    );

    // Handwritten files are preserved; generated files are replaced.
    std::fs::write(root.join("AGENTS.md"), "hand-authored agents file\n").unwrap();
    std::fs::write(root.join("PROJECT.md"), "# Project\n").unwrap();
    let preserved = render_with_owner(&server, &owner, project_params(UNCOVERED)).await;
    assert_eq!(preserved["outcome"], "preserved");
    assert_eq!(preserved["dispositions"]["refused"], 1);
    assert_eq!(
        std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
        "hand-authored agents file\n"
    );
    let claude = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(
        claude.contains("PROJECT.md"),
        "PROJECT.md presence is observed by the owner"
    );
    assert_eq!(preserved["receipt"]["project_doc_nonempty"], true);

    // Dry-run previews without writing.
    std::fs::remove_file(root.join("PROJECT.md")).unwrap();
    let dry = render_with_owner(
        &server,
        &owner,
        RenderParams {
            dry_run: Some(true),
            ..project_params(UNCOVERED)
        },
    )
    .await;
    assert_eq!(dry["dispositions"]["written"], Value::Null);
    assert_eq!(
        std::fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
        claude
    );

    // A fresh render with unchanged knowledge restores a deleted output and
    // re-evaluates PROJECT.md.
    std::fs::remove_file(root.join("CLAUDE.md")).unwrap();
    let restored = render_with_owner(&server, &owner, project_params(UNCOVERED)).await;
    assert_eq!(restored["current"], true);
    let restored_claude = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(!restored_claude.contains("PROJECT.md"));
    assert_ne!(restored["operation_id"], preserved["operation_id"]);

    // Recovering the older operation reports its receipt as historical and
    // does not re-apply it.
    let recovered = parse(
        &server
            .bbox_render(Parameters(RenderParams {
                operation: Some(preserved["operation_id"].as_str().unwrap().into()),
                project: Some(UNCOVERED.into()),
                ..Default::default()
            }))
            .await,
    );
    assert_eq!(recovered["operation_id"], preserved["operation_id"]);
    assert_eq!(recovered["current"], false);
    assert!(recovered["detail"].as_str().unwrap().contains("historical"));
    assert_eq!(
        std::fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
        restored_claude
    );
}

/// Receipt hashes are the exact bytes the owner wrote. Satellite hashes are
/// covered by the leaf executor tests: accepted catalog publications carry
/// no render placement, so a published catalog view has no satellites.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_hashes_are_the_owner_written_bytes() {
    let owner = OwnerFixture::new();
    let (scope, _) = scopes();
    let server = owner.server();
    let response = render_with_owner(&server, &owner, project_params(UNCOVERED)).await;
    let receipt: ProjectRenderReceiptV1 =
        serde_json::from_value(response["receipt"].clone()).unwrap();
    assert_eq!(receipt.projections.len(), 3);
    for projection in &receipt.projections {
        let bytes = std::fs::read(owner.root(&scope).join(&projection.file_name)).unwrap();
        assert_eq!(projection.projection_bytes, Some(bytes.len()));
        assert_eq!(
            projection.projection_sha256.as_deref().unwrap(),
            format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&bytes))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_both_renders_global_before_issuing_the_project_half() {
    let owner = OwnerFixture::new();
    let render_root = owner.fixture.root().join("owner-global");
    let mut env = crate::util::TestEnvGuard::new();
    for key in [
        "BLACKBOX_GLOBAL_COMMON_MD",
        "BLACKBOX_GLOBAL_CLAUDE_MD",
        "BLACKBOX_GLOBAL_CODEX_MD",
        "BLACKBOX_GLOBAL_GEMINI_MD",
    ] {
        env.remove(key);
    }
    let server = owner.server();
    announce(&server, owner.roots.keys().cloned().collect());
    let refused = server
        .bbox_render(Parameters(RenderParams {
            project: Some(UNCOVERED.into()),
            ..Default::default()
        }))
        .await;
    assert_eq!(refused.is_error, Some(true));
    assert!(
        text(&refused).contains("error.global_render_authority"),
        "{}",
        text(&refused)
    );
    assert_eq!(
        server.state.render_operations.latest_sequence(UNCOVERED),
        None,
        "a failed global half issues no project operation"
    );

    env.set("BLACKBOX_GLOBAL_COMMON_MD", render_root.join("BLACKBOX.md"));
    env.set("BLACKBOX_GLOBAL_CLAUDE_MD", render_root.join("CLAUDE.md"));
    env.set("BLACKBOX_GLOBAL_CODEX_MD", render_root.join("AGENTS.md"));
    env.set("BLACKBOX_GLOBAL_GEMINI_MD", render_root.join("GEMINI.md"));
    env.set("BLACKBOX_BACKUP_DIR", render_root.join("backups"));
    let response = render_with_owner(
        &server,
        &owner,
        RenderParams {
            project: Some(UNCOVERED.into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(response["status"], "render_complete");
    assert!(
        response["global_result"]
            .as_str()
            .unwrap()
            .contains("BLACKBOX.md")
    );
    assert!(render_root.join("BLACKBOX.md").is_file());
    let (scope, _) = scopes();
    assert!(owner.root(&scope).join("CLAUDE.md").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_failures_refuse_with_setup_and_never_open_a_daemon_checkout() {
    let owner = OwnerFixture::new();
    let (uncovered, covered) = scopes();

    // No grant covers either project: the refusal names the setup, never a
    // session type, and the governed project never reaches a lease.
    let unowned = owner.fixture.server_with_render_locality_cutover(COVERED);
    for project in [UNCOVERED, COVERED] {
        let result = unowned
            .bbox_render(Parameters(project_params(project)))
            .await;
        assert_eq!(result.is_error, Some(true));
        let message = text(&result);
        assert!(
            message.contains("error.render_locality_required"),
            "{message}"
        );
        assert!(message.contains("bbox-code-collector add"), "{message}");
        assert!(!message.contains("bro-harness"), "{message}");
    }
    let granted: u64 = unowned
        .state
        .checkout_access
        .health()
        .operations
        .into_iter()
        .map(|operation| operation.granted)
        .sum();
    assert_eq!(granted, 0);

    let server = owner.server();
    let refusal = |expected: &'static str| {
        let server = server.clone();
        async move {
            let result = server
                .bbox_render(Parameters(project_params(UNCOVERED)))
                .await;
            assert_eq!(result.is_error, Some(true));
            let message = text(&result);
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("bro-harness"), "{message}");
        }
    };
    refusal("error.render_owner_capability").await;
    let grant = grant_of(&server, PRODUCER);
    server
        .state
        .render_operations
        .poll(
            PRODUCER,
            &poll_request(vec![uncovered.clone()], vec![1]),
            &grant,
        )
        .unwrap();
    refusal("error.render_owner_capability").await;
    announce(&server, vec![covered.clone()]);
    refusal("error.render_owner_checkout").await;
    announce(&server, vec![uncovered.clone()]);
    server
        .state
        .render_operations
        .age_lane_for_test(PRODUCER, 10_000);
    refusal("error.render_owner_unavailable").await;
    assert_eq!(
        server.state.render_operations.latest_sequence(UNCOVERED),
        None
    );
    assert_eq!(leases(&server), 0);
    // Render operations never enter the enroll command channel.
    assert!(
        server
            .state
            .producer_commands
            .poll(
                PRODUCER,
                bbox_code_source::ProducerCommandPollRequestV1 {
                    schema_version: bbox_code_source::PRODUCER_COMMAND_SCHEMA_VERSION,
                    presence: bbox_code_source::ProducerPresenceV1 {
                        enroll_roots: vec![],
                        host_label: "owner".into(),
                        config_path: "/etc/collector.toml".into(),
                        service_label: None,
                        collector_version: "0.0.1".into(),
                    },
                },
            )
            .commands
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_render_recovers_its_receipt_after_a_daemon_restart() {
    let owner = OwnerFixture::new();
    let (scope, _) = scopes();
    let server = owner.server();
    server
        .state
        .render_operations
        .set_wait_timeout_for_test(Duration::from_millis(20));
    announce(&server, owner.roots.keys().cloned().collect());
    let pending = parse(
        &server
            .bbox_render(Parameters(project_params(UNCOVERED)))
            .await,
    );
    assert_eq!(pending["status"], "render_pending");
    let operation_id = pending["operation_id"].as_str().unwrap().to_string();
    assert_eq!(pending["recover"]["arguments"]["operation"], operation_id);
    assert!(!owner.root(&scope).join("CLAUDE.md").exists());

    // Restart: a fresh server over the same durable state.
    let restarted = owner.server();
    let (applied, receipt) = apply_one(&restarted, &owner.roots, |_| {}).unwrap();
    assert_eq!(applied, operation_id);
    // A duplicate acknowledgment is idempotent.
    let record = restarted
        .state
        .render_operations
        .record(&operation_id)
        .unwrap();
    assert_eq!(
        restarted
            .state
            .render_operations
            .settle(
                PRODUCER,
                &operation_id,
                &record.plan_sha256,
                &grant_of(&restarted, PRODUCER),
                Ok(receipt.clone()),
            )
            .unwrap()
            .as_str(),
        "already_settled"
    );
    let recovered = parse(
        &restarted
            .bbox_render(Parameters(RenderParams {
                operation: Some(operation_id.clone()),
                project: Some(UNCOVERED.into()),
                ..Default::default()
            }))
            .await,
    );
    assert_eq!(recovered["status"], "render_complete");
    assert_eq!(recovered["current"], true);
    let recovered_receipt: ProjectRenderReceiptV1 =
        serde_json::from_value(recovered["receipt"].clone()).unwrap();
    assert_eq!(recovered_receipt, receipt);
    assert!(owner.root(&scope).join("CLAUDE.md").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn knowledge_changed_before_completion_is_reported_stale_not_converged() {
    let owner = OwnerFixture::new();
    let (scope, _) = scopes();
    let server = owner.server();
    server
        .state
        .render_operations
        .set_wait_timeout_for_test(Duration::from_millis(20));
    announce(&server, owner.roots.keys().cloned().collect());
    let pending = parse(
        &server
            .bbox_render(Parameters(project_params(UNCOVERED)))
            .await,
    );
    let operation_id = pending["operation_id"].as_str().unwrap().to_string();
    owner.fixture.install_publication(
        UNCOVERED,
        &scope,
        COMMIT_TWO,
        &[marker_entry(
            UNCOVERED,
            "owner-uncovered",
            "CHANGED_AFTER_PLAN",
        )],
        &[],
    );
    let changed = owner.server();
    apply_one(&changed, &owner.roots, |_| {}).unwrap();
    let recovered = parse(
        &changed
            .bbox_render(Parameters(RenderParams {
                operation: Some(operation_id),
                project: Some(UNCOVERED.into()),
                ..Default::default()
            }))
            .await,
    );
    assert_eq!(recovered["status"], "render_stale");
    assert_eq!(recovered["current"], false);
    assert!(
        changed
            .state
            .render_locality_observations
            .snapshot()
            .completions
            .is_empty(),
        "a stale completion is never recorded as convergence"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partial_owner_render_is_reported_as_partial() {
    let owner = OwnerFixture::new();
    let server = owner.server();
    server
        .state
        .render_operations
        .set_wait_timeout_for_test(Duration::from_millis(20));
    announce(&server, owner.roots.keys().cloned().collect());
    let pending = parse(
        &server
            .bbox_render(Parameters(project_params(UNCOVERED)))
            .await,
    );
    let operation_id = pending["operation_id"].as_str().unwrap().to_string();
    apply_one(&server, &owner.roots, |receipt| {
        receipt.projections[1].disposition = ProjectRenderDispositionV1::Failed;
    })
    .unwrap();
    let recovered = parse(
        &server
            .bbox_render(Parameters(RenderParams {
                operation: Some(operation_id),
                project: Some(UNCOVERED.into()),
                ..Default::default()
            }))
            .await,
    );
    assert_eq!(recovered["status"], "render_partial");
    assert_eq!(recovered["outcome"], "partial");
    assert_eq!(recovered["dispositions"]["failed"], 1);
}

/// An owner that wrote every output but could not confirm completion
/// reports an incomplete receipt. It is surfaced as partial and is never
/// recorded as completion evidence for the cutover gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incomplete_all_written_receipt_is_never_completion_evidence() {
    let owner = OwnerFixture::new();
    let server = owner.server();
    server
        .state
        .render_operations
        .set_wait_timeout_for_test(Duration::from_millis(20));
    announce(&server, owner.roots.keys().cloned().collect());
    let pending = parse(
        &server
            .bbox_render(Parameters(project_params(UNCOVERED)))
            .await,
    );
    let operation_id = pending["operation_id"].as_str().unwrap().to_string();
    let (_, receipt) = apply_one(&server, &owner.roots, |receipt| {
        receipt.incomplete = true;
    })
    .unwrap();
    assert!(
        receipt
            .projections
            .iter()
            .all(|projection| projection.disposition == ProjectRenderDispositionV1::Written)
    );
    let recovered = parse(
        &server
            .bbox_render(Parameters(RenderParams {
                operation: Some(operation_id),
                project: Some(UNCOVERED.into()),
                ..Default::default()
            }))
            .await,
    );
    assert_eq!(recovered["status"], "render_partial");
    assert!(
        server
            .state
            .render_locality_observations
            .snapshot()
            .completions
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_rechecks_present_validity_of_a_validated_receipt() {
    let owner = OwnerFixture::new();
    let (uncovered, _) = scopes();
    let server = owner.server();
    let recover = |server: BlackboxServer, project: &'static str, operation_id: String| async move {
        parse(
            &server
                .bbox_render(Parameters(RenderParams {
                    operation: Some(operation_id),
                    project: Some(project.into()),
                    ..Default::default()
                }))
                .await,
        )
    };

    let completed = render_with_owner(&server, &owner, project_params(UNCOVERED)).await;
    assert_eq!(completed["validation"]["status"], "current");
    let uncovered_operation = completed["operation_id"].as_str().unwrap().to_string();
    let covered = render_with_owner(&server, &owner, project_params(COVERED)).await;
    let covered_operation = covered["operation_id"].as_str().unwrap().to_string();
    let again = recover(server.clone(), UNCOVERED, uncovered_operation.clone()).await;
    assert_eq!(again["current"], true);

    // Knowledge changes without another render being issued.
    owner.fixture.install_publication(
        UNCOVERED,
        &uncovered,
        COMMIT_TWO,
        &[marker_entry(
            UNCOVERED,
            "owner-uncovered",
            "CHANGED_WITHOUT_A_RENDER",
        )],
        &[],
    );
    let changed = owner.server();
    announce(&changed, owner.roots.keys().cloned().collect());
    let recovered = recover(changed.clone(), UNCOVERED, uncovered_operation).await;
    assert_eq!(recovered["current"], false, "{recovered}");
    assert_eq!(recovered["status"], "render_stale");
    assert_eq!(
        recovered["validation"]["status"], "current",
        "history is kept"
    );
    assert_eq!(recovered["present_validation"]["status"], "stale");

    // Owner authority is revoked without another render being issued.
    owner.install_grant(&changed, PRODUCER, false);
    let revoked = recover(changed.clone(), COVERED, covered_operation).await;
    assert_eq!(revoked["current"], false, "{revoked}");
    assert_eq!(revoked["present_validation"]["status"], "stale");
}
