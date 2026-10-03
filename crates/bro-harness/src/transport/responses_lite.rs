//! Responses Lite incremental tool catalog: a pure, transactional diff over
//! rendered JSON tool declarations, mirroring codex's
//! `context::world_state::top_level_tools` section and its
//! `build_responses_request` item identity scheme.
//!
//! Under Responses Lite the wire carries no `tools` parameter: the full
//! catalog enters history once as an `additional_tools` developer item, and
//! later requests append only added or changed declarations, namespace
//! metadata updates, and removal notices. This module computes those history
//! items and the next comparison baseline; it never touches a transport,
//! buffer, or snapshot on its own.
//!
//! Caller contract (the transport owns every effect):
//!
//! 1. Input is the current wire-schema authority's rendered declarations
//!    (`LiteToolCatalog::new`), not the full admitted-but-deferred catalog.
//! 2. `render_diff` returns ordered history items plus the proposed next
//!    baseline. Append the items to the authoritative input buffer first,
//!    then commit `baseline.to_side()` under the side cell
//!    `responses_lite_tools` at the same checkpoint. Never mark a baseline
//!    applied without the authoritative insertion (an unchanged diff commits
//!    nothing new; its returned baseline re-carries the existing anchor).
//! 3. On restore, `CatalogBaseline::from_side` yields `Unknown` for absent,
//!    legacy, malformed, or oversized cells; a `Known` baseline is trusted
//!    only while `is_coupled_to` still finds its anchor item in the retained
//!    history. Compaction (or any history rewrite) drops definition items:
//!    downgrade the baseline to `Unknown` at that boundary so the next
//!    request re-renders the full catalog; `is_coupled_to` also detects the
//!    loss after the fact.
//! 4. The ordinary Responses wire path never consults this state.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Item type tag of the Responses Lite definitions item (codex
/// `ResponseItem::AdditionalTools`, serde snake_case tag).
const ADDITIONAL_TOOLS: &str = "additional_tools";
/// Side-cell version; any other (or missing) version decodes as unknown.
const BASELINE_VERSION: u64 = 1;
/// A baseline larger than this decodes as unknown and rebuilds from a full
/// render rather than trusting a bloated or corrupt cell.
const MAX_BASELINE_ENTRIES: usize = 4096;
const MAX_BASELINE_BYTES: usize = 64 * 1024;
/// Namespacing domain for per-declaration comparison hashes (kept distinct
/// from the per-session item-identity domain).
fn hash_domain() -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        b"bro-harness/responses-lite/definition-hash/v1",
    )
}

/// One parsed declaration: a namespace (members plus its header hash), or a
/// named/type'd builtin.
struct CatalogEntry {
    name: String,
    definition: Value,
    /// `(member name, member declaration)` for namespaces, in wire order.
    members: Option<Vec<(String, Value)>>,
}

/// The exact serialized Responses Lite declarations visible in one request.
pub(crate) struct LiteToolCatalog {
    entries: Vec<CatalogEntry>,
    hashes: BTreeMap<String, String>,
}

impl LiteToolCatalog {
    /// Parse and validate the rendered declarations. Rejects duplicate
    /// identities (bare names, `namespace.member` keys), declarations without
    /// a name or type, malformed namespaces, and nested namespaces inside
    /// namespace members (the serializer never emits those shapes).
    pub(crate) fn new(definitions: &[Value]) -> Result<Self> {
        let mut entries = Vec::with_capacity(definitions.len());
        let mut hashes = BTreeMap::new();
        let insert =
            |hashes: &mut BTreeMap<String, String>, name: String, value: &Value| -> Result<()> {
                let hash = definition_hash(value);
                if hashes.insert(name.clone(), hash).is_some() {
                    bail!("duplicate Responses Lite tool declaration: {name}");
                }
                Ok(())
            };
        for definition in definitions {
            let Some(object) = definition.as_object() else {
                bail!("Responses Lite tool declaration must be an object");
            };
            let name = declaration_name(definition)?;
            let mut entry = CatalogEntry {
                name: name.to_owned(),
                definition: definition.clone(),
                members: None,
            };
            if let Some(tools) = object.get("tools") {
                let Some(members) = tools.as_array() else {
                    bail!("namespace {name} tools must be an array");
                };
                let mut parsed = Vec::with_capacity(members.len());
                for member in members {
                    let Some(member_object) = member.as_object() else {
                        bail!("namespace {name} member must be an object");
                    };
                    if member_object.contains_key("tools") {
                        bail!(
                            "namespace {name} member {} declares a nested namespace; \
                             nested namespaces are unsupported",
                            declaration_name(member)?
                        );
                    }
                    let member_name = declaration_name(member)?;
                    insert(&mut hashes, format!("{name}.{member_name}"), member)?;
                    parsed.push((member_name.to_owned(), member.clone()));
                }
                let mut header = definition.clone();
                header
                    .as_object_mut()
                    .expect("definition is an object")
                    .remove("tools");
                insert(&mut hashes, name.to_string(), &header)?;
                entry.members = Some(parsed);
            } else {
                insert(&mut hashes, name.to_string(), definition)?;
            }
            entries.push(entry);
        }
        Ok(Self { entries, hashes })
    }

