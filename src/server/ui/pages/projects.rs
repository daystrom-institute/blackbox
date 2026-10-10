//! `/projects` and `/projects/{id}`: the project catalog and one project's
//! attachments, publisher status, transport ownership and graphs, read
//! through the same internal readers as `bbox_project_list`,
//! `bbox_project_catalog_list`, `bbox_project_catalog_get`,
//! `bbox_project_publisher_status` and `bbox_project_graph_describe`.
//! `/fragments/projects` and `/fragments/projects/{id}` serve the same
//! renderers without the shell.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use bbox_corpus_core::project_catalog::{
    AttachmentStatus, CheckoutAttachment, CorpusProject, ProjectId, ProjectScope,
};
use maud::{Markup, html};

use super::super::shell::{Section, empty_state, error_fragment, layout};
use super::super::{Principal, UiState, html_response};
use super::readouts::{kv_table, rfc3339_age, serde_label, source, state_label, unix_age};
use crate::project_graph_read::{GraphDescription, GraphSummary};
use crate::server::state::ProjectRuntimeStatus;

/// One catalog project as the list renders it.
struct CatalogRow {
    project_id: String,
    /// What a person calls the project: an accepted alias, else the catalog
    /// display name when it is not the id, else the registered root's
    /// directory name. `None` when the catalog knows it by id alone.
    label: Option<String>,
    scope: Markup,
    active_attachments: usize,
    total_attachments: usize,
    /// The accepted publication state, `None` when the runtime has no view
    /// of this project.
    publisher: Option<&'static str>,
    last_activation: Option<u64>,
}

/// A registered root as `bbox_project_list` reports it. The recorded path
/// is rendered as text only, the way that tool already publishes it; the
/// page never opens it, and the read is listed in the catalog ownership
/// inventory to go with `ProjectRecord`.
#[derive(Clone)]
struct RootRow {
    project_id: String,
    repo_id: Option<String>,
    is_git_repo: bool,
    registered_at: String,
    aliases: Vec<String>,
    root: String,
    /// The root's last path component, the name a person knows it by.
    name: Option<String>,
}

