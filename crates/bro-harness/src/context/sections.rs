//! Concrete ordered sections for the harness model-visible World State.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::dispatch::{CompositionStrategy, DispatchState};
use super::world_state::{PreviousSection, RetainedFragment, WorldStateSection};
use super::{
    ContextualUserFragment, EnvironmentContext, EnvironmentContextDelta, FragmentRole, TextMessage,
    TurnContextItem, UserInstructions,
};

#[derive(Clone)]
pub struct ProjectInstructionsSection {
    strategy: CompositionStrategy,
    current: ProjectInstructionsSnapshot,
    rendered: Option<String>,
    directory: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInstructionsSnapshot {
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub content_sha256: Option<String>,
    #[serde(default)]
    pub loaded_paths: Vec<String>,
    #[serde(default)]
    pub loaded_paths_sha256: Option<String>,
}

impl ProjectInstructionsSnapshot {
    pub fn from_instructions(instructions: Option<&UserInstructions>) -> Self {
        let Some(instructions) = instructions else {
            return Self::default();
        };
        let loaded_paths: Vec<String> = instructions
            .loaded_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        Self {
            directory: Some(instructions.directory.clone()),
            content_sha256: Some(sha256_hex(instructions.text.as_bytes())),
            loaded_paths_sha256: Some(hash_strings(&loaded_paths)),
            loaded_paths,
        }
    }

    fn is_present(&self) -> bool {
        self.content_sha256.is_some()
    }
}

impl ProjectInstructionsSection {
    pub fn new(
        strategy: CompositionStrategy,
        instructions: Option<&UserInstructions>,
        directory: String,
    ) -> Self {
        Self {
            strategy,
            current: ProjectInstructionsSnapshot::from_instructions(instructions),
            rendered: instructions.map(ContextualUserFragment::render),
            directory,
        }
    }
}

impl WorldStateSection for ProjectInstructionsSection {
    const ID: &'static str = "project_instructions";
    type Snapshot = ProjectInstructionsSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.current.clone()
    }

    fn render_diff(&self, previous: PreviousSection<'_, Self::Snapshot>) -> Vec<TextMessage> {
        if !self.strategy.context_rides_user_lane() {
            return Vec::new();
        }
        match (&self.rendered, previous) {
            (Some(_), PreviousSection::Known(before)) if before == &self.current => Vec::new(),
            (Some(rendered), _) => user_message(vec![rendered.clone()]),
            (None, PreviousSection::Known(before)) if before.is_present() => {
                user_message(vec![format!(
                    "<project_instructions_update>\nProject instructions are no longer loaded for {}.\n</project_instructions_update>",
                    self.directory
                )])
            }
            _ => Vec::new(),
        }
    }

    fn matches_legacy_fragment(&self, role: FragmentRole, text: &str) -> bool {
        role == FragmentRole::User
            && self
                .rendered
                .as_ref()
                .is_some_and(|rendered| text.contains(rendered))
    }

    fn matches_retained_fragment(&self, role: FragmentRole, text: &str) -> bool {
        role == FragmentRole::User
            && text.contains("# AGENTS.md instructions for ")
            && text.contains("<INSTRUCTIONS>")
            && text.contains("</INSTRUCTIONS>")
    }

    fn reconciles_retained_fragments(&self) -> bool {
        self.strategy.context_rides_user_lane() && self.current.is_present()
    }
}

#[derive(Clone)]
pub struct DispatchSection {
    strategy: CompositionStrategy,
    current: DispatchSnapshot,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchSnapshot {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub pins: Option<String>,
}

impl DispatchSnapshot {
    pub fn from_dispatch(dispatch: &DispatchState) -> Self {
        Self {
            scope: dispatch.scope_render(),
            pins: dispatch.pins_render(),
        }
    }

    pub fn from_legacy(value: &Value) -> Option<Self> {
        let snapshot = Self {
            scope: value
                .get("scope")
                .and_then(Value::as_str)
                .map(str::to_string),
            pins: value
                .get("pins")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        (snapshot.scope.is_some() || snapshot.pins.is_some()).then_some(snapshot)
    }

    fn current_fragments(&self) -> impl Iterator<Item = &String> {
        self.scope.iter().chain(self.pins.iter())
    }
}

impl DispatchSection {
    pub fn new(strategy: CompositionStrategy, dispatch: &DispatchState) -> Self {
        Self {
            strategy,
            current: DispatchSnapshot::from_dispatch(dispatch),
        }
    }
}

impl WorldStateSection for DispatchSection {
    const ID: &'static str = "dispatch";
    type Snapshot = DispatchSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.current.clone()
    }

    fn render_diff(&self, previous: PreviousSection<'_, Self::Snapshot>) -> Vec<TextMessage> {
        if !self.strategy.context_rides_user_lane() {
            return Vec::new();
        }
        let mut blocks = Vec::new();
        match previous {
            PreviousSection::Known(before) => {
                if self.current.scope.is_some() && self.current.scope != before.scope {
                    blocks.extend(self.current.scope.clone());
                }
                if self.current.pins.is_some() && self.current.pins != before.pins {
                    blocks.extend(self.current.pins.clone());
                }
            }
            PreviousSection::Absent | PreviousSection::Unknown => {
                blocks.extend(self.current.scope.clone());
                blocks.extend(self.current.pins.clone());
            }
        }
        user_message(blocks)
    }

