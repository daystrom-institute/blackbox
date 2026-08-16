use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_knowledge_source::KnowledgeSourceLimits;
use bbox_knowledge_source_store::{ReadyProvisionalWorkspace, ReadyPublicationFile};
use bbox_project_graph::{
    EvidenceBindingSet, EvidenceParseLimits, EvidenceValidationError, GraphDocumentBytes,
    GraphGeneration, GraphParseLimits, ValidationError, load_graph_documents,
    parse_evidence_document,
};
use bro_core::WorkspaceId;

use crate::accepted_publication_runtime::VerifiedAcceptedPublication;
use crate::accepted_publication_store::AcceptedGraphSourceV1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectGraphValidity {
    Valid,
    Invalid { errors: Vec<ValidationError> },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProjectGraphGenerationIdentity {
    pub accepted_generation: String,
    pub accepted_commit: String,
    pub source_generation: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct ProjectGraphViewEntry {
    pub graph_id: String,
    pub validity: ProjectGraphValidity,
    pub generation: ProjectGraphGenerationIdentity,
    graph: Option<Arc<GraphGeneration>>,
}

impl ProjectGraphViewEntry {
    pub fn graph(&self) -> Option<&Arc<GraphGeneration>> {
        self.graph.as_ref()
    }

    pub fn valid(
        graph_id: String,
        generation: ProjectGraphGenerationIdentity,
        graph: GraphGeneration,
    ) -> Self {
        Self {
            graph_id,
            validity: ProjectGraphValidity::Valid,
            generation,
            graph: Some(Arc::new(graph)),
        }
    }

    pub fn invalid(
        graph_id: String,
        generation: ProjectGraphGenerationIdentity,
        errors: Vec<ValidationError>,
    ) -> Self {
        Self {
            graph_id,
            validity: ProjectGraphValidity::Invalid { errors },
            generation,
            graph: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProjectGraphOverlayValue {
    Upsert(ProjectGraphViewEntry),
    Tombstone {
        graph_id: String,
        generation: ProjectGraphGenerationIdentity,
    },
}

/// What a checkout's evidence lane does to the accepted binding set.
///
/// There is no partial state: one complete valid document replaces the whole
/// set, an absent document tombstones it, and an invalid one is `Invalid`,
/// which the read plane treats as "keep what was already accepted" while
/// still being able to report why.
#[derive(Debug, Clone)]
pub enum EvidenceOverlayValue {
    Upsert(EvidenceBindingSet),
    Tombstone,
    Invalid {
        errors: Vec<EvidenceValidationError>,
    },
}

#[derive(Debug, Clone)]
pub struct PublishedProjectGraphView {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub graphs: BTreeMap<String, ProjectGraphViewEntry>,
    /// The project's accepted binding set. Empty for a publication written
    /// before the evidence lane existed, which is indistinguishable from a
    /// publication that simply asserts nothing, and correctly so.
    pub evidence: EvidenceBindingSet,
}

#[derive(Debug, Clone)]
pub struct ProvisionalProjectGraphOverlay {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub workspace_id: WorkspaceId,
    pub source_generation_id: String,
    pub graphs: BTreeMap<String, ProjectGraphOverlayValue>,
    /// `None` when the checkout's evidence lane matches its baseline, so the
    /// published set already describes it.
    pub evidence: Option<EvidenceOverlayValue>,
}

/// A connector-managed source graph as the read plane sees it.
///
/// Read-only by construction: these generations are accepted by the source
/// projection store (`bbox-source-graph`), never by a checkout lane, so the
/// catalog offers no path that mutates one and the visibility policy does not
/// gate them the way it gates provisional checkout state.
#[derive(Debug, Clone)]
pub struct ConnectorProjectGraphView {
    pub project_id: ProjectId,
    pub graph_id: String,
    pub source_connector: String,
    pub entry: ProjectGraphViewEntry,
}

#[derive(Debug, Clone)]
pub enum ProjectGraphRead {
    Missing,
    Tombstoned(ProjectGraphGenerationIdentity),
    Valid(ProjectGraphViewEntry),
    Invalid(ProjectGraphViewEntry),
}

#[derive(Debug, Default)]
pub struct ProjectGraphViewCatalog {
    published: BTreeMap<ProjectId, PublishedProjectGraphView>,
    provisional: BTreeMap<(ProjectId, WorkspaceId), ProvisionalProjectGraphOverlay>,
    connector: BTreeMap<(ProjectId, String), ConnectorProjectGraphView>,
}

#[derive(Debug, Clone)]
pub struct ProjectGraphTreeValidation {
    pub graph_count: usize,
    pub errors: Vec<ValidationError>,
}

impl ProjectGraphViewCatalog {
    pub fn install_published(&mut self, view: PublishedProjectGraphView) {
        self.published.insert(view.project_id.clone(), view);
    }

    pub fn install_provisional(&mut self, overlay: ProvisionalProjectGraphOverlay) {
        self.provisional.insert(
            (overlay.project_id.clone(), overlay.workspace_id.clone()),
            overlay,
        );
    }

    pub fn remove_provisional(&mut self, project_id: &ProjectId, workspace_id: &WorkspaceId) {
        self.provisional
            .remove(&(project_id.clone(), workspace_id.clone()));
    }

    pub fn list_published(&self, project_id: &ProjectId) -> Vec<ProjectGraphViewEntry> {
        self.published
            .get(project_id)
            .map(|view| view.graphs.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Every installed published view, project by project. The word-search
    /// authority snapshot walks this under one read lock so a mid-query view
    /// install cannot change the filter halfway through a search.
    pub fn iter_published(&self) -> impl Iterator<Item = (&ProjectId, &PublishedProjectGraphView)> {
        self.published.iter()
    }

    pub fn list_own(
        &self,
        project_id: &ProjectId,
        workspace_id: &WorkspaceId,
    ) -> Vec<ProjectGraphViewEntry> {
        let mut graphs = self
            .published
            .get(project_id)
            .map(|view| view.graphs.clone())
            .unwrap_or_default();
        if let Some(overlay) = self
            .provisional
            .get(&(project_id.clone(), workspace_id.clone()))
        {
            for (graph_id, value) in &overlay.graphs {
                match value {
                    ProjectGraphOverlayValue::Upsert(entry) => {
                        graphs.insert(graph_id.clone(), entry.clone());
                    }
                    ProjectGraphOverlayValue::Tombstone { .. } => {
                        graphs.remove(graph_id);
                    }
                }
            }
        }
        graphs.into_values().collect()
    }

    pub fn load_published(&self, project_id: &ProjectId, graph_id: &str) -> ProjectGraphRead {
        self.published
            .get(project_id)
            .and_then(|view| view.graphs.get(graph_id))
            .cloned()
            .map(read_from_entry)
            .unwrap_or(ProjectGraphRead::Missing)
    }

    pub fn load_own(
        &self,
        project_id: &ProjectId,
        workspace_id: &WorkspaceId,
        graph_id: &str,
    ) -> ProjectGraphRead {
        if let Some(value) = self
            .provisional
            .get(&(project_id.clone(), workspace_id.clone()))
            .and_then(|overlay| overlay.graphs.get(graph_id))
        {
            return match value {
                ProjectGraphOverlayValue::Upsert(entry) => read_from_entry(entry.clone()),
                ProjectGraphOverlayValue::Tombstone { generation, .. } => {
                    ProjectGraphRead::Tombstoned(generation.clone())
                }
            };
        }
        self.load_published(project_id, graph_id)
    }

    pub fn provisional_for_project(
        &self,
        project_id: &ProjectId,
    ) -> Vec<&ProvisionalProjectGraphOverlay> {
        self.provisional
            .iter()
            .filter_map(|((candidate, _), overlay)| (candidate == project_id).then_some(overlay))
            .collect()
    }

    pub fn published_view(&self, project_id: &ProjectId) -> Option<&PublishedProjectGraphView> {
        self.published.get(project_id)
    }

    pub fn provisional_overlay(
        &self,
        project_id: &ProjectId,
        workspace_id: &WorkspaceId,
    ) -> Option<&ProvisionalProjectGraphOverlay> {
        self.provisional
            .get(&(project_id.clone(), workspace_id.clone()))
    }

    /// Install one connector-managed source graph.
    ///
    /// Refused when the project already publishes a graph under that id: one
    /// graph id in one project holds exactly one authority, and a connector
    /// refresh must never replace or shadow project-authored facts. The
    /// reverse ordering (a later publication introducing a colliding id)
    /// cannot be refused here, so it is resolved at read time in favour of the
    /// project-authored graph by [`Self::visible_connector`].
    pub fn install_connector(&mut self, view: ConnectorProjectGraphView) -> Result<()> {
        if self
            .published
            .get(&view.project_id)
            .is_some_and(|published| published.graphs.contains_key(&view.graph_id))
        {
            bail!(
                "error.graph_authority_conflict: graph `{}` is project authored; \
                 a connector refresh cannot replace it",
                view.graph_id
            );
        }
        self.connector
            .insert((view.project_id.clone(), view.graph_id.clone()), view);
        Ok(())
    }

    pub fn remove_connector(&mut self, project_id: &ProjectId, graph_id: &str) {
        self.connector
            .remove(&(project_id.clone(), graph_id.to_string()));
    }

    /// Every connector-managed graph for a project that is not shadowed by a
    /// project-authored graph of the same id.
    pub fn list_connector(&self, project_id: &ProjectId) -> Vec<ProjectGraphViewEntry> {
        self.connector
            .iter()
            .filter_map(|((candidate, graph_id), view)| {
                (candidate == project_id && self.visible_connector(project_id, graph_id).is_some())
                    .then(|| view.entry.clone())
            })
            .collect()
    }

    pub fn load_connector(&self, project_id: &ProjectId, graph_id: &str) -> ProjectGraphRead {
        self.visible_connector(project_id, graph_id)
            .map(|view| read_from_entry(view.entry.clone()))
            .unwrap_or(ProjectGraphRead::Missing)
    }

    /// The connector view for a graph id, unless a project-authored graph
    /// claims that id. Project authorship is the stronger authority, so a
    /// collision hides the connector projection rather than the project's own
    /// facts.
    pub fn visible_connector(
        &self,
        project_id: &ProjectId,
        graph_id: &str,
    ) -> Option<&ConnectorProjectGraphView> {
        if self
            .published
            .get(project_id)
            .is_some_and(|published| published.graphs.contains_key(graph_id))
        {
            return None;
        }
        self.connector
            .get(&(project_id.clone(), graph_id.to_string()))
    }

    /// The accepted binding set under published visibility.
    ///
    /// An unknown project and a project with no evidence lane both read as
    /// the empty set: an absent lane is not an error, it is an absence of
    /// assertions.
    pub fn evidence_published(&self, project_id: &ProjectId) -> EvidenceBindingSet {
        self.published
            .get(project_id)
            .map(|view| view.evidence.clone())
            .unwrap_or_default()
    }

    /// The binding set one checkout sees under own visibility: its own
    /// document when it committed a valid one, the published set otherwise.
    ///
    /// An `Invalid` overlay deliberately falls through to the published set.
    /// That is the replacement rule at the read plane: a bad candidate leaves
    /// the prior accepted set intact rather than blanking the caller's graph.
    pub fn evidence_own(
        &self,
        project_id: &ProjectId,
        workspace_id: &WorkspaceId,
    ) -> EvidenceBindingSet {
        match self
            .provisional
            .get(&(project_id.clone(), workspace_id.clone()))
            .and_then(|overlay| overlay.evidence.as_ref())
        {
            Some(EvidenceOverlayValue::Upsert(bindings)) => bindings.clone(),
            Some(EvidenceOverlayValue::Tombstone) => EvidenceBindingSet::default(),
            Some(EvidenceOverlayValue::Invalid { .. }) | None => {
                self.evidence_published(project_id)
            }
        }
    }

    /// Every binding set visible for a project under all visibility: the
    /// published set plus each live checkout's own replacement.
    ///
    /// Sets are returned separately rather than merged, because two checkouts
    /// can assert contradicting bindings for the same id and merging would
    /// silently pick one.
    pub fn evidence_all(&self, project_id: &ProjectId) -> Vec<EvidenceBindingSet> {
        let mut sets = vec![self.evidence_published(project_id)];
        for overlay in self.provisional_for_project(project_id) {
            match overlay.evidence.as_ref() {
                Some(EvidenceOverlayValue::Upsert(bindings)) => sets.push(bindings.clone()),
                Some(EvidenceOverlayValue::Tombstone)
                | Some(EvidenceOverlayValue::Invalid { .. })
                | None => {}
            }
        }
        sets
    }
}

/// Build the read-plane view of one accepted connector generation.
///
/// The generation comes from the source projection store, which has already
/// validated it, so this only refuses a generation that is not actually
/// connector authored.
pub fn build_connector_graph_view(
    project_id: ProjectId,
    generation: GraphGeneration,
) -> Result<ConnectorProjectGraphView> {
    if generation.descriptor.authority != bbox_project_graph::GraphAuthority::Connector {
        bail!(
            "error.graph_authority_conflict: graph `{}` is not connector authored",
            generation.descriptor.graph_id
        );
    }
    let Some(source_connector) = generation.descriptor.source_connector.clone() else {
        bail!(
            "error.graph_authority_conflict: connector graph `{}` names no source connector",
            generation.descriptor.graph_id
        );
    };
    let graph_id = generation.descriptor.graph_id.clone();
    let identity = ProjectGraphGenerationIdentity {
        accepted_generation: generation.descriptor.generation.to_string(),
        // A connector projection has no publisher commit: it is accepted from
        // observations, not from a checkout.
        accepted_commit: String::new(),
        source_generation: generation.descriptor.projection_version.clone(),
        // Load bearing: a connector graph is never workspace scoped, which is
        // what keeps the read plane from labelling it provisional.
        workspace_id: None,
        content_hash: generation.fingerprint.clone(),
    };
    Ok(ConnectorProjectGraphView {
        project_id,
        graph_id: graph_id.clone(),
        source_connector,
        entry: ProjectGraphViewEntry::valid(graph_id, identity, generation),
    })
}

fn read_from_entry(entry: ProjectGraphViewEntry) -> ProjectGraphRead {
    match entry.validity {
        ProjectGraphValidity::Valid => ProjectGraphRead::Valid(entry),
        ProjectGraphValidity::Invalid { .. } => ProjectGraphRead::Invalid(entry),
    }
}

pub fn build_published_graph_view(
    verified: &VerifiedAcceptedPublication,
) -> Result<PublishedProjectGraphView> {
    let stamp = verified.content_stamp();
    let project_id = stamp.project_id().clone();
    let scope = stamp.accepted_scope().clone();
    let documents = group_accepted_sources(&scope, verified.graph_sources())?;
    let mut graphs = BTreeMap::new();
    for (graph_id, files) in documents {
        let entry = parse_graph_entry(
            project_id.as_str(),
            &graph_id,
            &files,
            ProjectGraphGenerationIdentity {
                accepted_generation: stamp.generation_id().to_string(),
                accepted_commit: stamp.accepted_commit().to_string(),
                source_generation: None,
                workspace_id: None,
                content_hash: String::new(),
            },
        );
        if matches!(entry.validity, ProjectGraphValidity::Invalid { .. }) {
            bail!("accepted publication contains invalid graph `{graph_id}`");
        }
        graphs.insert(graph_id, entry);
    }
    // The accepted publication already refused an invalid document at prepare
    // and re-validated it on read, so anything that failed here would be a
    // store integrity failure, not a user error. Bail rather than silently
    // publishing a project with its bindings quietly dropped.
    let evidence = match verified.evidence_sources().values().next() {
        Some(source) => {
            let load = parse_evidence_document(
                project_id.as_str(),
                &source.source_bytes,
                EvidenceParseLimits::default(),
            );
            load.bindings.ok_or_else(|| {
                anyhow::anyhow!("accepted publication contains an invalid evidence document")
            })?
        }
        None => EvidenceBindingSet::default(),
    };
    Ok(PublishedProjectGraphView {
        project_id,
        scope,
        graphs,
        evidence,
    })
}

pub fn validate_project_graph_tree(
    scope_id: &str,
    project_root: &std::path::Path,
) -> Result<ProjectGraphTreeValidation> {
    let graph_root = project_root.join(".bbox/graphs");
    let Ok(entries) = std::fs::read_dir(&graph_root) else {
        return Ok(ProjectGraphTreeValidation {
            graph_count: 0,
            errors: Vec::new(),
        });
    };
    let limits = KnowledgeSourceLimits::default();
    let mut graph_count = 0_usize;
    let mut errors = Vec::new();
    for entry in entries {
        let entry = entry?;
        let graph_id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("graph id is not UTF-8"))?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            errors.push(ValidationError::new(
                "graph.admission_entry_type",
                graph_id,
                None,
                "graph lane entries must be directories and must not be symlinks",
            ));
            continue;
        }
        graph_count = graph_count.saturating_add(1);
        if graph_count as u64 > limits.max_graphs_per_lane {
            errors.push(ValidationError::new(
                "graph.admission_graph_limit",
                ".bbox/graphs",
                None,
                "graph lane exceeds its graph count limit",
            ));
            break;
        }
        let mut files = BTreeMap::new();
        for file in std::fs::read_dir(entry.path())? {
            let file = file?;
            let filename = file
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("graph filename is not UTF-8"))?;
            let metadata = std::fs::symlink_metadata(file.path())?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || !matches!(
                    filename.as_str(),
                    "schema.json" | "vertices.jsonl" | "edges.jsonl"
                )
            {
                errors.push(ValidationError::new(
                    "graph.admission_unknown_file",
                    filename,
                    None,
                    format!("graph `{graph_id}` contains an unknown or unsafe file"),
                ));
                continue;
            }
            let bytes = std::fs::read(file.path())?;
            let graph_jsonl = matches!(filename.as_str(), "vertices.jsonl" | "edges.jsonl");
            if (!graph_jsonl && bytes.is_empty()) || bytes.len() as u64 > limits.max_file_bytes {
                errors.push(ValidationError::new(
                    "graph.admission_file_limit",
                    filename.clone(),
                    None,
                    format!("graph `{graph_id}` source file exceeds its byte limit"),
                ));
            }
            files.insert(filename, bytes);
        }
        let graph_bytes = files
            .values()
            .try_fold(0_u64, |total, bytes| total.checked_add(bytes.len() as u64));
        if graph_bytes.is_none_or(|bytes| bytes > limits.max_graph_bytes) {
            errors.push(ValidationError::new(
                "graph.admission_graph_bytes",
                graph_id.clone(),
                None,
                "graph exceeds its aggregate byte limit",
            ));
            continue;
        }
        let entry = parse_graph_entry(
            scope_id,
            &graph_id,
            &files,
            ProjectGraphGenerationIdentity {
                accepted_generation: String::new(),
                accepted_commit: String::new(),
                source_generation: None,
                workspace_id: None,
                content_hash: String::new(),
            },
        );
        if let ProjectGraphValidity::Invalid {
            errors: graph_errors,
        } = entry.validity
        {
            errors.extend(graph_errors);
        }
    }
    Ok(ProjectGraphTreeValidation {
        graph_count,
        errors,
    })
}

pub fn build_provisional_graph_overlay(
    source: &ReadyProvisionalWorkspace,
    verified: &VerifiedAcceptedPublication,
) -> Result<ProvisionalProjectGraphOverlay> {
    let stamp = verified.content_stamp();
    if source.project_id != stamp.project_id().as_str()
        || source.descriptor.scope != *stamp.accepted_scope()
        || source.descriptor.accepted_generation != stamp.generation_id()
        || source.descriptor.accepted_commit != stamp.accepted_commit()
    {
        bail!("provisional graph source does not match accepted publication");
    }
    let baseline = group_ready_sources(&source.descriptor.scope, &source.baseline_graphs)?;
    let working = group_ready_sources(&source.descriptor.scope, &source.working_graphs)?;
    let graph_ids = baseline
        .keys()
        .chain(working.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut graphs = BTreeMap::new();
    for graph_id in graph_ids {
        let generation = ProjectGraphGenerationIdentity {
            accepted_generation: stamp.generation_id().to_string(),
            accepted_commit: stamp.accepted_commit().to_string(),
            source_generation: Some(source.source_generation_id.clone()),
            workspace_id: Some(source.descriptor.workspace_id.clone()),
            content_hash: String::new(),
        };
        match (baseline.get(&graph_id), working.get(&graph_id)) {
            (Some(before), Some(after)) if before == after => {}
            (_, Some(after)) => {
                graphs.insert(
                    graph_id.clone(),
                    ProjectGraphOverlayValue::Upsert(parse_graph_entry(
                        &source.project_id,
                        &graph_id,
                        after,
                        generation,
                    )),
                );
            }
            (Some(_), None) => {
                graphs.insert(
                    graph_id.clone(),
                    ProjectGraphOverlayValue::Tombstone {
                        graph_id,
                        generation,
                    },
                );
            }
            (None, None) => {}
        }
    }
    let evidence = evidence_overlay(
        stamp.project_id().as_str(),
        source.baseline_evidence.first(),
        source.working_evidence.first(),
    );
    Ok(ProvisionalProjectGraphOverlay {
        project_id: stamp.project_id().clone(),
        scope: source.descriptor.scope.clone(),
        workspace_id: source.descriptor.workspace_id.clone(),
        source_generation_id: source.source_generation_id.clone(),
        graphs,
        evidence,
    })
}

/// What this checkout's evidence lane does relative to its baseline.
///
/// The lane is a single document, so the diff is a four-way case rather than
/// the per-graph-id union the graph lane needs. An unchanged document
/// produces no overlay at all, which keeps the published set authoritative
/// and avoids minting a redundant per-checkout copy of it.
fn evidence_overlay(
    project_id: &str,
    baseline: Option<&ReadyPublicationFile>,
    working: Option<&ReadyPublicationFile>,
) -> Option<EvidenceOverlayValue> {
    match (baseline, working) {
        (Some(before), Some(after)) if before.source_bytes == after.source_bytes => None,
        (_, Some(after)) => {
            let load = parse_evidence_document(
                project_id,
                &after.source_bytes,
                EvidenceParseLimits::default(),
            );
            Some(match load.bindings {
                Some(bindings) => EvidenceOverlayValue::Upsert(bindings),
                None => EvidenceOverlayValue::Invalid {
                    errors: load.errors,
                },
            })
        }
        (Some(_), None) => Some(EvidenceOverlayValue::Tombstone),
        (None, None) => None,
    }
}

fn parse_graph_entry(
    scope_id: &str,
    graph_id: &str,
    files: &BTreeMap<String, Vec<u8>>,
    mut identity: ProjectGraphGenerationIdentity,
) -> ProjectGraphViewEntry {
    let limits = KnowledgeSourceLimits::default();
    let graph_bytes = files
        .values()
        .try_fold(0_u64, |total, bytes| total.checked_add(bytes.len() as u64));
    if graph_bytes.is_none_or(|bytes| bytes > limits.max_graph_bytes) {
        return invalid_entry(
            graph_id,
            identity,
            ValidationError::new(
                "graph.parse_graph_byte_limit",
                "graph",
                None,
                "graph exceeds its parse-time aggregate byte limit",
            ),
        );
    }
    if let Some((filename, _)) = files
        .iter()
        .find(|(_, bytes)| bytes.len() as u64 > limits.max_file_bytes)
    {
        return invalid_entry(
            graph_id,
            identity,
            ValidationError::new(
                "graph.parse_file_byte_limit",
                filename,
                None,
                "graph source exceeds its parse-time file byte limit",
            ),
        );
    }
    let required = (
        files.get("schema.json"),
        files.get("vertices.jsonl"),
        files.get("edges.jsonl"),
    );
    let (Some(schema), Some(vertices), Some(edges)) = required else {
        return invalid_entry(
            graph_id,
            identity,
            ValidationError::new(
                "graph.incomplete_source",
                "graph",
                None,
                "graph source is missing a required file",
            ),
        );
    };
    let loaded = load_graph_documents(
        scope_id,
        graph_id,
        GraphDocumentBytes {
            descriptor: files.get("graph.json").map(Vec::as_slice),
            schema,
            vertices,
            edges,
        },
        GraphParseLimits {
            max_vertices: limits.max_graph_rows_per_file as usize,
            max_edges: limits.max_graph_rows_per_file as usize,
        },
        PathBuf::new(),
    );
    identity.content_hash = loaded.report.fingerprint.clone().unwrap_or_default();
    match loaded.generation {
        Some(graph) => ProjectGraphViewEntry {
            graph_id: graph_id.to_string(),
            validity: ProjectGraphValidity::Valid,
            generation: identity,
            graph: Some(Arc::new(graph)),
        },
        None => ProjectGraphViewEntry {
            graph_id: graph_id.to_string(),
            validity: ProjectGraphValidity::Invalid {
                errors: loaded.report.errors,
            },
            generation: identity,
            graph: None,
        },
    }
}

fn invalid_entry(
    graph_id: &str,
    identity: ProjectGraphGenerationIdentity,
    error: ValidationError,
) -> ProjectGraphViewEntry {
    ProjectGraphViewEntry {
        graph_id: graph_id.to_string(),
        validity: ProjectGraphValidity::Invalid {
            errors: vec![error],
        },
        generation: identity,
        graph: None,
    }
}

fn group_accepted_sources(
    scope: &PublishedScope,
    sources: &BTreeMap<
        crate::accepted_publication_store::NormalizedRepoRelativeFilename,
        AcceptedGraphSourceV1,
    >,
) -> Result<BTreeMap<String, BTreeMap<String, Vec<u8>>>> {
    group_sources(
        scope,
        sources
            .iter()
            .map(|(filename, source)| (filename.as_str(), source.source_bytes.as_slice())),
    )
}

fn group_ready_sources(
    scope: &PublishedScope,
    sources: &[ReadyPublicationFile],
) -> Result<BTreeMap<String, BTreeMap<String, Vec<u8>>>> {
    group_sources(
        scope,
        sources.iter().map(|source| {
            (
                source.manifest.repository_relative_filename.as_str(),
                source.source_bytes.as_slice(),
            )
        }),
    )
}

fn group_sources<'a>(
    scope: &PublishedScope,
    sources: impl Iterator<Item = (&'a str, &'a [u8])>,
) -> Result<BTreeMap<String, BTreeMap<String, Vec<u8>>>> {
    let prefix = if scope.bbox_root_relpath() == "." {
        ".bbox/graphs/".to_string()
    } else {
        format!("{}/.bbox/graphs/", scope.bbox_root_relpath())
    };
    let mut graphs = BTreeMap::<String, BTreeMap<String, Vec<u8>>>::new();
    for (path, bytes) in sources {
        let relative = path
            .strip_prefix(&prefix)
            .with_context(|| format!("graph source `{path}` is outside its published scope"))?;
        let (graph_id, filename) = relative
            .split_once('/')
            .context("graph source path has invalid depth")?;
        if relative.matches('/').count() != 1
            || graphs
                .entry(graph_id.to_string())
                .or_default()
                .insert(filename.to_string(), bytes.to_vec())
                .is_some()
        {
            bail!("graph source path is duplicate or invalid");
        }
    }
    Ok(graphs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_project_graph::ValidationError;

    fn project_id() -> ProjectId {
        ProjectId::parse("p_graph_view".to_string()).unwrap()
    }

    fn workspace_id() -> WorkspaceId {
        WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    fn identity(hash: &str, provisional: bool) -> ProjectGraphGenerationIdentity {
        ProjectGraphGenerationIdentity {
            accepted_generation: "a".repeat(64),
            accepted_commit: "b".repeat(40),
            source_generation: provisional.then(|| "kws_source".to_string()),
            workspace_id: provisional.then(workspace_id),
            content_hash: hash.to_string(),
        }
    }

    fn valid_entry(graph_id: &str, hash: &str, provisional: bool) -> ProjectGraphViewEntry {
        ProjectGraphViewEntry {
            graph_id: graph_id.to_string(),
            validity: ProjectGraphValidity::Valid,
            generation: identity(hash, provisional),
            graph: None,
        }
    }

    fn invalid_overlay(graph_id: &str) -> ProjectGraphViewEntry {
        ProjectGraphViewEntry {
            graph_id: graph_id.to_string(),
            validity: ProjectGraphValidity::Invalid {
                errors: vec![ValidationError::new(
                    "edge.missing_source",
                    "edges.jsonl",
                    Some(1),
                    "edge source is missing",
                )],
            },
            generation: identity("invalid", true),
            graph: None,
        }
    }

    fn connector_generation(graph_id: &str, generation: u64) -> GraphGeneration {
        let schema: bbox_project_graph::GraphSchema = serde_json::from_str(
            r#"{"version":1,"namespace":"dataset","vertex_types":{"dataset:Asset":{"required":["remote_id"],"properties":{"remote_id":"string"}}},"edge_types":[]}"#,
        )
        .unwrap();
        bbox_project_graph::build_generation(
            bbox_project_graph::GraphKey {
                scope_id: "connector-source:synthetic-api:tenant".into(),
                graph_id: graph_id.to_string(),
                source: bbox_project_graph::GraphSource::ConnectorManaged,
            },
            bbox_project_graph::GraphDescriptor {
                descriptor_version: bbox_project_graph::DESCRIPTOR_VERSION,
                scope: bbox_project_graph::GraphScope::Project,
                graph_id: graph_id.to_string(),
                authority: bbox_project_graph::GraphAuthority::Connector,
                schema_id: "dataset:schema".into(),
                schema_version: 1,
                projection_version: Some("dataset-v1".into()),
                source_connector: Some("synthetic-api".into()),
                retention_policy: bbox_project_graph::RetentionPolicy::ConnectorManaged,
                generation,
            },
            schema,
            Vec::new(),
            Vec::new(),
            "c".repeat(64),
            std::path::PathBuf::from("/source-graphs"),
        )
    }

    /// A connector projection is visible without any provisional opt-in, and
    /// it carries connector identity rather than a workspace.
    #[test]
    fn connector_graphs_are_visible_without_a_checkout_opt_in() {
        let project_id = project_id();
        let mut catalog = ProjectGraphViewCatalog::default();
        let view = build_connector_graph_view(
            project_id.clone(),
            connector_generation("source-assets", 3),
        )
        .unwrap();
        assert_eq!(view.source_connector, "synthetic-api");
        assert_eq!(view.entry.generation.accepted_generation, "3");
        assert!(view.entry.generation.workspace_id.is_none());
        catalog.install_connector(view).unwrap();

        let listed = catalog.list_connector(&project_id);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].graph_id, "source-assets");
        assert!(matches!(
            catalog.load_connector(&project_id, "source-assets"),
            ProjectGraphRead::Valid(_)
        ));
        assert!(matches!(
            catalog.load_connector(&project_id, "missing"),
            ProjectGraphRead::Missing
        ));
        // The project-authored lanes are untouched by a connector install.
        assert!(catalog.list_published(&project_id).is_empty());
    }

    /// A connector refresh cannot replace a project-authored graph, and a
    /// later publication of the same id shadows the connector projection
    /// rather than the other way round.
    #[test]
    fn connector_graphs_never_replace_project_authored_graphs() {
        let project_id = project_id();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let mut catalog = ProjectGraphViewCatalog::default();
        catalog.install_published(PublishedProjectGraphView {
            project_id: project_id.clone(),
            scope: scope.clone(),
            graphs: BTreeMap::from([(
                "records".into(),
                valid_entry("records", "published", false),
            )]),
            evidence: EvidenceBindingSet::default(),
        });

        let colliding =
            build_connector_graph_view(project_id.clone(), connector_generation("records", 1))
                .unwrap();
        let error = catalog.install_connector(colliding).unwrap_err();
        assert!(
            error.to_string().contains("error.graph_authority_conflict"),
            "{error}"
        );
        assert!(matches!(
            catalog.load_published(&project_id, "records"),
            ProjectGraphRead::Valid(_)
        ));

        // The reverse ordering: the connector graph lands first, then a
        // publication claims the id. The project-authored graph wins.
        let mut catalog = ProjectGraphViewCatalog::default();
        catalog
            .install_connector(
                build_connector_graph_view(project_id.clone(), connector_generation("records", 1))
                    .unwrap(),
            )
            .unwrap();
        catalog.install_published(PublishedProjectGraphView {
            project_id: project_id.clone(),
            scope,
            graphs: BTreeMap::from([(
                "records".into(),
                valid_entry("records", "published", false),
            )]),
            evidence: EvidenceBindingSet::default(),
        });
        assert!(catalog.list_connector(&project_id).is_empty());
        assert!(matches!(
            catalog.load_connector(&project_id, "records"),
            ProjectGraphRead::Missing
        ));
        assert!(catalog.visible_connector(&project_id, "records").is_none());
    }

    /// The read plane only accepts generations that really are connector
    /// authored.
    #[test]
    fn a_project_authored_generation_cannot_become_a_connector_view() {
        let mut generation = connector_generation("source-assets", 1);
        generation.descriptor.authority = bbox_project_graph::GraphAuthority::Project;
        let error = build_connector_graph_view(project_id(), generation).unwrap_err();
        assert!(
            error.to_string().contains("error.graph_authority_conflict"),
            "{error}"
        );

        let mut generation = connector_generation("source-assets", 1);
        generation.descriptor.source_connector = None;
        let error = build_connector_graph_view(project_id(), generation).unwrap_err();
        assert!(
            error.to_string().contains("names no source connector"),
            "{error}"
        );
    }

    fn bindings_document(binding_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "bindings": [{
                "binding_id": binding_id,
                "source": {"kind": "graph_vertex", "graph_id": "records", "vertex_id": "record-1"},
                "kind": "record:CORRESPONDS_TO",
                "target": {"kind": "graph_vertex", "graph_id": "source", "vertex_id": "asset-1"},
                "assertion_authority": "project",
                "mapping_version": "mapping-v1",
                "asserted_at": "2026-01-01T00:00:00Z"
            }]
        }))
        .unwrap()
    }

    fn ready_evidence(bytes: Vec<u8>) -> ReadyPublicationFile {
        ReadyPublicationFile {
            manifest: bbox_knowledge_source::SourceFileManifestEntryV1 {
                repository_relative_filename: ".bbox/evidence/bindings.json".to_string(),
                encoded_bytes: bytes.len() as u64,
                content_sha256: bbox_knowledge_source::source_file_blob_sha256(&bytes),
            },
            source_bytes: bytes,
        }
    }

    fn published_with_evidence(
        project_id: &ProjectId,
        scope: &PublishedScope,
        evidence: EvidenceBindingSet,
    ) -> PublishedProjectGraphView {
        PublishedProjectGraphView {
            project_id: project_id.clone(),
            scope: scope.clone(),
            graphs: BTreeMap::new(),
            evidence,
        }
    }

    fn accepted_set(binding_id: &str) -> EvidenceBindingSet {
        parse_evidence_document(
            "proj-a",
            &bindings_document(binding_id),
            EvidenceParseLimits::default(),
        )
        .bindings
        .expect("fixture document is valid")
    }

    /// An unchanged evidence document mints no overlay: the published set
    /// stays authoritative rather than being copied per checkout.
    #[test]
    fn an_unchanged_evidence_document_produces_no_overlay() {
        let file = ready_evidence(bindings_document("b1"));
        assert!(evidence_overlay("proj-a", Some(&file), Some(&file)).is_none());
        assert!(evidence_overlay("proj-a", None, None).is_none());
    }

    /// A changed document replaces the whole set; a deleted one tombstones it.
    #[test]
    fn a_changed_evidence_document_upserts_and_a_deleted_one_tombstones() {
        let baseline = ready_evidence(bindings_document("b1"));
        let working = ready_evidence(bindings_document("b2"));
        let Some(EvidenceOverlayValue::Upsert(bindings)) =
            evidence_overlay("proj-a", Some(&baseline), Some(&working))
        else {
            panic!("a changed document should upsert the whole set");
        };
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings.iter().next().unwrap().binding_id, "b2");

        assert!(matches!(
            evidence_overlay("proj-a", Some(&baseline), None),
            Some(EvidenceOverlayValue::Tombstone)
        ));
        // A checkout that adds the lane where the baseline had none upserts.
        assert!(matches!(
            evidence_overlay("proj-a", None, Some(&working)),
            Some(EvidenceOverlayValue::Upsert(_))
        ));
    }

    /// Contract: an invalid candidate leaves the prior accepted set intact.
    /// The overlay records why, and own visibility still reads published.
    #[test]
    fn an_invalid_evidence_document_keeps_the_published_set() {
        let baseline = ready_evidence(bindings_document("b1"));
        let broken = ready_evidence(br#"{"version":1,"bindings":[{"binding_id":""}]}"#.to_vec());
        let Some(EvidenceOverlayValue::Invalid { errors }) =
            evidence_overlay("proj-a", Some(&baseline), Some(&broken))
        else {
            panic!("an invalid document should not replace the accepted set");
        };
        assert!(!errors.is_empty());

        let project_id = project_id();
        let workspace_id = workspace_id();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let mut catalog = ProjectGraphViewCatalog::default();
        catalog.install_published(published_with_evidence(
            &project_id,
            &scope,
            accepted_set("published-binding"),
        ));
        catalog.install_provisional(ProvisionalProjectGraphOverlay {
            project_id: project_id.clone(),
            scope,
            workspace_id: workspace_id.clone(),
            source_generation_id: "kws_source".into(),
            graphs: BTreeMap::new(),
            evidence: Some(EvidenceOverlayValue::Invalid { errors }),
        });
        let own = catalog.evidence_own(&project_id, &workspace_id);
        assert_eq!(own.len(), 1);
        assert_eq!(
            own.iter().next().unwrap().binding_id,
            "published-binding",
            "an invalid candidate must leave the prior accepted set intact"
        );
    }

    /// An absent evidence lane reads as the empty accepted set, for an
    /// unknown project and for a pre-evidence publication alike.
    #[test]
    fn an_absent_evidence_lane_reads_as_the_empty_set() {
        let project_id = project_id();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let mut catalog = ProjectGraphViewCatalog::default();
        assert!(catalog.evidence_published(&project_id).is_empty());
        catalog.install_published(published_with_evidence(
            &project_id,
            &scope,
            EvidenceBindingSet::default(),
        ));
        assert!(catalog.evidence_published(&project_id).is_empty());
        assert_eq!(catalog.evidence_all(&project_id).len(), 1);
    }

    /// Own visibility replaces the whole published set, and a tombstone
    /// empties it, without disturbing what published visibility reports.
    #[test]
    fn own_evidence_replaces_the_published_set_without_changing_published() {
        let project_id = project_id();
        let workspace_id = workspace_id();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let mut catalog = ProjectGraphViewCatalog::default();
        catalog.install_published(published_with_evidence(
            &project_id,
            &scope,
            accepted_set("published-binding"),
        ));
        catalog.install_provisional(ProvisionalProjectGraphOverlay {
            project_id: project_id.clone(),
            scope: scope.clone(),
            workspace_id: workspace_id.clone(),
            source_generation_id: "kws_source".into(),
            graphs: BTreeMap::new(),
            evidence: Some(EvidenceOverlayValue::Upsert(accepted_set("own-binding"))),
        });
        assert_eq!(
            catalog
                .evidence_own(&project_id, &workspace_id)
                .iter()
                .next()
                .unwrap()
                .binding_id,
            "own-binding"
        );
        assert_eq!(
            catalog
                .evidence_published(&project_id)
                .iter()
                .next()
                .unwrap()
                .binding_id,
            "published-binding"
        );
        // All visibility keeps the two sets separate rather than merging two
        // possibly contradicting assertions for one binding id.
        assert_eq!(catalog.evidence_all(&project_id).len(), 2);

        catalog.install_provisional(ProvisionalProjectGraphOverlay {
            project_id: project_id.clone(),
            scope,
            workspace_id: workspace_id.clone(),
            source_generation_id: "kws_source".into(),
            graphs: BTreeMap::new(),
            evidence: Some(EvidenceOverlayValue::Tombstone),
        });
        assert!(catalog.evidence_own(&project_id, &workspace_id).is_empty());
        assert!(!catalog.evidence_published(&project_id).is_empty());
        assert_eq!(catalog.evidence_all(&project_id).len(), 1);
    }

    #[test]
    fn own_view_replaces_one_whole_graph_without_changing_published() {
        let project_id = project_id();
        let workspace_id = workspace_id();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let mut catalog = ProjectGraphViewCatalog::default();
        catalog.install_published(PublishedProjectGraphView {
            project_id: project_id.clone(),
            scope: scope.clone(),
            graphs: BTreeMap::from([
                ("records".into(), valid_entry("records", "published", false)),
                ("other".into(), valid_entry("other", "other", false)),
            ]),
            evidence: EvidenceBindingSet::default(),
        });
        catalog.install_provisional(ProvisionalProjectGraphOverlay {
            project_id: project_id.clone(),
            scope,
            workspace_id: workspace_id.clone(),
            source_generation_id: "kws_source".into(),
            graphs: BTreeMap::from([(
                "records".into(),
                ProjectGraphOverlayValue::Upsert(valid_entry("records", "working", true)),
            )]),
            evidence: None,
        });

        let ProjectGraphRead::Valid(published) = catalog.load_published(&project_id, "records")
        else {
            panic!("published graph should remain visible");
        };
        assert_eq!(published.generation.content_hash, "published");
        let ProjectGraphRead::Valid(own) = catalog.load_own(&project_id, &workspace_id, "records")
        else {
            panic!("own graph should resolve the overlay");
        };
        assert_eq!(own.generation.content_hash, "working");
        assert_eq!(catalog.list_own(&project_id, &workspace_id).len(), 2);
    }

    #[test]
    fn invalid_own_graph_never_falls_back_and_does_not_hide_other_graphs() {
        let project_id = project_id();
        let workspace_id = workspace_id();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let mut catalog = ProjectGraphViewCatalog::default();
        catalog.install_published(PublishedProjectGraphView {
            project_id: project_id.clone(),
            scope: scope.clone(),
            graphs: BTreeMap::from([
                ("records".into(), valid_entry("records", "published", false)),
                ("other".into(), valid_entry("other", "other", false)),
            ]),
            evidence: EvidenceBindingSet::default(),
        });
        catalog.install_provisional(ProvisionalProjectGraphOverlay {
            project_id: project_id.clone(),
            scope,
            workspace_id: workspace_id.clone(),
            source_generation_id: "kws_source".into(),
            graphs: BTreeMap::from([(
                "records".into(),
                ProjectGraphOverlayValue::Upsert(invalid_overlay("records")),
            )]),
            evidence: None,
        });

        assert!(matches!(
            catalog.load_own(&project_id, &workspace_id, "records"),
            ProjectGraphRead::Invalid(_)
        ));
        assert!(matches!(
            catalog.load_own(&project_id, &workspace_id, "other"),
            ProjectGraphRead::Valid(_)
        ));
        assert!(matches!(
            catalog.load_published(&project_id, "records"),
            ProjectGraphRead::Valid(_)
        ));
    }

    #[test]
    fn tombstone_hides_only_the_selected_graph_in_own_view() {
        let project_id = project_id();
        let workspace_id = workspace_id();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let mut catalog = ProjectGraphViewCatalog::default();
        catalog.install_published(PublishedProjectGraphView {
            project_id: project_id.clone(),
            scope: scope.clone(),
            graphs: BTreeMap::from([
                ("records".into(), valid_entry("records", "published", false)),
                ("other".into(), valid_entry("other", "other", false)),
            ]),
            evidence: EvidenceBindingSet::default(),
        });
        catalog.install_provisional(ProvisionalProjectGraphOverlay {
            project_id: project_id.clone(),
            scope,
            workspace_id: workspace_id.clone(),
            source_generation_id: "kws_source".into(),
            graphs: BTreeMap::from([(
                "records".into(),
                ProjectGraphOverlayValue::Tombstone {
                    graph_id: "records".into(),
                    generation: identity("deleted", true),
                },
            )]),
            evidence: None,
        });
        assert!(matches!(
            catalog.load_own(&project_id, &workspace_id, "records"),
            ProjectGraphRead::Tombstoned(_)
        ));
        assert_eq!(catalog.list_own(&project_id, &workspace_id).len(), 1);
    }
}