    /// Render the history items for this catalog against the previous
    /// baseline. Absent or unknown previous state renders the full catalog;
    /// known state renders added or changed declarations only, namespace
    /// metadata updates without repeating unchanged members, and a removal
    /// notice for definitions that disappeared. The empty catalog is known
    /// state, not missing state. The returned baseline is the proposed next
    /// comparison state; see the module contract for committing it.
    pub(crate) fn render_diff(
        &self,
        previous: PreviousCatalogState<'_>,
        session_id: &str,
    ) -> LiteCatalogTransition {
        let previous_hashes = match previous {
            PreviousCatalogState::Known(baseline) => Some(&baseline.hashes),
            PreviousCatalogState::Absent | PreviousCatalogState::Unknown => None,
        };
        let changed = |name: &str| {
            previous_hashes.and_then(|hashes| hashes.get(name)) != self.hashes.get(name)
        };
        let mut notices = Vec::new();
        let mut tools = Vec::new();
        for entry in &self.entries {
            let Some(members) = &entry.members else {
                if changed(&entry.name) {
                    tools.push(entry.definition.clone());
                }
                continue;
            };
            let changed_members = members
                .iter()
                .filter(|(name, _)| changed(&format!("{}.{}", entry.name, name)))
                .map(|(_, member)| member.clone())
                .collect::<Vec<_>>();
            if !changed_members.is_empty() {
                let mut namespace = entry.definition.clone();
                namespace["tools"] = Value::Array(changed_members);
                tools.push(namespace);
            } else if changed(&entry.name) {
                // Namespace declarations require a member: update metadata as
                // text without repeating unchanged tool definitions.
                let instructions = entry.definition["description"].as_str().unwrap_or_default();
                let text = if instructions.is_empty() {
                    format!(
                        "The {} namespace no longer has additional instructions.",
                        entry.name
                    )
                } else {
                    format!(
                        "Updated instructions for the {} namespace:\n{instructions}",
                        entry.name
                    )
                };
                notices.push(developer_notice(&text));
            }
        }
        let mut items = Vec::new();
        let anchor_id;
        if tools.is_empty() {
            // Nothing newly defined: the previous anchor item (or no anchor
            // for the empty catalog) remains the authoritative insertion.
            anchor_id = match previous {
                PreviousCatalogState::Known(baseline) => baseline.anchor_id.clone(),
                _ => None,
            };
        } else {
            let id = additional_tools_id(session_id, &tools);
            anchor_id = Some(id.clone());
            items.push(json!({
                "type": ADDITIONAL_TOOLS,
                "id": id,
                "role": "developer",
                "tools": tools,
            }));
        }
        items.append(&mut notices);
        if let Some(hashes) = previous_hashes {
            let removed = hashes
                .keys()
                .filter(|key| !self.hashes.contains_key(*key))
                .cloned()
                .collect::<Vec<_>>();
            if !removed.is_empty() {
                let names = removed.join("\n- ");
                items.push(developer_notice(&format!(
                    "The following tools are no longer available. Do not call them:\n- {names}"
                )));
            }
        }
        LiteCatalogTransition {
            items,
            baseline: CatalogBaseline {
                hashes: self.hashes.clone(),
                anchor_id,
            },
        }
    }
}

/// What is known about the previously model-visible catalog.
#[derive(Clone, Copy)]
pub(crate) enum PreviousCatalogState<'a> {
    /// No baseline was ever committed for this session.
    Absent,
    /// The baseline is unusable (legacy, malformed, oversized, uncoupled, or
    /// invalidated by compaction): the next diff re-renders the full catalog.
    Unknown,
    /// A validated baseline whose anchor item is still retained.
    Known(&'a CatalogBaseline),
}

