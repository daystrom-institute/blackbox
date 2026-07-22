#![allow(dead_code)] // D1 lands the provider surface; D2 wires public consumers.

pub mod agent;
pub mod artifact;
pub mod commit;
pub mod file;
pub mod knowledge;
pub mod note;
pub mod packet;
pub mod project_file;
pub mod roadmap_item;
pub mod session;
pub mod symbol;
pub mod system_memory;
pub mod thread;
pub mod transcript;
pub mod virtual_bash_call;
pub mod whiteboard;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{Result, bail};

use bbox_artifacts::artifacts::ArtifactCatalog;
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use bbox_corpus_index::index::TranscriptIndex;
use bbox_edge_index::edge_index::Edge;
use bbox_indexing::projects::ProjectRegistry;
use bbox_knowledge::knowledge::Knowledge;
use bbox_packets::Packets;
use bbox_stores::roadmap::Roadmap;
use bbox_threads::notes::Notes;
use bbox_threads::threads::Threads;
use bbox_whiteboards::whiteboards::WhiteboardRegistry;
use parking_lot::RwLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityView {
    pub ref_string: String,
    pub entity_type: EntityType,
    pub properties: BTreeMap<String, String>,
    pub neighborhood: Neighborhood,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitySchemaView {
    pub entity_type: EntityType,
    pub properties: Vec<String>,
    pub edge_families: Vec<String>,
    pub filterable_fields: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Neighborhood {
    pub forward: Vec<Edge>,
    pub reverse: Vec<Edge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextHop {
    pub edge_family_name: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeFamilyExpectation {
    pub family_name: String,
    pub min_count: Option<usize>,
    pub max_count: Option<usize>,
    pub required: bool,
}

/// Borrowed view over the corpus stores the entity providers read.
/// The daemon builds one from `SharedState` (`SharedState::corpus_stores`);
/// the struct itself names only peeled store types so the provider layer
/// sits below the daemon core in the crate DAG.
#[derive(Clone, Copy)]
pub struct CorpusStores<'a> {
    pub idx: &'a RwLock<TranscriptIndex>,
    pub kb: &'a RwLock<Knowledge>,
    pub roadmap: &'a RwLock<Roadmap>,
    pub threads: &'a RwLock<Threads>,
    pub notes: &'a RwLock<Notes>,
    pub projects: &'a RwLock<ProjectRegistry>,
    pub packets: &'a RwLock<Packets>,
    pub artifacts: &'a RwLock<ArtifactCatalog>,
    pub whiteboards: &'a WhiteboardRegistry,
    pub store_dir: &'a std::path::Path,
}

pub struct ProviderContext<'a> {
    stores: Option<CorpusStores<'a>>,
    /// Request-local knowledge visibility view. When present, knowledge
    /// providers must use it instead of the daemon's mutable aggregate store.
    knowledge_view: Option<&'a Knowledge>,
    /// Opaque daemon extension: providers registered via
    /// `register_extra_providers` (task, brofile) downcast this to the
    /// daemon state type they were registered with. Keeps per-call state
    /// without this crate naming the daemon's types.
    ext: Option<&'a (dyn std::any::Any + Send + Sync)>,
    /// Edge sidecar, when the call site already holds a read guard.
    /// Edge-projected vertex types (symbol, symbol_v2) have no entity doc
    /// of their own — the indexer derives only their edges (gap-496fe07f)
    /// — so their existence check falls back to edge participation when
    /// this is present. Absent → those providers keep the strict
    /// indexed-doc requirement.
    edges: Option<&'a bbox_edge_index::edge_index::EdgeIndex>,
    /// Request-pinned Tantivy searcher. Code-source reads pair this with the
    /// pinned selector map and edge sidecar so provider properties and labels
    /// cannot reopen a newer reader mid-request.
    searcher: Option<&'a tantivy::Searcher>,
}

impl<'a> ProviderContext<'a> {
    pub fn new(stores: CorpusStores<'a>) -> Self {
        Self {
            stores: Some(stores),
            knowledge_view: None,
            ext: None,
            edges: None,
            searcher: None,
        }
    }

    pub fn new_with_ext(
        stores: CorpusStores<'a>,
        ext: &'a (dyn std::any::Any + Send + Sync),
    ) -> Self {
        Self {
            stores: Some(stores),
            knowledge_view: None,
            ext: Some(ext),
            edges: None,
            searcher: None,
        }
    }

    pub fn with_edge_index(mut self, edges: &'a bbox_edge_index::edge_index::EdgeIndex) -> Self {
        self.edges = Some(edges);
        self
    }

    pub fn with_knowledge_view(mut self, knowledge: &'a Knowledge) -> Self {
        self.knowledge_view = Some(knowledge);
        self
    }

    pub fn with_searcher(mut self, searcher: &'a tantivy::Searcher) -> Self {
        self.searcher = Some(searcher);
        self
    }

    pub fn empty_for_tests() -> Self {
        Self {
            stores: None,
            knowledge_view: None,
            ext: None,
            edges: None,
            searcher: None,
        }
    }

    pub fn stores(&self) -> Option<&CorpusStores<'a>> {
        self.stores.as_ref()
    }

    pub fn knowledge_view(&self) -> Option<&Knowledge> {
        self.knowledge_view
    }

    pub fn ext(&self) -> Option<&'a (dyn std::any::Any + Send + Sync)> {
        self.ext
    }

    pub fn edge_index(&self) -> Option<&'a bbox_edge_index::edge_index::EdgeIndex> {
        self.edges
    }

    pub fn indexed_entity_properties(
        &self,
        entity_id: &str,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let Some(stores) = self.stores() else {
            return Ok(None);
        };
        let index = stores.idx.read();
        match self.searcher {
            Some(searcher) => index.entity_properties_with_searcher(entity_id, searcher),
            None => index.entity_properties(entity_id),
        }
    }
}

