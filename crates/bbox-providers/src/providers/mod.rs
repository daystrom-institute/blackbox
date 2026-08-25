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
    /// Schema-declared and schema-derived next-hop hints for this entity's
    /// type, filled by providers whose entities have a declared schema to read
    /// them from (today: project graph vertices). Empty everywhere else, so a
    /// provider that only counts edge families keeps its existing behavior.
    pub next_hop_hints: Vec<NextHopHint>,
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

/// Which way a recommended hop is followed. `None` on a `NextHop` means the
/// recommendation is direction-blind: the count spans both directions and the
/// consumer has nothing better to suggest than "look at this family".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NextHopDirection {
    Out,
    In,
}

impl NextHopDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextHop {
    pub edge_family_name: String,
    pub count: usize,
    /// Set when the hop is direction-aware, so a consumer can render (and a
    /// caller can pass back) `direction=out` / `direction=in` instead of
    /// re-deriving it.
    pub direction: Option<NextHopDirection>,
    /// Human-meaningful name for the hop, when the substrate has one. Only
    /// authored schema hints carry a label.
    pub label: Option<String>,
    /// Whether this hop came from an AUTHORED hint rather than being derived or
    /// observed. Authored hops are never dropped by a consumer's display cap:
    /// the author said they matter.
    pub authored: bool,
}

/// One next-hop hint attached to an [`EntityView`], projected from whatever
/// schema the owning provider reads. Deliberately provider-local rather than
/// re-exported from the graph crate: the provider layer sits below the graph
/// store in the crate DAG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextHopHint {
    pub edge_family_name: String,
    pub direction: NextHopDirection,
    pub label: Option<String>,
    pub authored: bool,
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
    /// INVARIANT: provider code may run on a thread that ALREADY holds a
    /// shared guard on this lock (the search tools pass
    /// `&state.idx.read()` into hybrid/discover and the providers are
    /// invoked while that guard lives). Every acquisition inside this
    /// crate must therefore be `read_recursive()`, never `read()`: a
    /// plain `read()` parks behind a queued writer, and with the outer
    /// guard held that writer can never be granted, deadlocking the
    /// whole index plane (cage incident 2026-08-25:
    /// hybrid-search labeling vs `republish_code_read_view`'s
    /// `idx.write()`). Regression test:
    /// `provider_reads_do_not_deadlock_behind_queued_idx_writer` in
    /// `src/server/routes.rs`.
    pub idx: &'a RwLock<TranscriptIndex>,
    pub kb: &'a RwLock<Knowledge>,
    pub roadmap: &'a RwLock<Roadmap>,
    pub threads: &'a RwLock<Threads>,
    pub notes: &'a RwLock<Notes>,
    /// Injected project authority. Providers enumerate records through it
    /// rather than reading the registry (or `projects.json`) directly.
    pub projects: &'a dyn bbox_corpus_core::project_record::ProjectRecordsProvider,
    pub checkout_registry: &'a RwLock<bbox_indexing::checkout_registry::CheckoutRegistry>,
    pub checkout_access: &'a bbox_indexing::checkout_access::CheckoutAccessBroker,
    /// The runtime project authority the daemon selected at startup, handed
    /// to providers explicitly.
    pub project_authority: ProviderProjectAuthority<'a>,
    pub packets: &'a RwLock<Packets>,
    pub artifacts: &'a RwLock<ArtifactCatalog>,
    pub whiteboards: &'a WhiteboardRegistry,
    /// Installed published project-graph views: the source of the graph
    /// embedding route's coverage (the embed projection lives only in the
    /// in-memory accepted generation, never in the word index).
    pub project_graph_views: &'a RwLock<bbox_indexing::project_graph_view::ProjectGraphViewCatalog>,
    pub store_dir: &'a std::path::Path,
}

/// Which project authority the process is running under.
///
/// Passed explicitly because it cannot be inferred: a catalog deployment
/// whose projects all carry a base attachment produces a record projection
/// indistinguishable from a bridge deployment's, so reading the mode off
/// record shape would be a guess that happens to be right most of the time
/// (Phase 5 plan section 5.1, section 11).
#[derive(Clone, Copy)]
pub enum ProviderProjectAuthority<'a> {
    Bridge,
    Catalog {
        catalog: &'a bbox_indexing::project_catalog_store::ProjectCatalogStore,
    },
}

/// Path-free request authority for relative checkout-backed providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCheckoutSelection {
    pub project_id: String,
    pub checkout_id: String,
    pub published_scope: bbox_corpus_core::identity::PublishedScope,
}

pub struct ProviderContext<'a> {
    stores: Option<CorpusStores<'a>>,
    checkout_selection: Option<ProviderCheckoutSelection>,
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
    project_graph_resolver: Option<&'a dyn ProjectGraphEntityResolver>,
    provisional: Option<&'a str>,
}