/// One transactional diff: ordered history items to append plus the proposed
/// next baseline. Commit both atomically per the module contract.
pub(crate) struct LiteCatalogTransition {
    pub(crate) items: Vec<Value>,
    pub(crate) baseline: CatalogBaseline,
}

/// The comparison baseline: canonical hashes of every declaration the model
/// has most recently been shown, plus the anchor binding it to the retained
/// authoritative definition item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogBaseline {
    hashes: BTreeMap<String, String>,
    /// Item id of the last `additional_tools` insertion this baseline
    /// describes; `None` while nothing is defined (known-empty catalog).
    anchor_id: Option<String>,
}

impl CatalogBaseline {
    /// The empty known baseline (nothing defined, nothing to couple to).
    pub(crate) fn empty() -> Self {
        Self {
            hashes: BTreeMap::new(),
            anchor_id: None,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Side-cell form: `{"version":1,"hashes":{...},"anchor_id":"at_..."}`.
    pub(crate) fn to_side(&self) -> Value {
        json!({
            "version": BASELINE_VERSION,
            "hashes": self.hashes,
            "anchor_id": self.anchor_id,
        })
    }

    /// Tolerant restore. Absent, legacy (unversioned), malformed, and
    /// oversized cells decode to `Unknown`; the caller then re-renders the
    /// full catalog instead of trusting a stale map.
    pub(crate) fn from_side(value: &Value) -> BaselineRestore {
        let Some(object) = value.as_object() else {
            return BaselineRestore::Unknown;
        };
        if object.get("version").and_then(Value::as_u64) != Some(BASELINE_VERSION) {
            return BaselineRestore::Unknown;
        }
        let Some(hashes_value) = object.get("hashes").and_then(Value::as_object) else {
            return BaselineRestore::Unknown;
        };
        if hashes_value.len() > MAX_BASELINE_ENTRIES
            || serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > MAX_BASELINE_BYTES)
        {
            return BaselineRestore::Unknown;
        }
        let mut hashes = BTreeMap::new();
        for (name, hash) in hashes_value {
            let Some(hash) = hash.as_str() else {
                return BaselineRestore::Unknown;
            };
            hashes.insert(name.clone(), hash.to_owned());
        }
        let anchor_id = match object.get("anchor_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id.clone()),
            Some(_) => return BaselineRestore::Unknown,
        };
        BaselineRestore::Known(Self { hashes, anchor_id })
    }

    /// Provable coupling to retained history: the baseline describes model
    /// state only while the `additional_tools` item it was committed with is
    /// still present. An anchorless (known-empty) baseline couples trivially.
    /// Compaction or any history rewrite that drops definition items breaks
    /// the coupling; the caller must downgrade to `Unknown` there.
    pub(crate) fn is_coupled_to(&self, input: &[Value]) -> bool {
        let Some(anchor) = &self.anchor_id else {
            return true;
        };
        input.iter().any(|item| {
            item["type"].as_str() == Some(ADDITIONAL_TOOLS) && item["id"].as_str() == Some(anchor)
        })
    }
}

pub(crate) enum BaselineRestore {
    Known(CatalogBaseline),
    Unknown,
}

/// codex `definition_name`: namespaces and members are named; built-ins are
/// identified by type.
fn declaration_name(definition: &Value) -> Result<&str> {
    definition["name"]
        .as_str()
        .or_else(|| definition["type"].as_str())
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Responses Lite tool declaration has no name or type"))
}

/// Canonical form: object keys recursively sorted, then compact JSON. With
/// serde_json's default (non-preserve-order) maps this is the ordinary
/// serialization; sorting defensively keeps the hash stable if that ever
/// changes.
fn canonical_json(value: &Value) -> Vec<u8> {
    fn sort(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), sort(value)))
                    .collect::<serde_json::Map<_, _>>(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_vec(&sort(value)).expect("JSON values serialize")
}

/// Stable per-declaration fingerprint (uuid v5 over the canonical payload,
/// codex's identity trick without adding a hash dependency).
fn definition_hash(value: &Value) -> String {
    Uuid::new_v5(&hash_domain(), &canonical_json(value)).to_string()
}