pub trait InspectableEntityProvider: Send + Sync {
    fn entity_type(&self) -> EntityType;
    fn owns_ref(&self, r: &EntityRef) -> bool;
    fn handles_virtual(&self) -> bool {
        false
    }

    /// Load scalar/entity-specific properties from the provider's backing
    /// store. The returned `EntityView.neighborhood` is intentionally empty;
    /// callers populate it from `EdgeIndex` before rendering or calling
    /// `recommended_next_hops`.
    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView>;
    fn schema(&self) -> EntitySchemaView;
    fn expected_edge_families(&self, r: &EntityRef) -> Vec<EdgeFamilyExpectation>;

    fn recommended_next_hops(
        &self,
        entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop>;

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String>;
}

pub fn provider_for(entity_type: EntityType) -> &'static dyn InspectableEntityProvider {
    registry()
        .iter()
        .find(|provider| provider.entity_type() == entity_type)
        .map(|provider| provider.as_ref())
        .expect("provider registry must cover every EntityType")
}

pub fn all_providers() -> &'static [Box<dyn InspectableEntityProvider>] {
    registry().as_slice()
}

/// Daemon-side providers (task, brofile, ...) registered before the first
/// registry use. The daemon owns types this crate must not name; it hands
/// them in at boot (and test setup), and `registry()` drains them into the
/// static provider set on first access.
static EXTRA_PROVIDERS: std::sync::Mutex<Vec<Box<dyn InspectableEntityProvider>>> =
    std::sync::Mutex::new(Vec::new());

pub fn register_extra_providers(extras: Vec<Box<dyn InspectableEntityProvider>>) {
    let mut pending = EXTRA_PROVIDERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for provider in extras {
        if pending
            .iter()
            .all(|registered| registered.entity_type() != provider.entity_type())
        {
            pending.push(provider);
        }
    }
}

fn registry() -> &'static Vec<Box<dyn InspectableEntityProvider>> {
    static REGISTRY: OnceLock<Vec<Box<dyn InspectableEntityProvider>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut providers: Vec<Box<dyn InspectableEntityProvider>> = vec![
            Box::new(knowledge::KnowledgeProvider),
            Box::new(knowledge::ProvisionalKnowledgeProvider),
            Box::new(system_memory::SystemMemoryProvider),
            Box::new(file::FileProvider),
            Box::new(project_file::ProjectFileProvider),
            Box::new(project_file::ProjectFileV2Provider),
            Box::new(transcript::TranscriptProvider),
            Box::new(session::SessionProvider),
            Box::new(thread::ThreadProvider),
            Box::new(note::NoteProvider),
            Box::new(symbol::SymbolProvider),
            Box::new(symbol::SymbolV2Provider),
            Box::new(whiteboard::WhiteboardProvider),
            Box::new(commit::CommitProvider),
            Box::new(virtual_bash_call::BashCallProvider),
            Box::new(agent::AgentProvider),
            Box::new(packet::PacketProvider),
            Box::new(artifact::ArtifactProvider),
            Box::new(roadmap_item::RoadmapItemProvider),
        ];
        providers.append(
            &mut EXTRA_PROVIDERS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        providers
    })
}

pub fn ensure_type(r: &EntityRef, ty: EntityType) -> Result<()> {
    if r.entity_type() != ty {
        bail!("provider for {ty} cannot inspect {}", r.entity_type());
    }
    Ok(())
}

pub fn empty_neighborhood_view(r: &EntityRef, properties: BTreeMap<String, String>) -> EntityView {
    EntityView {
        ref_string: r.to_string(),
        entity_type: r.entity_type(),
        properties,
        neighborhood: Neighborhood::default(),
    }
}

pub fn schema(
    entity_type: EntityType,
    properties: &[&str],
    edge_families: &[&str],
    filterable_fields: &[&str],
) -> EntitySchemaView {
    EntitySchemaView {
        entity_type,
        properties: properties
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        edge_families: edge_families
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        filterable_fields: filterable_fields
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
    }
}

pub fn expected(family_name: &str, required: bool) -> EdgeFamilyExpectation {
    EdgeFamilyExpectation {
        family_name: family_name.to_string(),
        min_count: required.then_some(1),
        max_count: None,
        required,
    }
}

pub fn next_hops(neighborhood: &Neighborhood, families: &[&str]) -> Vec<NextHop> {
    families
        .iter()
        .map(|family| {
            let count = neighborhood
                .forward
                .iter()
                .chain(neighborhood.reverse.iter())
                .filter(|edge| edge.kind == *family)
                .count();
            NextHop {
                edge_family_name: (*family).to_string(),
                count,
            }
        })
        .collect()
}

pub fn truncate_label(value: impl AsRef<str>) -> String {
    let value = value.as_ref().trim();
    let mut out = String::new();
    for ch in value.chars() {
        if out.len() + ch.len_utf8() > 80 {
            break;
        }
        out.push(ch);
    }
    out
}
