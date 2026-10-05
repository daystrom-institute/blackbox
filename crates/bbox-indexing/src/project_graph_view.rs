use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_knowledge_source::KnowledgeSourceLimits;
use bbox_project_graph::{
    EvidenceBindingSet, EvidenceParseLimits, GraphDocumentBytes, GraphGeneration, GraphParseLimits,
    ValidationError, load_graph_documents, parse_evidence_document,
};

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
pub struct PublishedProjectGraphView {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    /// The accepted generation this view projects.
    ///
    /// It rides the view rather than being read back off an entry because
    /// the install admission gate needs it for EVERY view, and a project
    /// whose graphs lane is empty has no entry to read it from. An empty
    /// view for the current generation and an empty view left over from an
    /// older one are the same bytes and opposite answers.
    pub accepted_generation: String,
    pub graphs: BTreeMap<String, ProjectGraphViewEntry>,
    /// The project's accepted binding set. Empty for a publication written
    /// before the evidence lane existed, which is indistinguishable from a
    /// publication that simply asserts nothing, and correctly so.
    pub evidence: EvidenceBindingSet,
}

/// A connector-managed source graph as the read plane sees it.
///
/// Read-only by construction: these generations are accepted by a source
/// projection store, never by a checkout lane, so the
/// catalog offers no path that mutates one.
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
    Valid(ProjectGraphViewEntry),
    Invalid(ProjectGraphViewEntry),
}

#[derive(Debug, Default)]
pub struct ProjectGraphViewCatalog {
    published: BTreeMap<ProjectId, PublishedProjectGraphView>,
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

    pub fn load_published(&self, project_id: &ProjectId, graph_id: &str) -> ProjectGraphRead {
        self.published
            .get(project_id)
            .and_then(|view| view.graphs.get(graph_id))
            .cloned()
            .map(read_from_entry)
            .unwrap_or(ProjectGraphRead::Missing)
    }

    pub fn published_view(&self, project_id: &ProjectId) -> Option<&PublishedProjectGraphView> {
        self.published.get(project_id)
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
        accepted_generation: stamp.generation_id().to_string(),
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

    fn project_id() -> ProjectId {
        ProjectId::parse("p_graph_view".to_string()).unwrap()
    }

    fn valid_entry(graph_id: &str, hash: &str) -> ProjectGraphViewEntry {
        ProjectGraphViewEntry {
            graph_id: graph_id.to_string(),
            validity: ProjectGraphValidity::Valid,
            generation: ProjectGraphGenerationIdentity {
                accepted_generation: "a".repeat(64),
                accepted_commit: "b".repeat(40),
                source_generation: None,
                content_hash: hash.to_string(),
            },
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

    /// A connector projection is visible without any checkout opt-in and
    /// carries connector identity.
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
            accepted_generation: "test-accepted-generation".into(),
            graphs: BTreeMap::from([("records".into(), valid_entry("records", "published"))]),
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
            accepted_generation: "test-accepted-generation".into(),
            graphs: BTreeMap::from([("records".into(), valid_entry("records", "published"))]),
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

    fn published_with_evidence(
        project_id: &ProjectId,
        scope: &PublishedScope,
        evidence: EvidenceBindingSet,
    ) -> PublishedProjectGraphView {
        PublishedProjectGraphView {
            project_id: project_id.clone(),
            scope: scope.clone(),
            accepted_generation: "test-accepted-generation".into(),
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
    }

    #[test]
    fn the_published_evidence_set_is_the_accepted_document() {
        let project_id = project_id();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let mut catalog = ProjectGraphViewCatalog::default();
        catalog.install_published(published_with_evidence(
            &project_id,
            &scope,
            accepted_set("published-binding"),
        ));
        let published = catalog.evidence_published(&project_id);
        assert_eq!(published.len(), 1);
        assert_eq!(
            published.iter().next().unwrap().binding_id,
            "published-binding"
        );
    }
}
