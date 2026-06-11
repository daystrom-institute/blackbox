use std::collections::BTreeMap;

use anyhow::Result;

use bbox_corpus_core::entity_ref::EntityRef;
use crate::providers::{self, EntityView, ProviderContext};

const LABEL_KEYS: &[&str] = &["title", "name", "qualified_name", "topic", "subject"];

pub fn load(ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
    providers::provider_for(r.entity_type()).get_entity(ctx, r)
}

pub fn label_from_properties(properties: &BTreeMap<String, String>) -> Option<String> {
    LABEL_KEYS
        .iter()
        .find_map(|key| properties.get(*key))
        .filter(|value| !value.trim().is_empty())
        .map(providers::truncate_label)
}

pub fn compact_label(
    ctx: &ProviderContext<'_>,
    r: &EntityRef,
    loaded: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    loaded
        .and_then(label_from_properties)
        .or_else(|| {
            load(ctx, r)
                .ok()
                .and_then(|entity| label_from_properties(&entity.properties))
        })
        .or_else(|| providers::provider_for(r.entity_type()).compact_label(ctx, r))
}
