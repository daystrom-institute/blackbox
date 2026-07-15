//! Typed comparison state for context the model is expected to know.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{FragmentRole, TextMessage};

pub const WORLD_STATE_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousSection<'a, T> {
    Absent,
    Unknown,
    Known(&'a T),
}

pub trait WorldStateSection: Send + Sync + 'static {
    const ID: &'static str;
    type Snapshot: Serialize + DeserializeOwned + Clone + Send + Sync + 'static;

    fn snapshot(&self) -> Self::Snapshot;

    fn render_diff(&self, previous: PreviousSection<'_, Self::Snapshot>) -> Vec<TextMessage>;

    /// Select the snapshot persisted after rendering. Most sections persist
    /// the current value. Sections with an intentionally non-restorable input
    /// can preserve a comparison baseline from the previous snapshot here.
    fn persisted_snapshot(
        &self,
        _previous: PreviousSection<'_, Self::Snapshot>,
        current: &Self::Snapshot,
    ) -> Self::Snapshot {
        current.clone()
    }

    fn matches_legacy_fragment(&self, _role: FragmentRole, _text: &str) -> bool {
        false
    }

    fn matches_retained_fragment(&self, _role: FragmentRole, _text: &str) -> bool {
        false
    }

    fn matches_legacy_fragments(&self, retained: &[RetainedFragment]) -> bool {
        retained
            .iter()
            .any(|fragment| self.matches_legacy_fragment(fragment.role, &fragment.text))
    }

    fn matches_retained_fragments(&self, retained: &[RetainedFragment]) -> bool {
        retained
            .iter()
            .any(|fragment| self.matches_retained_fragment(fragment.role, &fragment.text))
    }

    /// True when retained-history absence is meaningful for this section.
    fn reconciles_retained_fragments(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedFragment {
    pub role: FragmentRole,
    pub text: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedWorldState {
    v: u16,
    #[serde(default)]
    sections: BTreeMap<String, Value>,
}

enum RestoreState {
    Absent,
    Unknown,
    Compatible(BTreeMap<String, Value>),
}

trait ErasedSection: Send + Sync {
    fn id(&self) -> &'static str;
    fn render(
        &self,
        restore: &RestoreState,
        retained: &[RetainedFragment],
    ) -> anyhow::Result<(Vec<TextMessage>, Value)>;
}

struct SectionAdapter<S>(S);

impl<S> ErasedSection for SectionAdapter<S>
where
    S: WorldStateSection,
{
    fn id(&self) -> &'static str {
        S::ID
    }

    fn render(
        &self,
        restore: &RestoreState,
        retained: &[RetainedFragment],
    ) -> anyhow::Result<(Vec<TextMessage>, Value)> {
        let current = self.0.snapshot();
        let retained_match = self.0.matches_retained_fragments(retained);
        let legacy_match = self.0.matches_legacy_fragments(retained);
        let decoded = match restore {
            RestoreState::Absent if legacy_match => Some(current.clone()),
            RestoreState::Absent => None,
            RestoreState::Unknown => {
                let messages = self.0.render_diff(PreviousSection::Unknown);
                return Ok((messages, normalized_json(&current)?));
            }
            RestoreState::Compatible(sections) => match sections.get(S::ID) {
                Some(value) => match serde_json::from_value::<S::Snapshot>(value.clone()) {
                    Ok(value) => Some(value),
                    Err(_) => {
                        let messages = self.0.render_diff(PreviousSection::Unknown);
                        return Ok((messages, normalized_json(&current)?));
                    }
                },
                None if legacy_match => Some(current.clone()),
                None if retained_match => {
                    let messages = self.0.render_diff(PreviousSection::Unknown);
                    return Ok((messages, normalized_json(&current)?));
                }
                None => None,
            },
        };
        let previous = match decoded.as_ref() {
            Some(_)
                if self.0.reconciles_retained_fragments() && !retained_match && !legacy_match =>
            {
                PreviousSection::Absent
            }
            Some(value) => PreviousSection::Known(value),
            None => PreviousSection::Absent,
        };
        let messages = self.0.render_diff(previous.clone());
        let persisted = self.0.persisted_snapshot(previous, &current);
        Ok((messages, normalized_json(&persisted)?))
    }
}

/// Extract model-visible user/developer text from any current transport
/// snapshot. Inline compaction summaries are excluded: a summary mentioning a
/// marker is not proof that the original structured fragment survived.
pub fn retained_fragments_from_snapshot(snapshot: &Value) -> Vec<RetainedFragment> {
    let items = snapshot
        .get("input")
        .and_then(Value::as_array)
        .or_else(|| snapshot.as_array());
    let Some(items) = items else {
        return Vec::new();
    };

    let mut retained = Vec::new();
    for item in items {
        let role = match item.get("role").and_then(Value::as_str) {
            Some("user") => FragmentRole::User,
            Some("developer") | Some("system") => FragmentRole::Developer,
            _ => continue,
        };
        collect_text_fragments(item.get("content"), role, &mut retained);
    }
    retained
}

fn collect_text_fragments(
    content: Option<&Value>,
    role: FragmentRole,
    retained: &mut Vec<RetainedFragment>,
) {
    match content {
        Some(Value::String(text)) => push_retained_text(role, text, retained),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("text") | Some("input_text") | Some("output_text")
                ) && let Some(text) = block.get("text").and_then(Value::as_str)
                {
                    push_retained_text(role, text, retained);
                }
            }
        }
        _ => {}
    }
}