/// codex `build_responses_request`: the definitions item id is uuid v5 of the
/// canonical tools payload under the session-namespaced OID domain, suffixed
/// `at_`. Retries and resumed sessions with the same session and payload
/// reproduce the id; a changed payload intentionally gets a new id.
fn additional_tools_id(session_id: &str, tools: &[Value]) -> String {
    let session_namespace = Uuid::new_v5(&Uuid::NAMESPACE_OID, session_id.as_bytes());
    let payload = canonical_json(&Value::Array(tools.to_vec()));
    format!("at_{}", Uuid::new_v5(&session_namespace, &payload))
}

/// A standalone developer notice item (namespace metadata updates, removals).
fn developer_notice(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "developer",
        "content": [{"type": "input_text", "text": text}],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, extra: &str) -> Value {
        json!({"type":"function", "name":name, "description":format!("Look up {extra}."),
            "parameters":{"type":"object", "properties":{"q":{"type":"string"}}}})
    }

    fn builtin(kind: &str) -> Value {
        json!({"type":kind, "enabled":true})
    }

    fn namespace(name: &str, description: &str, members: Vec<Value>) -> Value {
        json!({"type":"namespace", "name":name, "description":description, "tools":members})
    }

    fn diff<'a>(
        catalog: &LiteToolCatalog,
        previous: PreviousCatalogState<'a>,
    ) -> LiteCatalogTransition {
        catalog.render_diff(previous, "session-1")
    }

    fn notice_text(item: &Value) -> &str {
        item["content"][0]["text"].as_str().expect("notice text")
    }

    #[test]
    fn absent_previous_renders_the_full_catalog_once() {
        let catalog =
            LiteToolCatalog::new(&[tool("read", "files"), builtin("web_search")]).unwrap();
        let transition = diff(&catalog, PreviousCatalogState::Absent);
        assert_eq!(transition.items.len(), 1);
        let item = &transition.items[0];
        assert_eq!(item["type"], "additional_tools");
        assert_eq!(item["role"], "developer");
        assert_eq!(
            item["tools"].as_array().unwrap().len(),
            2,
            "initial render carries every declaration"
        );
        let id = item["id"].as_str().unwrap().to_owned();
        assert!(id.starts_with("at_"), "{id}");
        assert_eq!(transition.baseline.anchor_id.as_deref(), Some(id.as_str()));
        assert_eq!(transition.baseline.hashes.len(), 2);
    }

    #[test]
    fn known_previous_renders_changed_declarations_only() {
        let original =
            LiteToolCatalog::new(&[tool("read", "files"), tool("search", "code")]).unwrap();
        let first = diff(&original, PreviousCatalogState::Absent);
        let baseline = first.baseline.clone();
        let id = first.items[0]["id"].as_str().unwrap().to_owned();

        // Unchanged catalog: nothing new, baseline re-carries the anchor.
        let same = diff(&original, PreviousCatalogState::Known(&baseline));
        assert!(same.items.is_empty());
        assert_eq!(same.baseline, baseline);

        // Only the changed declaration re-renders.
        let updated =
            LiteToolCatalog::new(&[tool("read", "files"), tool("search", "the web")]).unwrap();
        let transition = diff(&updated, PreviousCatalogState::Known(&baseline));
        assert_eq!(transition.items.len(), 1);
        let tools = transition.items[0]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "search");
        let new_id = transition.items[0]["id"].as_str().unwrap().to_owned();
        assert_ne!(new_id, id, "changed payload gets a new identity");
        assert_eq!(
            transition.baseline.anchor_id.as_deref(),
            Some(new_id.as_str())
        );
        assert_ne!(
            transition.baseline.hashes["search"],
            baseline.hashes["search"]
        );
        assert_eq!(transition.baseline.hashes["read"], baseline.hashes["read"]);
    }

    #[test]
    fn reordered_catalog_is_unchanged_state() {
        let original =
            LiteToolCatalog::new(&[tool("read", "files"), tool("search", "code")]).unwrap();
        let baseline = diff(&original, PreviousCatalogState::Absent).baseline;
        let reordered =
            LiteToolCatalog::new(&[tool("search", "code"), tool("read", "files")]).unwrap();
        let transition = diff(&reordered, PreviousCatalogState::Known(&baseline));
        assert!(
            transition.items.is_empty(),
            "per-declaration hashes make ordering irrelevant"
        );
        assert_eq!(transition.baseline.hashes, baseline.hashes);
    }

    #[test]
    fn removal_emits_notice_and_empties_the_baseline() {
        let original =
            LiteToolCatalog::new(&[tool("read", "files"), tool("search", "code")]).unwrap();
        let baseline = diff(&original, PreviousCatalogState::Absent).baseline;
        let smaller = LiteToolCatalog::new(&[tool("read", "files")]).unwrap();
        let transition = diff(&smaller, PreviousCatalogState::Known(&baseline));
        assert_eq!(transition.items.len(), 1);
        assert_eq!(transition.items[0]["role"], "developer");
        assert_eq!(
            notice_text(&transition.items[0]),
            "The following tools are no longer available. Do not call them:\n- search"
        );
        assert_eq!(transition.baseline.hashes.len(), 1);
        assert_eq!(transition.baseline.anchor_id, None);
    }

    #[test]
    fn namespace_member_change_renders_only_that_member() {
        let members = vec![tool("read", "files"), tool("write", "files")];
        let original =
            LiteToolCatalog::new(&[namespace("functions", "Tools.", members.clone())]).unwrap();
        let baseline = diff(&original, PreviousCatalogState::Absent).baseline;
        assert_eq!(baseline.hashes.len(), 3, "header plus two members");

        let changed = vec![tool("read", "files"), tool("write", "files quickly")];
        let updated = LiteToolCatalog::new(&[namespace("functions", "Tools.", changed)]).unwrap();
        let transition = diff(&updated, PreviousCatalogState::Known(&baseline));
        assert_eq!(transition.items.len(), 1);
        let emitted = &transition.items[0]["tools"].as_array().unwrap();
        assert_eq!(emitted.len(), 1, "unchanged member is not repeated");
        assert_eq!(emitted[0]["name"], "write");
    }

    #[test]
    fn namespace_metadata_only_change_is_a_notice_without_members() {
        let members = vec![tool("read", "files")];
        let original =
            LiteToolCatalog::new(&[namespace("functions", "Tools.", members.clone())]).unwrap();
        let baseline = diff(&original, PreviousCatalogState::Absent).baseline;

        let updated =
            LiteToolCatalog::new(&[namespace("functions", "Better tools.", members.clone())])
                .unwrap();
        let transition = diff(&updated, PreviousCatalogState::Known(&baseline));
        assert_eq!(transition.items.len(), 1);
        assert_eq!(transition.items[0]["type"], "message");
        assert_eq!(
            notice_text(&transition.items[0]),
            "Updated instructions for the functions namespace:\nBetter tools."
        );
        assert_ne!(
            transition.baseline.hashes["functions"],
            baseline.hashes["functions"]
        );
        assert_eq!(
            transition.baseline.hashes["functions.read"],
            baseline.hashes["functions.read"]
        );

        let cleared = LiteToolCatalog::new(&[namespace("functions", "", members)]).unwrap();
        let transition = diff(&cleared, PreviousCatalogState::Known(&baseline));
        assert_eq!(
            notice_text(&transition.items[0]),
            "The functions namespace no longer has additional instructions."
        );
    }

    #[test]
    fn empty_catalog_is_known_not_absent() {
        let empty = LiteToolCatalog::new(&[]).unwrap();
        let transition = diff(&empty, PreviousCatalogState::Absent);
        assert!(transition.items.is_empty());
        assert!(transition.baseline.is_empty());
        assert_eq!(transition.baseline.anchor_id, None);

        // Empty after nonempty keeps the removal notice (covered above) and
        // a later refill renders fully against the known-empty baseline.
        let baseline = transition.baseline;
        let refilled = LiteToolCatalog::new(&[tool("read", "files")]).unwrap();
        let transition = diff(&refilled, PreviousCatalogState::Known(&baseline));
        assert_eq!(transition.items[0]["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn duplicate_identities_and_unsupported_shapes_are_rejected() {
        let duplicate = &[tool("read", "files"), tool("read", "code")];
        assert!(LiteToolCatalog::new(duplicate).is_err());

        let member_clash = &[namespace(
            "functions",
            "Tools.",
            vec![tool("read", "files")],
        )];
        let catalog = LiteToolCatalog::new(member_clash).unwrap();
        assert!(
            LiteToolCatalog::new(&[
                namespace("functions", "Other.", vec![tool("other", "x")]),
                tool("functions", "collides with the namespace header")
            ])
            .is_err()
        );
        assert_eq!(catalog.hashes.len(), 2);

        // Nested namespaces inside members are unsupported.
        assert!(
            LiteToolCatalog::new(&[namespace(
                "functions",
                "Tools.",
                vec![namespace("inner", "Nested.", vec![])],
            )])
            .is_err()
        );

        // No name and no type; tools that is not an array; non-object members.
        assert!(LiteToolCatalog::new(&[json!({"description":"anonymous"})]).is_err());
        assert!(LiteToolCatalog::new(&[json!({"name":"ns","tools":{"read":true}})]).is_err());
        assert!(LiteToolCatalog::new(&[json!({"name":"ns","tools":[7]})]).is_err());
        assert!(LiteToolCatalog::new(&[json!(7)]).is_err());
    }

    #[test]
    fn baseline_restores_tolerantly_and_bounds_size() {
        let catalog = LiteToolCatalog::new(&[tool("read", "files")]).unwrap();
        let baseline = diff(&catalog, PreviousCatalogState::Absent).baseline;
        let side = baseline.to_side();
        match CatalogBaseline::from_side(&side) {
            BaselineRestore::Known(restored) => assert_eq!(restored, baseline),
            BaselineRestore::Unknown => panic!("valid baseline must restore"),
        }

        // Legacy bare map (no version), absent, malformed values, and wrong
        // types all decode to unknown; the caller re-renders fully.
        for legacy in [
            Value::Null,
            json!({}),
            json!({"functions.read": "hash"}),
            json!({"version": 0, "hashes": {}}),
            json!({"version": 1, "hashes": {"a": 7}}),
            json!({"version": 1, "hashes": "nope"}),
            json!({"version": 1, "hashes": {}, "anchor_id": 4}),
        ] {
            assert!(
                matches!(
                    CatalogBaseline::from_side(&legacy),
                    BaselineRestore::Unknown
                ),
                "{legacy}"
            );
        }

        // Oversized baselines rebuild instead of trusting a bloated cell.
        let mut oversized = json!({"version": 1, "hashes": {}, "anchor_id": null});
        let hashes = oversized["hashes"].as_object_mut().unwrap();
        for index in 0..=MAX_BASELINE_ENTRIES {
            hashes.insert(format!("tool_{index}"), json!("hash"));
        }
        assert!(matches!(
            CatalogBaseline::from_side(&oversized),
            BaselineRestore::Unknown
        ));
    }

    #[test]
    fn baseline_couples_to_retained_authoritative_history() {
        let catalog = LiteToolCatalog::new(&[tool("read", "files")]).unwrap();
        let transition = diff(&catalog, PreviousCatalogState::Absent);
        let baseline = transition.baseline;
        let item = &transition.items[0];

        // Coupled while the anchor item is retained.
        assert!(baseline.is_coupled_to(&[item.clone()]));
        assert!(baseline.is_coupled_to(&[
            json!({"type":"message","role":"user","content":"task"}),
            item.clone(),
        ]));

        // Compaction dropped the definition items: uncoupled, downgrade.
        let compacted = vec![json!({"type":"message","role":"user","content":"task"})];
        assert!(!baseline.is_coupled_to(&compacted));

        // An orphan snapshot from other history never couples.
        let stranger = json!({"type":"additional_tools", "id":"at_00000000-0000-5000-8000-000000000000", "role":"developer", "tools":[]});
        assert!(!baseline.is_coupled_to(&[stranger]));

        // Anchorless (known-empty) baselines couple trivially.
        assert!(CatalogBaseline::empty().is_coupled_to(&compacted));
        assert!(CatalogBaseline::empty().is_coupled_to(&[]));
    }

    #[test]
    fn item_ids_are_deterministic_per_session_and_payload() {
        let tools = vec![tool("read", "files"), tool("search", "code")];
        let a = LiteToolCatalog::new(&tools).unwrap();
        let b = LiteToolCatalog::new(&tools).unwrap();
        let id_a = diff(&a, PreviousCatalogState::Absent).items[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let id_b = diff(&b, PreviousCatalogState::Absent).items[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(id_a, id_b, "same session and payload reproduce the id");

        let other_session = a
            .render_diff(PreviousCatalogState::Absent, "session-2")
            .items[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(id_a, other_session, "identity binds to the session");

        let changed = LiteToolCatalog::new(&[tool("read", "files")]).unwrap();
        let id_changed = diff(&changed, PreviousCatalogState::Absent).items[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(id_a, id_changed, "identity binds to the payload");
    }
}
