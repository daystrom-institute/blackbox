#![allow(dead_code)] // D1 lands the provider surface; D2 wires public consumers.

pub mod agent;
pub mod artifact;
pub mod brofile;
pub mod commit;
pub mod file;
pub mod knowledge;
pub mod note;
pub mod packet;
pub mod project_file;
pub mod roadmap_item;
pub mod session;
pub mod symbol;
pub mod thread;
pub mod transcript;
pub mod virtual_bash_call;
pub mod virtual_task;
pub mod whiteboard;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{bail, Result};

use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

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

pub(crate) struct ProviderContext<'a> {
    state: Option<&'a crate::SharedState>,
}

impl<'a> ProviderContext<'a> {
    pub(crate) fn new(state: &'a crate::SharedState) -> Self {
        Self { state: Some(state) }
    }

    #[cfg(test)]
    pub(crate) fn empty_for_tests() -> Self {
        Self { state: None }
    }

    pub(crate) fn state(&self) -> Option<&'a crate::SharedState> {
        self.state
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

fn registry() -> &'static Vec<Box<dyn InspectableEntityProvider>> {
    static REGISTRY: OnceLock<Vec<Box<dyn InspectableEntityProvider>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        vec![
            Box::new(knowledge::KnowledgeProvider),
            Box::new(file::FileProvider),
            Box::new(project_file::ProjectFileProvider),
            Box::new(project_file::ProjectFileV2Provider),
            Box::new(transcript::TranscriptProvider),
            Box::new(session::SessionProvider),
            Box::new(thread::ThreadProvider),
            Box::new(note::NoteProvider),
            Box::new(symbol::SymbolProvider),
            Box::new(symbol::SymbolV2Provider),
            Box::new(brofile::BrofileProvider),
            Box::new(whiteboard::WhiteboardProvider),
            Box::new(commit::CommitProvider),
            Box::new(virtual_task::TaskProvider),
            Box::new(virtual_bash_call::BashCallProvider),
            Box::new(agent::AgentProvider),
            Box::new(packet::PacketProvider),
            Box::new(artifact::ArtifactProvider),
            Box::new(roadmap_item::RoadmapItemProvider),
        ]
    })
}

pub(crate) fn ensure_type(r: &EntityRef, ty: EntityType) -> Result<()> {
    if r.entity_type() != ty {
        bail!("provider for {ty} cannot inspect {}", r.entity_type());
    }
    Ok(())
}

pub(crate) fn empty_neighborhood_view(
    r: &EntityRef,
    properties: BTreeMap<String, String>,
) -> EntityView {
    EntityView {
        ref_string: r.to_string(),
        entity_type: r.entity_type(),
        properties,
        neighborhood: Neighborhood::default(),
    }
}

pub(crate) fn schema(
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

pub(crate) fn expected(family_name: &str, required: bool) -> EdgeFamilyExpectation {
    EdgeFamilyExpectation {
        family_name: family_name.to_string(),
        min_count: required.then_some(1),
        max_count: None,
        required,
    }
}

pub(crate) fn next_hops(neighborhood: &Neighborhood, families: &[&str]) -> Vec<NextHop> {
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

pub(crate) fn truncate_label(value: impl AsRef<str>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_refs() -> Vec<&'static str> {
        vec![
            "knowledge:abc12345",
            "file:README.md",
            "project_file:proj1234:relhash:chunkhash:0",
            "transcript:claude:session123:42:0",
            "session:claude:session123",
            "thread:thread-12345678",
            "note:note-12345678",
            "symbol:proj1234:crate::Type::method:defhash",
            "brofile:auditor",
            "whiteboard:board-12345678",
            "commit:repo1234:abcdef1234567890",
            "task:task-12345678",
            "bash_call:session123:7",
            "agent:code-reviewer@v3",
            "packet:domain:phase-decompose/triage",
            "artifact:packet/phase-decompose/triage@1",
        ]
    }

    #[test]
    fn registry_dispatches_every_entity_type() {
        let ctx = ProviderContext::empty_for_tests();
        for raw in sample_refs() {
            let parsed = EntityRef::parse(raw).unwrap();
            assert_eq!(parsed.render(), raw);
            let provider = provider_for(parsed.entity_type());
            assert!(provider.owns_ref(&parsed));
            assert_eq!(provider.handles_virtual(), parsed.is_virtual());
            let view = provider.get_entity(&ctx, &parsed).unwrap();
            assert_eq!(view.entity_type, parsed.entity_type());
        }
    }

    #[test]
    fn compact_labels_fit_inline_budget() {
        let ctx = ProviderContext::empty_for_tests();
        for raw in sample_refs() {
            let parsed = EntityRef::parse(raw).unwrap();
            let provider = provider_for(parsed.entity_type());
            let label = provider.compact_label(&ctx, &parsed).unwrap();
            assert!(label.len() <= 80, "{raw}: {label}");
        }
    }

    #[test]
    fn registry_covers_entity_type_enum() {
        for entity_type in EntityType::ALL {
            provider_for(entity_type);
        }
        assert_eq!(all_providers().len(), EntityType::ALL.len());
    }
}