fn push_retained_text(role: FragmentRole, text: &str, retained: &mut Vec<RetainedFragment>) {
    if text.starts_with("[Earlier conversation compacted to a summary]") {
        return;
    }
    retained.push(RetainedFragment {
        role,
        text: text.to_string(),
    });
}

/// Prepend contextual blocks to the most recent native user message. This is
/// used only for overflow compaction, where the task was already appended
/// before the provider rejected the request. It preserves the invariant that
/// the task text remains the final content in that user message.
pub fn prepend_blocks_to_last_user(snapshot: &mut Value, blocks: &[String]) -> bool {
    if blocks.is_empty() {
        return true;
    }
    let items = if snapshot.is_array() {
        snapshot.as_array_mut()
    } else {
        snapshot.get_mut("input").and_then(Value::as_array_mut)
    };
    let Some(items) = items else {
        return false;
    };
    let Some(message) = items.iter_mut().rev().find(|item| {
        item.get("role").and_then(Value::as_str) == Some("user")
            && item.get("type").and_then(Value::as_str) != Some("compaction_summary")
    }) else {
        return false;
    };

    let Some(content) = message.get_mut("content") else {
        return false;
    };
    match content {
        Value::String(text) => {
            let mut joined = blocks.join("\n\n");
            if !text.is_empty() {
                joined.push_str("\n\n");
                joined.push_str(text);
            }
            *text = joined;
            true
        }
        Value::Array(existing) => {
            let block_type = existing
                .iter()
                .find_map(|item| item.get("type").and_then(Value::as_str))
                .filter(|kind| matches!(*kind, "text" | "input_text"))
                .unwrap_or("text");
            let mut prefixed: Vec<Value> = blocks
                .iter()
                .map(|text| serde_json::json!({"type": block_type, "text": text}))
                .collect();
            prefixed.append(existing);
            *existing = prefixed;
            true
        }
        _ => false,
    }
}

/// Ordered section registry plus tolerant restore/reconciliation logic.
pub struct WorldStateCoordinator {
    restore: RestoreState,
    retained: Vec<RetainedFragment>,
    sections: Vec<Box<dyn ErasedSection>>,
    ids: BTreeSet<&'static str>,
}

impl WorldStateCoordinator {
    pub fn from_side(value: &Value, retained: Vec<RetainedFragment>) -> Self {
        let restore = if value.is_null() {
            RestoreState::Absent
        } else {
            match serde_json::from_value::<PersistedWorldState>(value.clone()) {
                Ok(persisted) if persisted.v == WORLD_STATE_VERSION => {
                    RestoreState::Compatible(persisted.sections)
                }
                _ => RestoreState::Unknown,
            }
        };
        Self {
            restore,
            retained,
            sections: Vec::new(),
            ids: BTreeSet::new(),
        }
    }

    pub fn register<S: WorldStateSection>(&mut self, section: S) -> anyhow::Result<()> {
        if S::ID.is_empty() {
            anyhow::bail!("world-state section id must not be empty");
        }
        if !self.ids.insert(S::ID) {
            anyhow::bail!("duplicate world-state section id {}", S::ID);
        }
        self.sections.push(Box::new(SectionAdapter(section)));
        Ok(())
    }

    pub fn render_and_snapshot(&self) -> anyhow::Result<(Vec<TextMessage>, Value)> {
        let mut messages = Vec::new();
        let mut sections = BTreeMap::new();
        for section in &self.sections {
            let (mut rendered, snapshot) = section.render(&self.restore, &self.retained)?;
            messages.append(&mut rendered);
            sections.insert(section.id().to_string(), snapshot);
        }
        let persisted = PersistedWorldState {
            v: WORLD_STATE_VERSION,
            sections,
        };
        Ok((messages, serde_json::to_value(persisted)?))
    }
}

fn normalized_json<T: Serialize>(value: &T) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(value)?;
    remove_null_object_fields(&mut value);
    Ok(value)
}