/// The registered roots, ordered by project id.
fn registered_roots(ui: &UiState) -> Vec<RootRow> {
    let snapshot = ui.shared.records_provider.records_snapshot();
    let mut rows: Vec<_> = snapshot
        .records
        .iter()
        .map(|record| RootRow {
            project_id: record.project_id.clone(),
            repo_id: record.repo_id.clone(),
            is_git_repo: record.is_git_repo,
            registered_at: record.registered_at.clone(),
            aliases: record.aliases.iter().cloned().collect(),
            root: record.canonical_path.clone(),
            name: std::path::Path::new(&record.canonical_path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
        })
        .collect();
    rows.sort_by(|a, b| a.project_id.cmp(&b.project_id));
    rows
}

struct CatalogData {
    /// `None` while the version-1 registry is the runtime authority.
    catalog: Option<Result<Vec<CatalogRow>, String>>,
    /// The code-source activation records could not be read.
    activation_error: Option<String>,
    /// `bbox_project_list`: registered roots with an attached path.
    records: Vec<RootRow>,
}

impl CatalogData {
    /// Blocking: the activation records and publication status are files.
    fn read(ui: &UiState) -> Self {
        let shared = &ui.shared;
        let (activations, activation_error) = activation_times(ui);
        let records = registered_roots(ui);
        let root_names: BTreeMap<&str, &str> = records
            .iter()
            .filter_map(|record| Some((record.project_id.as_str(), record.name.as_deref()?)))
            .collect();
        let catalog = shared.project_authority.catalog_store().map(|store| {
            let state = store.snapshot().map_err(|error| error.to_string())?;
            let mut counts = BTreeMap::<&ProjectId, (usize, usize)>::new();
            for row in state.attachments().attachments.values() {
                let entry = counts.entry(&row.project_id).or_default();
                entry.1 += 1;
                if row.status == AttachmentStatus::Attached {
                    entry.0 += 1;
                }
            }
            let mut rows: Vec<_> = state
                .catalog()
                .projects
                .values()
                .map(|project| {
                    let id = project.project_id.as_str();
                    let (active, total) =
                        counts.get(&project.project_id).copied().unwrap_or((0, 0));
                    CatalogRow {
                        project_id: id.to_string(),
                        label: project_label(project, root_names.get(id).copied()),
                        scope: scope_markup(&project.scope),
                        active_attachments: active,
                        total_attachments: total,
                        publisher: shared
                            .project_runtime_status(id)
                            .map(|status| status.accepted.state),
                        last_activation: activations.get(id).copied(),
                    }
                })
                .collect();
            rows.sort_by(|a, b| {
                a.label
                    .as_deref()
                    .unwrap_or(&a.project_id)
                    .to_lowercase()
                    .cmp(&b.label.as_deref().unwrap_or(&b.project_id).to_lowercase())
                    .then_with(|| a.project_id.cmp(&b.project_id))
            });
            Ok(rows)
        });
        Self {
            catalog,
            activation_error,
            records,
        }
    }
}

/// The latest code-source activation time per project.
fn activation_times(ui: &UiState) -> (BTreeMap<String, u64>, Option<String>) {
    match ui.shared.code_sources.store().activation_records_mixed() {
        Ok(records) => (
            records
                .iter()
                .map(|record| {
                    (
                        record.project_id().to_string(),
                        record.activated_unix_secs(),
                    )
                })
                .collect(),
            None,
        ),
        Err(error) => (BTreeMap::new(), Some(format!("{error:#}"))),
    }
}

/// The scope as a person reads it: the path inside the repository first,
/// the repository identity shortened with the full value on hover.
fn scope_markup(scope: &ProjectScope) -> Markup {
    match scope {
        ProjectScope::Published(scope) => {
            let relpath = scope.bbox_root_relpath();
            let repo = scope.repo_id();
            html! {
                @if relpath == "." { "repository root" } @else { code { (relpath) } }
                " in repo "
                code title=(repo) { (short_id(repo)) }
            }
        }
        ProjectScope::LegacyLocal => html! { "legacy local" },
        ProjectScope::Connector(scope) => html! {
            (scope.connector_kind().as_str()) " connector "
            code { (scope.connector_source_id().as_str()) }
        },
    }
}

/// The first twelve characters of a long identifier; shorter ones as is.
fn short_id(id: &str) -> &str {
    match id.char_indices().nth(12) {
        Some((end, _)) if id.len() > 16 => &id[..end],
        _ => id,
    }
}

/// The name a person uses for a catalog project.
fn project_label(project: &CorpusProject, root_name: Option<&str>) -> Option<String> {
    if let Some(alias) = project.operator_aliases.iter().next() {
        return Some(alias.clone());
    }
    if project.display_name != project.project_id.as_str() {
        return Some(project.display_name.clone());
    }
    root_name.map(str::to_string)
}

/// The project cell: the label first, the id beneath it.
fn project_cell(project_id: &str, label: Option<&str>) -> Markup {
    html! {
        a href=(format!("/projects/{project_id}")) {
            @match label {
                Some(label) => { strong { (label) } }
                None => { span.muted { "unnamed" } }
            }
        }
        br;
        code.muted title=(project_id) { (short_id(project_id)) }
    }
}

/// An accepted publication state as a verdict.
fn publisher_label(state: &str) -> Markup {
    let class = match state {
        "current" => "running",
        "prior" => "waiting",
        "missing" => "finished",
        "corrupt" => "failed",
        _ => "stale",
    };
    state_label(class, state)
}

/// `GET /projects`
pub(crate) async fn page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let content = catalog_markup(ui.clone()).await;
    html_response(
        StatusCode::OK,
        layout(
            &ui.shell(&principal),
            Some(Section::Projects),
            "Projects",
            content,
        ),
    )
}

/// `GET /fragments/projects`
pub(crate) async fn fragment(State(ui): State<Arc<UiState>>) -> Response {
    html_response(StatusCode::OK, catalog_markup(ui).await)
}

