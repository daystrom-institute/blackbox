use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_knowledge_source::KnowledgeSourceLimits;
use bbox_knowledge_source_store::{ReadyProvisionalWorkspace, ReadyPublicationFile};
use bbox_project_graph::{
    GraphDocumentBytes, GraphGeneration, GraphParseLimits, ValidationError, load_graph_documents,
};
use bro_core::WorkspaceId;

use crate::accepted_publication_runtime::VerifiedAcceptedPublication;
use crate::accepted_publication_store::AcceptedGraphSourceV1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectGraphValidity {
    Valid,
    Invalid { errors: Vec<ValidationError> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
}

#[derive(Debug, Clone)]
pub enum ProjectGraphOverlayValue {
    Upsert(ProjectGraphViewEntry),
    Tombstone {
        graph_id: String,
        generation: ProjectGraphGenerationIdentity,
    },
}

#[derive(Debug, Clone)]
pub struct PublishedProjectGraphView {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub graphs: BTreeMap<String, ProjectGraphViewEntry>,
}

#[derive(Debug, Clone)]
pub struct ProvisionalProjectGraphOverlay {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub workspace_id: WorkspaceId,
    pub source_generation_id: String,
    pub graphs: BTreeMap<String, ProjectGraphOverlayValue>,
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
    Ok(PublishedProjectGraphView {
        project_id,
        scope,
        graphs,
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
    Ok(ProvisionalProjectGraphOverlay {
        project_id: stamp.project_id().clone(),
        scope: source.descriptor.scope.clone(),
        workspace_id: source.descriptor.workspace_id.clone(),
        source_generation_id: source.source_generation_id.clone(),
        graphs,
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
        });
        assert!(matches!(
            catalog.load_own(&project_id, &workspace_id, "records"),
            ProjectGraphRead::Tombstoned(_)
        ));
        assert_eq!(catalog.list_own(&project_id, &workspace_id).len(), 1);
    }
}