fn remove_null_object_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, value| !value.is_null());
            for value in map.values_mut() {
                remove_null_object_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                remove_null_object_fields(value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Clone, Serialize, Deserialize)]
    struct Snapshot {
        value: String,
        optional: Option<String>,
    }

    struct Section {
        id_value: String,
    }

    impl WorldStateSection for Section {
        const ID: &'static str = "section";
        type Snapshot = Snapshot;

        fn snapshot(&self) -> Self::Snapshot {
            Snapshot {
                value: self.id_value.clone(),
                optional: None,
            }
        }

        fn render_diff(&self, previous: PreviousSection<'_, Self::Snapshot>) -> Vec<TextMessage> {
            let label = match previous {
                PreviousSection::Absent => "absent",
                PreviousSection::Unknown => "unknown",
                PreviousSection::Known(previous) if previous.value == self.id_value => {
                    return Vec::new();
                }
                PreviousSection::Known(_) => "changed",
            };
            vec![TextMessage {
                role: FragmentRole::User,
                text_blocks: vec![format!("{label}:{}", self.id_value)],
            }]
        }

        fn matches_legacy_fragment(&self, _role: FragmentRole, text: &str) -> bool {
            text.contains("legacy-section")
        }

        fn matches_retained_fragment(&self, _role: FragmentRole, text: &str) -> bool {
            text.contains("<section>")
        }

        fn reconciles_retained_fragments(&self) -> bool {
            true
        }
    }

    struct Second;

    impl WorldStateSection for Second {
        const ID: &'static str = "second";
        type Snapshot = String;

        fn snapshot(&self) -> Self::Snapshot {
            "two".into()
        }

        fn render_diff(&self, _previous: PreviousSection<'_, String>) -> Vec<TextMessage> {
            vec![TextMessage {
                role: FragmentRole::Developer,
                text_blocks: vec!["second".into()],
            }]
        }
    }

    #[test]
    fn section_order_is_registration_order_and_ids_are_unique() {
        let mut coordinator = WorldStateCoordinator::from_side(&Value::Null, Vec::new());
        coordinator
            .register(Section {
                id_value: "one".into(),
            })
            .unwrap();
        coordinator.register(Second).unwrap();
        let (messages, _) = coordinator.render_and_snapshot().unwrap();
        assert_eq!(messages[0].text_blocks, ["absent:one"]);
        assert_eq!(messages[1].text_blocks, ["second"]);
        assert!(
            coordinator
                .register(Section {
                    id_value: "duplicate".into(),
                })
                .is_err()
        );
    }

    #[test]
    fn malformed_and_future_snapshots_restore_as_unknown() {
        for side in [
            serde_json::json!({"v": 1, "sections": {"section": 9}}),
            serde_json::json!({"v": 99, "sections": {}}),
            serde_json::json!({"bad": true}),
        ] {
            let mut coordinator = WorldStateCoordinator::from_side(&side, Vec::new());
            coordinator
                .register(Section {
                    id_value: "one".into(),
                })
                .unwrap();
            let (messages, _) = coordinator.render_and_snapshot().unwrap();
            assert_eq!(messages[0].text_blocks, ["unknown:one"]);
        }
    }

    #[test]
    fn known_unchanged_section_emits_nothing_and_strips_nulls() {
        let side = serde_json::json!({
            "v": 1,
            "sections": {"section": {"value": "one"}}
        });
        let retained = vec![RetainedFragment {
            role: FragmentRole::User,
            text: "<section>one</section>".into(),
        }];
        let mut coordinator = WorldStateCoordinator::from_side(&side, retained);
        coordinator
            .register(Section {
                id_value: "one".into(),
            })
            .unwrap();
        let (messages, snapshot) = coordinator.render_and_snapshot().unwrap();
        assert!(messages.is_empty());
        assert!(snapshot["sections"]["section"].get("optional").is_none());
    }

    #[test]
    fn known_snapshot_without_retained_fragment_reemits_once() {
        let side = serde_json::json!({
            "v": 1,
            "sections": {"section": {"value": "one"}}
        });
        let mut coordinator = WorldStateCoordinator::from_side(&side, Vec::new());
        coordinator
            .register(Section {
                id_value: "one".into(),
            })
            .unwrap();
        let (messages, _) = coordinator.render_and_snapshot().unwrap();
        assert_eq!(messages[0].text_blocks, ["absent:one"]);
    }

    #[test]
    fn legacy_fragment_migrates_without_duplicate_first_turn_content() {
        let retained = vec![RetainedFragment {
            role: FragmentRole::User,
            text: "legacy-section one".into(),
        }];
        let mut coordinator = WorldStateCoordinator::from_side(
            &serde_json::json!({"v": 1, "sections": {}}),
            retained,
        );
        coordinator
            .register(Section {
                id_value: "one".into(),
            })
            .unwrap();
        let (messages, _) = coordinator.render_and_snapshot().unwrap();
        assert!(messages.is_empty());
    }
}