    fn persisted_snapshot(
        &self,
        previous: PreviousSection<'_, Self::Snapshot>,
        current: &Self::Snapshot,
    ) -> Self::Snapshot {
        let mut persisted = current.clone();
        if persisted.scope.is_none()
            && let PreviousSection::Known(before) = previous
        {
            // Scope is dispatch correlation state and is deliberately absent
            // on resume. Preserve its comparison baseline without restoring it
            // into the live DispatchContext.
            persisted.scope = before.scope.clone();
        }
        persisted
    }

    fn matches_legacy_fragments(&self, retained: &[RetainedFragment]) -> bool {
        self.current.current_fragments().all(|expected| {
            retained.iter().any(|fragment| {
                fragment.role == FragmentRole::User && fragment.text.contains(expected)
            })
        })
    }

    fn matches_retained_fragments(&self, retained: &[RetainedFragment]) -> bool {
        self.current.current_fragments().all(|expected| {
            retained.iter().any(|fragment| {
                fragment.role == FragmentRole::User
                    && ((expected.starts_with("<bbox_scope>")
                        && fragment.text.contains("<bbox_scope>")
                        && fragment.text.contains("</bbox_scope>"))
                        || (expected.starts_with("<bbox_pins>")
                            && fragment.text.contains("<bbox_pins>")
                            && fragment.text.contains("</bbox_pins>")))
            })
        })
    }

    fn reconciles_retained_fragments(&self) -> bool {
        self.strategy.context_rides_user_lane()
            && (self.current.scope.is_some() || self.current.pins.is_some())
    }
}

#[derive(Clone)]
pub struct EnvironmentSection {
    strategy: CompositionStrategy,
    current: EnvironmentContext,
    full_render: String,
}

impl EnvironmentSection {
    pub fn new(strategy: CompositionStrategy, current: EnvironmentContext) -> Self {
        let full_render = current.render();
        Self {
            strategy,
            current,
            full_render,
        }
    }
}

impl WorldStateSection for EnvironmentSection {
    const ID: &'static str = "environment";
    type Snapshot = TurnContextItem;

    fn snapshot(&self) -> Self::Snapshot {
        self.current.to_turn_context_item()
    }

    fn render_diff(&self, previous: PreviousSection<'_, Self::Snapshot>) -> Vec<TextMessage> {
        if !self.strategy.context_rides_user_lane() {
            return Vec::new();
        }
        match previous {
            PreviousSection::Known(before) => {
                EnvironmentContextDelta::from_turn_context_item(before, &self.current)
                    .map(|delta| user_message(vec![delta.render()]))
                    .unwrap_or_default()
            }
            PreviousSection::Absent | PreviousSection::Unknown => {
                user_message(vec![self.full_render.clone()])
            }
        }
    }

    fn matches_legacy_fragment(&self, role: FragmentRole, text: &str) -> bool {
        role == FragmentRole::User && text.contains(&self.full_render)
    }

    fn matches_retained_fragment(&self, role: FragmentRole, text: &str) -> bool {
        role == FragmentRole::User
            && text.contains("<environment_context>")
            && text.contains("</environment_context>")
    }