pub trait ProjectGraphEntityResolver: Send + Sync {
    fn resolve_entity(&self, r: &EntityRef, provisional: Option<&str>) -> Result<EntityView>;

    /// Whether traversal may step into the graph lane that owns `r`
    /// (unified-retrieval design 5.2).
    ///
    /// Graph selection precedes neighbor enumeration: an unreadable graph is
    /// not walked, its vertices never enter the frontier, and no truncated
    /// path or count discloses that it exists. The check is live per hop
    /// against the resolver's current view, never a stale index stamp. The
    /// default admits everything, so contexts without a project-graph
    /// resolver keep their existing traversal behavior.
    fn traversal_admits(&self, r: &EntityRef, provisional: Option<&str>) -> bool {
        let _ = (r, provisional);
        true
    }

    /// Tenant-owned evidence bindings touching `r`, in both directions.
    ///
    /// Distinct from `resolve_entity` because an evidence binding can name an
    /// endpoint this resolver does not own: a project file or a knowledge
    /// entry is a legal endpoint, and the edge has to surface on THAT entity's
    /// neighborhood too, not only on the graph vertex at the other end.
    /// Returning edges here is how a non-graph entity gets them without every
    /// provider learning about evidence.
    ///
    /// Each edge carries the `evidence.*` metadata family, with any endpoint
    /// the resolver could not observe left as `unresolved` for the read plane
    /// to refine.
    fn evidence_edges(&self, r: &EntityRef, provisional: Option<&str>) -> Vec<Edge> {
        let _ = (r, provisional);
        Vec::new()
    }
}

impl<'a> ProviderContext<'a> {
    pub fn new(stores: CorpusStores<'a>) -> Self {
        Self {
            stores: Some(stores),
            checkout_selection: None,
            knowledge_view: None,
            ext: None,
            edges: None,
            searcher: None,
            project_graph_resolver: None,
            provisional: None,
        }
    }

    pub fn new_with_ext(
        stores: CorpusStores<'a>,
        ext: &'a (dyn std::any::Any + Send + Sync),
    ) -> Self {
        Self {
            stores: Some(stores),
            checkout_selection: None,
            knowledge_view: None,
            ext: Some(ext),
            edges: None,
            searcher: None,
            project_graph_resolver: None,
            provisional: None,
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

    pub fn with_checkout_selection(mut self, selection: ProviderCheckoutSelection) -> Self {
        self.checkout_selection = Some(selection);
        self
    }

    pub fn with_searcher(mut self, searcher: &'a tantivy::Searcher) -> Self {
        self.searcher = Some(searcher);
        self
    }

    pub fn with_project_graph_resolver(
        mut self,
        resolver: &'a dyn ProjectGraphEntityResolver,
        provisional: Option<&'a str>,
    ) -> Self {
        self.project_graph_resolver = Some(resolver);
        self.provisional = provisional;
        self
    }

    pub fn empty_for_tests() -> Self {
        Self {
            stores: None,
            checkout_selection: None,
            knowledge_view: None,
            ext: None,
            edges: None,
            searcher: None,
            project_graph_resolver: None,
            provisional: None,
        }
    }

    pub fn stores(&self) -> Option<&CorpusStores<'a>> {
        self.stores.as_ref()
    }

    pub fn checkout_selection(&self) -> Option<&ProviderCheckoutSelection> {
        self.checkout_selection.as_ref()
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

    pub fn project_graph_resolver(&self) -> Option<&'a dyn ProjectGraphEntityResolver> {
        self.project_graph_resolver
    }

    pub fn provisional_mode(&self) -> Option<&'a str> {
        self.provisional
    }

    /// Whether a traversal may step into `r`'s graph lane. Non-graph refs
    /// stay admissible; the bound resolver owns the live per-lane policy
    /// check, including the plane mode this context was built with.
    pub fn graph_lane_admitted(&self, r: &EntityRef) -> bool {
        self.project_graph_resolver
            .is_none_or(|resolver| resolver.traversal_admits(r, self.provisional))
    }

    /// Evidence bindings touching `r`. Empty when no project-graph resolver is
    /// bound, which is the correct reading for a context that cannot see the
    /// project graph at all.
    pub fn evidence_edges(&self, r: &EntityRef) -> Vec<Edge> {
        self.project_graph_resolver
            .map(|resolver| resolver.evidence_edges(r, self.provisional))
            .unwrap_or_default()
    }

    pub fn indexed_entity_properties(
        &self,
        entity_id: &str,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let Some(stores) = self.stores() else {
            return Ok(None);
        };
        // read_recursive, not read: see the invariant on `CorpusStores::idx`.
        let index = stores.idx.read_recursive();
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
        next_hop_hints: Vec::new(),
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
                direction: None,
                label: None,
                authored: false,
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