async fn catalog_markup(ui: Arc<UiState>) -> Markup {
    match tokio::task::spawn_blocking(move || CatalogData::read(&ui)).await {
        Ok(data) => render_catalog(&data),
        Err(error) => error_fragment("bbox_project_catalog_list", &error.to_string()),
    }
}

fn render_catalog(data: &CatalogData) -> Markup {
    html! {
        div #projects-catalog {
            section.region aria-labelledby="projects-catalog-title" {
                h2 #projects-catalog-title { "Catalog" }
                @if let Some(error) = &data.activation_error {
                    (error_fragment("code-source activation records", error))
                }
                @match &data.catalog {
                    None => {
                        (empty_state(
                            "The project catalog is not this daemon's runtime authority; the registered roots below are its project set.",
                            "bbox_project_catalog_list",
                        ))
                    }
                    Some(Err(error)) => { (error_fragment("bbox_project_catalog_list", error)) }
                    Some(Ok(rows)) if rows.is_empty() => {
                        (empty_state("The catalog holds no projects.", "bbox_project_catalog_list"))
                    }
                    Some(Ok(rows)) => {
                        table {
                            thead {
                                tr {
                                    th scope="col" { "Project" }
                                    th scope="col" { "Scope" }
                                    th.num scope="col" { "Attachments" }
                                    th scope="col" { "Publisher" }
                                    th scope="col" { "Last activation" }
                                }
                            }
                            tbody {
                                @for row in rows {
                                    tr {
                                        td { (project_cell(&row.project_id, row.label.as_deref())) }
                                        td { (row.scope) }
                                        td.num title=(format!("{} attached of {} recorded", row.active_attachments, row.total_attachments)) {
                                            (row.active_attachments) " / " (row.total_attachments)
                                        }
                                        td {
                                            @match row.publisher {
                                                Some(state) => { (publisher_label(state)) }
                                                None => { span.muted { "no runtime view" } }
                                            }
                                        }
                                        td {
                                            @match row.last_activation {
                                                Some(at) => { (unix_age(at)) }
                                                None => { span.muted { "never" } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        (source("bbox_project_catalog_list, bbox_project_publisher_status"))
                    }
                }
            }
            section.region aria-labelledby="projects-roots-title" {
                h2 #projects-roots-title { "Registered roots" }
                @if data.records.is_empty() {
                    (empty_state("No project roots are registered.", "bbox_project_list"))
                } @else {
                    table {
                        thead {
                            tr {
                                th scope="col" { "Project" }
                                th scope="col" { "Root" }
                                th scope="col" { "Aliases" }
                                th scope="col" { "Repository" }
                                th scope="col" { "Registered" }
                            }
                        }
                        tbody {
                            @for record in &data.records {
                                tr {
                                    td { (project_cell(&record.project_id, record.aliases.first().map(String::as_str).or(record.name.as_deref()))) }
                                    td { code { (record.root) } }
                                    td {
                                        @for alias in &record.aliases { code { (alias) } " " }
                                        @if record.aliases.is_empty() { span.muted { "none" } }
                                    }
                                    td {
                                        @match &record.repo_id {
                                            Some(repo) => { code title=(repo) { (short_id(repo)) } }
                                            None if record.is_git_repo => { "git, no repository id" }
                                            None => { span.muted { "not a git repository" } }
                                        }
                                    }
                                    td { (rfc3339_age(&record.registered_at)) }
                                }
                            }
                        }
                    }
                    (source("bbox_project_list"))
                }
            }
        }
    }
}

/// How a project is known to this daemon.
enum Identity {
    Catalog {
        project: CorpusProject,
        attachments: Vec<CheckoutAttachment>,
        default_attachment: Option<String>,
        epoch: u64,
    },
    Record(RootRow),
}

/// One visible graph and, when its description read, its schema and
/// retrieval facts.
struct GraphRow {
    summary: GraphSummary,
    description: Option<GraphDescription>,
}

struct DetailData {
    project_id: String,
    /// See `project_label`; the id when nothing names the project.
    label: String,
    /// The registered root for this id, when one is recorded.
    root: Option<RootRow>,
    identity: Identity,
    runtime: Option<ProjectRuntimeStatus>,
    activation: Result<Option<(u64, String, u64)>, String>,
    knowledge_transport: bool,
    history_transport: Result<bool, String>,
    graphs: Result<Vec<GraphRow>, String>,
}

/// The outcome of resolving a project id for the detail view.
enum Detail {
    Found(Box<DetailData>),
    Unknown,
    Failed(String),
}

impl Detail {
    /// Blocking: catalog, publication, activation and graph reads.
    fn read(ui: &UiState, id: &str) -> Self {
        let shared = &ui.shared;
        let identity = match shared.project_authority.catalog_store() {
            Some(store) => {
                let Ok(project_id) = ProjectId::parse(id.to_string()) else {
                    return Detail::Unknown;
                };
                let state = match store.snapshot() {
                    Ok(state) => state,
                    Err(error) => return Detail::Failed(error.to_string()),
                };
                let Some(project) = state.catalog().projects.get(&project_id).cloned() else {
                    return Detail::Unknown;
                };
                let mut attachments: Vec<_> = state
                    .attachments()
                    .attachments
                    .values()
                    .filter(|row| row.project_id == project_id)
                    .cloned()
                    .collect();
                attachments.sort_by(|a, b| a.attachment_id.cmp(&b.attachment_id));
                Identity::Catalog {
                    project,
                    attachments,
                    default_attachment: state
                        .attachments()
                        .default_attachments
                        .get(&project_id)
                        .map(|id| id.as_str().to_string()),
                    epoch: state.epoch(),
                }
            }
            None => {
                match registered_roots(ui)
                    .into_iter()
                    .find(|record| record.project_id == id)
                {
                    Some(record) => Identity::Record(record),
                    None => return Detail::Unknown,
                }
            }
        };
        let activation = ui
            .shared
            .code_sources
            .store()
            .activation_records_mixed()
            .map(|records| {
                records
                    .iter()
                    .find(|record| record.project_id() == id)
                    .map(|record| {
                        (
                            record.activated_unix_secs(),
                            record.generation_id().to_string(),
                            record.document_count(),
                        )
                    })
            })
            .map_err(|error| format!("{error:#}"));
        let graphs = read_graphs(ui, id);
        let root = registered_roots(ui)
            .into_iter()
            .find(|record| record.project_id == id);
        let label = match &identity {
            Identity::Catalog { project, .. } => {
                project_label(project, root.as_ref().and_then(|root| root.name.as_deref()))
            }
            Identity::Record(record) => record
                .aliases
                .first()
                .cloned()
                .or_else(|| record.name.clone()),
        }
        .unwrap_or_else(|| id.to_string());
        Detail::Found(Box::new(DetailData {
            project_id: id.to_string(),
            label,
            root,
            identity,
            runtime: shared.project_runtime_status(id),
            activation,
            knowledge_transport: shared.knowledge_transport_cutover.covers_project_str(id),
            history_transport:
                crate::server::history_activation::state_transport_owns_project_history(shared, id)
                    .map_err(|error| format!("{error:#}")),
            graphs,
        }))
    }
}

/// `bbox_project_graph_list` and `bbox_project_graph_describe` for every
/// visible graph of the project.
fn read_graphs(ui: &UiState, id: &str) -> Result<Vec<GraphRow>, String> {
    let (summaries, _) = ui
        .server
        .project_graph_inventory_domain(Some(id))
        .map_err(|error| format!("{error:#}"))?;
    let mut described = BTreeMap::<String, Vec<GraphDescription>>::new();
    for summary in &summaries {
        if !described.contains_key(&summary.graph_id) {
            let descriptions = ui
                .server
                .project_graph_describe_domain(id, &summary.graph_id)
                .map(|(descriptions, _)| descriptions)
                .unwrap_or_default();
            described.insert(summary.graph_id.clone(), descriptions);
        }
    }
    Ok(summaries
        .into_iter()
        .map(|summary| {
            let description = described.get_mut(&summary.graph_id).and_then(|variants| {
                variants
                    .iter()
                    .position(|variant| {
                        variant.summary.source == summary.source
                            && variant.summary.content_hash == summary.content_hash
                    })
                    .map(|index| variants.swap_remove(index))
            });
            GraphRow {
                summary,
                description,
            }
        })
        .collect())
}

/// `GET /projects/{id}`
pub(crate) async fn detail_page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let reader_ui = ui.clone();
    let reader_id = id.clone();
    let (title, content) =
        match tokio::task::spawn_blocking(move || Detail::read(&reader_ui, &reader_id)).await {
            Ok(Detail::Found(data)) => (format!("Project {}", data.label), render_detail(&data)),
            Ok(Detail::Unknown) => {
                return super::placeholder::not_found(State(ui), Extension(principal)).await;
            }
            Ok(Detail::Failed(error)) => (
                format!("Project {id}"),
                error_fragment("bbox_project_catalog_get", &error),
            ),
            Err(error) => (
                format!("Project {id}"),
                error_fragment("bbox_project_catalog_get", &error.to_string()),
            ),
        };
    html_response(
        StatusCode::OK,
        layout(
            &ui.shell(&principal),
            Some(Section::Projects),
            &title,
            content,
        ),
    )
}

/// `GET /fragments/projects/{id}`
pub(crate) async fn detail_fragment(
    State(ui): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || Detail::read(&ui, &id)).await;
    match result {
        Ok(Detail::Found(data)) => html_response(StatusCode::OK, render_detail(&data)),
        Ok(Detail::Unknown) => html_response(
            StatusCode::NOT_FOUND,
            empty_state("No project has this id.", "bbox_project_catalog_get"),
        ),
        Ok(Detail::Failed(error)) => html_response(
            StatusCode::OK,
            error_fragment("bbox_project_catalog_get", &error),
        ),
        Err(error) => html_response(
            StatusCode::OK,
            error_fragment("bbox_project_catalog_get", &error.to_string()),
        ),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn render_detail(data: &DetailData) -> Markup {
    html! {
        div #project-detail {
            p { a href="/projects" { "All projects" } }
            div.panels {
                section.panel aria-labelledby="project-identity" {
                    h2 #project-identity { "Identity" }
                    (identity_table(data))
                }
                section.panel aria-labelledby="project-transport" {
                    h2 #project-transport { "Transport ownership" }
                    (transport_table(data))
                }
                section.panel aria-labelledby="project-publisher" {
                    h2 #project-publisher { "Publisher" }
                    (publisher_panel(data.runtime.as_ref()))
                }
            }
            section.region aria-labelledby="project-attachments" {
                h2 #project-attachments { "Attachments" }
                (attachments_table(data))
            }
            section.region aria-labelledby="project-graphs" {
                h2 #project-graphs { "Graphs" }
                (graphs_table(&data.graphs))
            }
        }
    }
}

fn identity_table(data: &DetailData) -> Markup {
    let mut rows = vec![(
        "Project id".to_string(),
        html! { code { (data.project_id) } },
    )];
    match &data.identity {
        Identity::Catalog { project, epoch, .. } => {
            rows.push(("Display name".into(), html! { (project.display_name) }));
            rows.push(("Scope".into(), scope_markup(&project.scope)));
            rows.push((
                "Repository history".into(),
                match &project.repo_history {
                    Some(history) => html! { code { (history.as_str()) } },
                    None => html! { span.muted { "none" } },
                },
            ));
            rows.push(("Created".into(), rfc3339_age(&project.created_at)));
            rows.push((
                "Aliases".into(),
                html! {
                    @for alias in &project.operator_aliases { code { (alias) } " " }
                    @for alias in &project.nominated_aliases { code { (alias) } " (pending) " }
                    @if project.operator_aliases.is_empty() && project.nominated_aliases.is_empty() {
                        span.muted { "none" }
                    }
                },
            ));
            rows.push(("Catalog epoch".into(), html! { (epoch) }));
            if let Some(root) = &data.root {
                rows.push(("Registered root".into(), html! { code { (root.root) } }));
            }
        }
        Identity::Record(record) => {
            rows.push(("Root".into(), html! { code { (record.root) } }));
            rows.push((
                "Repository".into(),
                match &record.repo_id {
                    Some(repo) => html! { code { (repo) } },
                    None => html! { span.muted { "none" } },
                },
            ));
            rows.push(("Registered".into(), rfc3339_age(&record.registered_at)));
            rows.push((
                "Aliases".into(),
                html! {
                    @for alias in &record.aliases { code { (alias) } " " }
                    @if record.aliases.is_empty() { span.muted { "none" } }
                },
            ));
        }
    }
    match &data.activation {
        Ok(Some((at, generation, documents))) => rows.push((
            "Last code activation".into(),
            html! { (unix_age(*at)) ", generation " code { (generation) } ", " (documents) " documents" },
        )),
        Ok(None) => rows.push((
            "Last code activation".into(),
            html! { span.muted { "never" } },
        )),
        Err(error) => rows.push((
            "Last code activation".into(),
            error_fragment("code-source activation records", error),
        )),
    }
    html! {
        (kv_table(rows))
        (source("bbox_project_catalog_get, bbox_project_list"))
    }
}

fn transport_table(data: &DetailData) -> Markup {
    html! {
        (kv_table(vec![
            (
                "Knowledge transport".into(),
                html! {
                    @if data.knowledge_transport { (state_label("running", "transport-owned")) }
                    @else { "checkout-owned" }
                },
            ),
            (
                "Git history".into(),
                match &data.history_transport {
                    Ok(true) => html! { (state_label("running", "transport-owned")) },
                    Ok(false) => html! { "checkout-owned" },
                    Err(error) => error_fragment("history transport ownership", error),
                },
            ),
        ]))
        (source("knowledge transport cutover, history activation journal"))
    }
}

fn publisher_panel(runtime: Option<&ProjectRuntimeStatus>) -> Markup {
    let Some(status) = runtime else {
        return empty_state(
            "No accepted-publication view: the project catalog is not this daemon's runtime authority.",
            "bbox_project_publisher_status",
        );
    };
    let accepted = &status.accepted;
    let candidate = &status.last_candidate;
    let optional = |value: &Option<String>| match value {
        Some(value) => html! { code { (value) } },
        None => html! { span.muted { "none" } },
    };
    html! {
        (kv_table(vec![
            ("Catalog authority".into(), html! { (status.catalog_authority) }),
            ("Accepted state".into(), publisher_label(accepted.state)),
            ("Serves published content".into(), html! { (yes_no(accepted.serves_published_content)) }),
            ("Scope agreement".into(), html! { (accepted.scope_agreement) }),
            ("Ref".into(), optional(&accepted.full_ref)),
            ("Accepted commit".into(), optional(&accepted.accepted_commit)),
            ("Generation".into(), optional(&accepted.generation_id)),
            (
                "Pointer written".into(),
                match accepted.pointer_written_unix_secs {
                    Some(at) => unix_age(at),
                    None => html! { span.muted { "never" } },
                },
            ),
            ("Binding".into(), html! { (status.binding.status) }),
            (
                "Last candidate".into(),
                html! {
                    (candidate.status)
                    @if let Some(state) = candidate.state { ", " (state) }
                    @if let Some(at) = candidate.created_unix_secs { ", " (unix_age(at)) }
                    @if let Some(diagnostic) = &candidate.diagnostic { br; span.finding-next { (diagnostic) } }
                },
            ),
            (
                "Watcher".into(),
                html! {
                    @if status.watcher.watcher_running { (state_label("running", "running")) }
                    @else { span.muted { "not running in this process" } }
                    @if !status.watcher.capable_but_unregistered.is_empty() {
                        br; (state_label("waiting", "unregistered"))
                        " " (status.watcher.capable_but_unregistered.join(", "))
                    }
                },
            ),
        ]))
        @if let Some(diagnostic) = &accepted.diagnostic {
            p { (state_label("failed", "diagnostic")) " " code { (diagnostic) } }
        }
        (source("bbox_project_publisher_status"))
    }
}

fn attachments_table(data: &DetailData) -> Markup {
    let Identity::Catalog {
        attachments,
        default_attachment,
        ..
    } = &data.identity
    else {
        return empty_state(
            "A registered root carries no catalog attachments.",
            "bbox_project_catalog_get",
        );
    };
    if attachments.is_empty() {
        return empty_state(
            "No checkout is attached to this project.",
            "bbox_project_catalog_get",
        );
    }
    let capabilities = |attachment_id: &str| {
        data.runtime
            .as_ref()
            .and_then(|status| {
                status
                    .attachments
                    .iter()
                    .find(|row| row.attachment_id == attachment_id)
            })
            .map(|row| row.available.join(", "))
    };
    html! {
        table {
            thead {
                tr {
                    th scope="col" { "Attachment" }
                    th scope="col" { "Status" }
                    th scope="col" { "Kind" }
                    th scope="col" { "Checkout" }
                    th scope="col" { "Project root" }
                    th scope="col" { "Branch" }
                    th scope="col" { "Capabilities" }
                    th scope="col" { "Attached" }
                }
            }
            tbody {
                @for row in attachments {
                    @let id = row.attachment_id.as_str();
                    tr {
                        td {
                            code { (id) }
                            @if default_attachment.as_deref() == Some(id) { " " span.muted { "(default)" } }
                        }
                        td {
                            @match row.status {
                                AttachmentStatus::Attached => { (state_label("running", "attached")) }
                                _ => { (state_label("finished", &serde_label(&row.status))) }
                            }
                        }
                        td { (serde_label(&row.kind)) }
                        td { code { (row.checkout_id) } }
                        td { code { (row.project_root_relpath) } }
                        td { (row.branch_ref.as_deref().unwrap_or("none")) }
                        td {
                            @match capabilities(id) {
                                Some(names) if !names.is_empty() => { (names) }
                                _ => { span.muted { "none recorded" } }
                            }
                        }
                        td { (rfc3339_age(&row.attached_at)) }
                    }
                }
            }
        }
        p.panel-source {
            "Recorded host-local attachment rows, not proof of live checkout access. Source: "
            code { "bbox_project_catalog_get" }
        }
    }
}

fn graphs_table(graphs: &Result<Vec<GraphRow>, String>) -> Markup {
    let rows = match graphs {
        Ok(rows) => rows,
        Err(error) => return error_fragment("bbox_project_graph_describe", error),
    };
    if rows.is_empty() {
        return empty_state(
            "No project graph is visible for this project.",
            "bbox_project_graph_list",
        );
    }
    html! {
        table {
            thead {
                tr {
                    th scope="col" { "Graph" }
                    th scope="col" { "Source" }
                    th scope="col" { "Status" }
                    th.num scope="col" { "Vertices" }
                    th.num scope="col" { "Edges" }
                    th scope="col" { "Schema" }
                    th scope="col" { "Retrieval" }
                }
            }
            tbody {
                @for row in rows {
                    @let summary = &row.summary;
                    tr {
                        td { code { (summary.graph_id) } }
                        td { (summary.source) }
                        td { (summary.status) }
                        td.num { (summary.vertex_count) }
                        td.num { (summary.edge_count) }
                        @match &row.description {
                            Some(description) => {
                                td {
                                    @match &description.schema {
                                        Some(schema) => {
                                            code { (schema.schema_id) } " v" (schema.schema_version)
                                            ", " (schema.vertex_type_count) " vertex and "
                                            (schema.edge_type_count) " edge types"
                                        }
                                        None => { span.muted { "no schema" } }
                                    }
                                }
                                td {
                                    @if description.retrieval.indexable {
                                        (description.retrieval.indexed_vertex_count) " indexed"
                                    } @else {
                                        span.muted { "not indexed" }
                                    }
                                }
                            }
                            None => {
                                td { span.muted { "unavailable" } }
                                td { span.muted { "unavailable" } }
                            }
                        }
                    }
                }
            }
        }
        (source("bbox_project_graph_describe"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::StatusCode;

    use super::super::readouts::test_support::{PRINCIPAL, get};
    use crate::server::SharedState;
    use crate::server::state::catalog_fixture::CatalogFixture;

    const ATTACHED: &str = "p_ui_attached";
    const REMOTE: &str = "p_ui_remote";
    const ATTACHMENT: &str = "att_00000000000000000000000000000a01";

    /// A catalog daemon with one attached and one remote-only project.
    fn seeded_catalog() -> (CatalogFixture, Arc<SharedState>) {
        let fixture = CatalogFixture::new();
        let attached_scope = CatalogFixture::scope(".");
        fixture.add_published_project(ATTACHED, &attached_scope);
        fixture.attach_checkout(
            ATTACHED,
            &attached_scope,
            &fixture.root().join("never-mounted"),
            ATTACHMENT,
        );
        fixture.add_published_project(REMOTE, &CatalogFixture::scope("remote"));
        let shared = fixture.server().state.clone();
        (fixture, shared)
    }

    #[tokio::test]
    async fn catalog_page_lists_every_project_with_attachments_and_publisher_state() {
        let (_fixture, shared) = seeded_catalog();
        let (status, body) = get(shared, "/projects").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("<h1>Projects</h1>"), "{body}");
        assert!(body.contains("href=\"/projects\" aria-current=\"page\""));
        for project in [ATTACHED, REMOTE] {
            assert!(
                body.contains(&format!("href=\"/projects/{project}\"")),
                "the catalog must link {project}: {body}"
            );
        }
        assert!(body.contains("repository root"), "{body}");
        assert!(body.contains("<code>remote</code>"), "{body}");
        assert!(body.contains("title=\"repo_example\""), "{body}");
        assert!(body.contains("1 / 1"), "{body}");
        assert!(body.contains("0 / 0"), "{body}");
        assert!(body.contains("class=\"state state-"), "{body}");
        assert!(body.contains("bbox_project_catalog_list"));
        assert!(!body.contains("is not this daemon's runtime authority"));
    }

    #[tokio::test]
    async fn project_page_shows_attachments_publisher_transport_and_graphs() {
        let (_fixture, shared) = seeded_catalog();
        let (status, body) = get(shared, &format!("/projects/{ATTACHED}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains(&format!("Project {ATTACHED}")), "{body}");
        for heading in [
            "Identity",
            "Transport ownership",
            "Publisher",
            "Attachments",
            "Graphs",
        ] {
            assert!(body.contains(heading), "missing {heading}: {body}");
        }
        assert!(body.contains(ATTACHMENT), "{body}");
        assert!(body.contains("refs/heads/main"), "{body}");
        assert!(body.contains("Accepted state"), "{body}");
        assert!(body.contains("checkout-owned"), "{body}");
        assert!(
            body.contains("No project graph is visible for this project."),
            "{body}"
        );
    }

    #[tokio::test]
    async fn an_unknown_project_is_the_shell_404() {
        let (_fixture, shared) = seeded_catalog();
        for id in ["p_ui_missing", "not a project id"] {
            let uri = format!("/projects/{}", id.replace(' ', "%20"));
            let (status, body) = get(shared.clone(), &uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
            assert!(body.contains("No page lives at this address."), "{body}");
            assert!(body.contains(PRINCIPAL), "a 404 keeps the shell");
        }
        let (status, body) = get(shared, "/fragments/projects/p_ui_missing").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.contains("<html"), "{body}");
    }

    #[tokio::test]
    async fn fragments_render_the_page_regions_without_the_shell() {
        let (_fixture, shared) = seeded_catalog();
        let (status, body) = get(shared.clone(), "/fragments/projects").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("id=\"projects-catalog\""), "{body}");
        assert!(body.contains(REMOTE));
        assert!(!body.contains("<html"));
        let (status, body) = get(shared, &format!("/fragments/projects/{REMOTE}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("id=\"project-detail\""), "{body}");
        assert!(body.contains("No checkout is attached to this project."));
        assert!(!body.contains("<html"));
    }

    #[tokio::test]
    async fn a_bridge_daemon_names_the_inactive_catalog_and_lists_registered_roots() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let (status, body) = get(shared, "/projects").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("is not this daemon's runtime authority"),
            "{body}"
        );
        assert!(body.contains("No project roots are registered."), "{body}");
        assert!(body.contains("bbox_project_list"));
    }
}