    fn reconciles_retained_fragments(&self) -> bool {
        self.strategy.context_rides_user_lane()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolManifestSnapshot {
    pub visible_sha256: String,
    pub deferred_sha256: String,
    #[serde(default)]
    pub visible_tools: Vec<String>,
    #[serde(default)]
    pub deferred_tools: Vec<String>,
}

pub struct ToolManifestSection {
    snapshot: ToolManifestSnapshot,
}

impl ToolManifestSection {
    pub fn new(visible_tools: Vec<String>, deferred_tools: Vec<String>) -> Self {
        Self {
            snapshot: ToolManifestSnapshot {
                visible_sha256: hash_strings(&visible_tools),
                deferred_sha256: hash_strings(&deferred_tools),
                visible_tools,
                deferred_tools,
            },
        }
    }
}

impl WorldStateSection for ToolManifestSection {
    const ID: &'static str = "tool_manifest";
    type Snapshot = ToolManifestSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot.clone()
    }

    fn render_diff(&self, _previous: PreviousSection<'_, Self::Snapshot>) -> Vec<TextMessage> {
        // Tool definitions and the deferred manifest retain their existing
        // transport-owned placement. World State records identity only.
        Vec::new()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAvailabilitySnapshot {
    pub tool: bool,
    pub corpus: bool,
    pub atoms: bool,
    pub refactor: bool,
    pub execution: bool,
    pub collaboration: bool,
}

pub struct ServiceAvailabilitySection {
    snapshot: ServiceAvailabilitySnapshot,
}

impl ServiceAvailabilitySection {
    pub fn new(snapshot: ServiceAvailabilitySnapshot) -> Self {
        Self { snapshot }
    }
}

impl WorldStateSection for ServiceAvailabilitySection {
    const ID: &'static str = "service_availability";
    type Snapshot = ServiceAvailabilitySnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot.clone()
    }

    fn render_diff(&self, _previous: PreviousSection<'_, Self::Snapshot>) -> Vec<TextMessage> {
        // Availability is already expressed by the admitted tool array. The
        // typed snapshot prevents a second prose disclosure channel.
        Vec::new()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedLensesSnapshot {
    #[serde(default)]
    pub ids: Vec<String>,
    pub shadow_only: bool,
}

pub struct SelectedLensesSection {
    snapshot: SelectedLensesSnapshot,
}

impl SelectedLensesSection {
    pub fn shadow(ids: Vec<String>) -> Self {
        Self {
            snapshot: SelectedLensesSnapshot {
                ids,
                shadow_only: true,
            },
        }
    }
}

impl WorldStateSection for SelectedLensesSection {
    const ID: &'static str = "selected_lenses";
    type Snapshot = SelectedLensesSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot.clone()
    }

    fn render_diff(&self, _previous: PreviousSection<'_, Self::Snapshot>) -> Vec<TextMessage> {
        // Stage F is metrics-only. A selection never changes prompt disclosure.
        Vec::new()
    }
}

pub fn migrate_legacy_world_state(prior_side: &Value) -> Value {
    if let Some(world_state) = prior_side.get("world_state")
        && !world_state.is_null()
    {
        return world_state.clone();
    }

    let mut sections = BTreeMap::<String, Value>::new();
    if let Some(environment) =
        TurnContextItem::from_side(prior_side.get("reference_context").unwrap_or(&Value::Null))
        && let Ok(value) = serde_json::to_value(environment)
    {
        sections.insert(EnvironmentSection::ID.to_string(), value);
    }
    if let Some(dispatch) =
        DispatchSnapshot::from_legacy(prior_side.get("dispatch_emitted").unwrap_or(&Value::Null))
        && let Ok(value) = serde_json::to_value(dispatch)
    {
        sections.insert(DispatchSection::ID.to_string(), value);
    }
    if sections.is_empty() {
        Value::Null
    } else {
        json!({"v": super::world_state::WORLD_STATE_VERSION, "sections": sections})
    }
}

fn user_message(blocks: Vec<String>) -> Vec<TextMessage> {
    if blocks.is_empty() {
        Vec::new()
    } else {
        vec![TextMessage {
            role: FragmentRole::User,
            text_blocks: blocks,
        }]
    }
}

fn hash_strings(values: &[String]) -> String {
    let mut hasher = Sha256::new();
    for value in values {
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::context::dispatch::{DispatchContext, DispatchContextArg, DispatchScope};

    #[test]
    fn instructions_snapshot_fingerprints_content_and_loaded_paths() {
        let instructions = UserInstructions {
            directory: "/repo".into(),
            text: "rule".into(),
            loaded_paths: vec![PathBuf::from("/repo/AGENTS.md")],
        };
        let first = ProjectInstructionsSnapshot::from_instructions(Some(&instructions));
        let mut changed = instructions.clone();
        changed.text.push('!');
        let second = ProjectInstructionsSnapshot::from_instructions(Some(&changed));
        assert_ne!(first.content_sha256, second.content_sha256);
        assert_eq!(first.loaded_paths, ["/repo/AGENTS.md"]);
        assert!(first.loaded_paths_sha256.is_some());
    }

    #[test]
    fn scope_baseline_survives_a_scope_less_resume_snapshot() {
        let previous = DispatchSnapshot {
            scope: Some("<bbox_scope>old</bbox_scope>".into()),
            pins: None,
        };
        let dispatch = DispatchState::from_arg(
            DispatchContextArg::Provided(Box::new(DispatchContext {
                v: 1,
                scope: None,
                ..Default::default()
            })),
            &Value::Null,
        );
        let section = DispatchSection::new(CompositionStrategy::CodexShaped, &dispatch);
        let current = section.snapshot();
        let persisted = section.persisted_snapshot(PreviousSection::Known(&previous), &current);
        assert_eq!(persisted.scope, previous.scope);
    }

    #[test]
    fn dispatch_legacy_match_requires_every_current_fragment() {
        let dispatch = DispatchState::from_arg(
            DispatchContextArg::Provided(Box::new(DispatchContext {
                v: 1,
                scope: Some(DispatchScope {
                    task: Some("t".into()),
                    ..Default::default()
                }),
                pins: Some("pin".into()),
                ..Default::default()
            })),
            &Value::Null,
        );
        let section = DispatchSection::new(CompositionStrategy::CodexShaped, &dispatch);
        let only_scope = vec![RetainedFragment {
            role: FragmentRole::User,
            text: dispatch.scope_render().unwrap(),
        }];
        assert!(!section.matches_legacy_fragments(&only_scope));
    }
}
