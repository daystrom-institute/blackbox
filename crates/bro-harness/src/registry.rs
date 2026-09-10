//! Tool registry with a three-tier deferral model — our own uniform,
//! client-side equivalent of Claude Code's Tool Search, so behavior is
//! identical across all transports (the Anthropic-native `defer_loading`
//! server feature isn't available on GLM/DeepSeek/OpenAI endpoints).
//!
//! Tiers:
//!
//! - `Pinned`: always in the wire `tools` array (first) AND surfaced
//!   prominently in the system prompt. Elevated by `PinPolicy` (no default
//!   patterns; set `BRO_HARNESS_PIN_TOOLS` to opt in), plus `tool_search`
//!   itself.
//! - `Eager`: always in the wire array; the core built-ins.
//! - `Deferred`: NOT in the wire array; advertised name+desc in a
//!   system-prompt manifest and loaded on demand via `tool_search`. All MCP
//!   tools default here.
//!
//! `tool_search(query)` is a Pinned meta-tool we own: it matches the deferred
//! catalog and inserts hits into a shared `activated` set, so the next turn's
//! wire array includes their full schemas. Client tools are dispatched
//! in-process; the registry holds everything.

use crate::mcp::ToolFilter;
use crate::transport::ToolSpec;
use async_trait::async_trait;
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub const TOOL_SEARCH: &str = "tool_search";
const MAX_ACTIVE_TOOLS: usize = 32;
const MAX_ACTIVE_DEFINITION_BYTES: usize = 64 * 1024;
const DEFAULT_SEARCH_LIMIT: usize = 8;
const MAX_SEARCH_LIMIT: usize = 16;
const MAX_SEARCH_RESULT_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Pinned,
    Eager,
    Deferred,
}

/// Which tools get elevated to `Pinned`. Patterns are exact names or a
/// trailing-`*` prefix glob. Override with `BRO_HARNESS_PIN_TOOLS`
/// (comma-separated).
#[derive(Default)]
pub struct PinPolicy {
    patterns: Vec<String>,
}

impl PinPolicy {
    pub fn from_env() -> Self {
        let patterns = match std::env::var("BRO_HARNESS_PIN_TOOLS") {
            Ok(v) => v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            Err(_) => Vec::new(),
        };
        Self { patterns }
    }

    pub fn matches(&self, name: &str) -> bool {
        self.patterns.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == p,
        })
    }

    /// Add a name (or trailing-`*` glob) to the pin set. Used to pin the fleet
    /// `report` tool only in bidirectional mode.
    pub fn also_pin(&mut self, name: &str) {
        self.patterns.push(name.to_string());
    }
}

struct Entry {
    tool: Arc<dyn Tool>,
    tier: Tier,
    definition_bytes: usize,
}

pub struct Registry {
    execution: Arc<tokio::sync::RwLock<()>>,
    tools: HashMap<String, Entry>,
    activated: Arc<Mutex<HashSet<String>>>,
    /// Prior flat activation promises remain evidence even if restoration is
    /// explicitly cleared or current catalog/policy removes a tool.
    resume_required: std::collections::BTreeSet<String>,
}

impl Registry {
    /// Built-ins default `Eager`, MCP tools default `Deferred`; `pin` elevates
    /// matching names (from either origin) to `Pinned`. A `tool_search`
    /// meta-tool is added as `Pinned` unless explicitly denied.
    ///
    /// `filter` gates **built-ins** by their bare name (`shell_run`, `git_*`,
    /// …) — the same allow/deny plane that filters MCP tools, applied uniformly
    /// so a profile can deny a built-in family. MCP tools arrive already
    /// filtered by their fully-qualified `mcp__server__tool` name in
    /// `load_mcp_tools`, then split by placement; only out-box/both survivors
    /// are passed here. `tool_search` honors an explicit deny but ignores
    /// allow-list exclusion, so a narrow allow-list doesn't strand a bro that
    /// needs to load deferred tools.
    pub fn new(
        builtins: Vec<Arc<dyn Tool>>,
        mcp: Vec<Arc<dyn Tool>>,
        pin: &PinPolicy,
        filter: &ToolFilter,
    ) -> anyhow::Result<Self> {
        Self::with_options(builtins, mcp, pin, filter, false)
    }

    /// Like [`Registry::new`], but `defer_builtins` controls the default tier of
    /// non-pinned built-ins. `false` (the [`new`](Registry::new) default) keeps
    /// them `Eager` (always in the wire array). `true` demotes them to
    /// `Deferred` — out of the wire array but still `tool_search`-loadable —
    /// which is how code-mode `only` hides the flat surface so `exec`/`wait`
    /// (pinned) are the sole authorial entrypoints. Pinned built-ins are
    /// unaffected (they stay visible regardless).
    pub fn with_options(
        builtins: Vec<Arc<dyn Tool>>,
        mcp: Vec<Arc<dyn Tool>>,
        pin: &PinPolicy,
        filter: &ToolFilter,
        defer_builtins: bool,
    ) -> anyhow::Result<Self> {
        let default_builtin_tier = if defer_builtins {
            Tier::Deferred
        } else {
            Tier::Eager
        };
        let mut tools: HashMap<String, Entry> = HashMap::new();
        for t in builtins {
            if !filter.permits(t.name()) {
                continue;
            }
            let tier = if pin.matches(t.name()) {
                Tier::Pinned
            } else if matches!(
                t.name(),
                "smart_read"
                    | "git_status"
                    | "git_log"
                    | "git_diff"
                    | "git_show"
                    | "git_commit"
                    | "sandbox_status"
                    | "sandbox_grounding"
            ) {
                // Compatibility and convenience tools remain callable, but basic
                // file/search/shell operations own the default authoring surface.
                Tier::Deferred
            } else {
                default_builtin_tier
            };
            insert_admitted(&mut tools, t, tier)?;
        }
        for t in mcp {
            let tier = if pin.matches(t.name()) {
                Tier::Pinned
            } else {
                Tier::Deferred
            };
            insert_admitted(&mut tools, t, tier)?;
        }

        let admitted = bro_tools::prune_tool_dependencies(
            tools.values().map(|entry| entry.tool.clone()).collect(),
        );
        let admitted_names: HashSet<_> = admitted.iter().map(|tool| tool.name()).collect();
        tools.retain(|name, _| admitted_names.contains(name.as_str()));

        let activated = Arc::new(Mutex::new(HashSet::new()));

        // tool_search is added unless explicitly denied. It ignores allow-list
        // exclusion (decision b): a narrow allow-list still gets search so it
        // can load the deferred tools it allowed.
        if !filter.denied(TOOL_SEARCH) {
            // Snapshot the deferred catalog for the search tool.
            let catalog: Arc<Vec<DeferredEntry>> = Arc::new(
                tools
                    .values()
                    .filter(|e| e.tier == Tier::Deferred)
                    .map(|e| DeferredEntry {
                        name: e.tool.name().to_string(),
                        description: e.tool.description().to_string(),
                        schema: e.tool.input_schema(),
                        definition_bytes: e.definition_bytes,
                    })
                    .collect(),
            );
            let search: Arc<dyn Tool> = Arc::new(ToolSearchTool {
                catalog,
                activated: activated.clone(),
            });
            tools.insert(
                TOOL_SEARCH.to_string(),
                Entry {
                    definition_bytes: tool_definition_bytes(search.as_ref()),
                    tool: search,
                    tier: Tier::Pinned,
                },
            );
        }

        Ok(Self {
            tools,
            activated,
            resume_required: Default::default(),
            execution: Arc::new(tokio::sync::RwLock::new(())),
        })
    }

    /// Session-owned activation names, never persisted schemas or permissions.
    /// Prior activations remain evidence when their tools become pinned/eager;
    /// otherwise a later resume could forget the earlier schema promise.
    pub fn activation_state(&self) -> Value {
        let mut names: std::collections::BTreeSet<_> =
            self.activated.lock().unwrap().iter().cloned().collect();
        names.extend(
            self.resume_required
                .iter()
                .filter(|name| {
                    self.tools
                        .get(*name)
                        .is_some_and(|entry| entry.tier != Tier::Deferred)
                })
                .cloned(),
        );
        json!(names)
    }

    /// Restore visibility only for tools surviving the current catalog,
    /// placement and policy filters. A prior activation cannot grant access.
    pub fn restore_activations(&self, state: &Value) {
        let mut activated = self.activated.lock().unwrap();
        activated.clear();
        for name in state
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if self
                .tools
                .get(name)
                .is_some_and(|entry| entry.tier == Tier::Deferred)
            {
                activated.insert(name.to_string());
            }
        }
    }

    /// Saved state controls restoration; successful receipts independently
    /// constrain the schema promised to a resumed conversation. Only legacy
    /// snapshots with no activation field recover activations from receipts.
    pub fn restore_resume_activations(&mut self, saved: Option<&Value>, receipts: &Value) {
        self.restore_activations(saved.unwrap_or(receipts));
        self.resume_required = saved
            .into_iter()
            .chain(std::iter::once(receipts))
            .flat_map(|value| value.as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }

    /// Prior flat activation requires a current flat schema. Tools hidden for
    /// code-mode that were never flat-activated create no such requirement.
    pub fn validate_resume_tool_schemas(&self) -> anyhow::Result<()> {
        let activated = self.activated.lock().unwrap();
        let bytes = activated
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|entry| entry.definition_bytes)
            .sum::<usize>();
        if activated.len() > MAX_ACTIVE_TOOLS || bytes > MAX_ACTIVE_DEFINITION_BYTES {
            anyhow::bail!(
                "error.resume_tool_activation_limit: restored visibility exceeds the session catalog budget ({} tools, {bytes} definition bytes). Historical schemas cannot be silently removed; start a fresh session with focused discovery.",
                activated.len()
            );
        }
        let missing: Vec<_> =
            self.resume_required
                .iter()
                .filter(|name| {
                    !self.tools.get(*name).is_some_and(|entry| {
                        entry.tier != Tier::Deferred || activated.contains(*name)
                    })
                })
                .cloned()
                .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "error.resume_tool_schema_missing: previously activated client tools lack current wire schemas: {}. Restore the intended permitted tool catalog and activation state, or start a fresh session using current policy.",
                missing.join(", ")
            );
        }
        Ok(())
    }

    fn spec_of(e: &Entry) -> ToolSpec {
        ToolSpec {
            name: e.tool.name().to_string(),
            description: e.tool.description().to_string(),
            schema: e.tool.input_schema(),
            grammar: e.tool.freeform_grammar(),
        }
    }

    /// The wire `tools` array for a turn: Pinned (incl. tool_search) first,
    /// then Eager, then any Deferred tools activated so far. Sorted within
    /// each group for stable prompt-cache prefixes.
    pub fn wire_specs(&self) -> Vec<ToolSpec> {
        let activated = self.activated.lock().unwrap();
        let mut pinned = Vec::new();
        let mut eager = Vec::new();
        let mut active = Vec::new();
        for e in self.tools.values() {
            match e.tier {
                Tier::Pinned => pinned.push(Self::spec_of(e)),
                Tier::Eager => eager.push(Self::spec_of(e)),
                Tier::Deferred => {
                    if activated.contains(e.tool.name()) {
                        active.push(Self::spec_of(e));
                    }
                }
            }
        }
        for v in [&mut pinned, &mut eager, &mut active] {
            v.sort_by(|a, b| a.name.cmp(&b.name));
        }
        pinned.extend(eager);
        pinned.extend(active);
        pinned
    }

    /// Pinned tool (name, description) pairs for the prominent system-prompt
    /// callout.
    pub fn pinned(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self
            .tools
            .values()
            .filter(|e| e.tier == Tier::Pinned)
            .map(|e| (e.tool.name().to_string(), short_desc(e.tool.description())))
            .collect();
        v.sort();
        v
    }

    /// Deferred-and-not-yet-activated (name, description) pairs for the
    /// names-only manifest.
    pub fn manifest(&self) -> Vec<(String, String)> {
        let activated = self.activated.lock().unwrap();
        let mut v: Vec<_> = self
            .tools
            .values()
            .filter(|e| e.tier == Tier::Deferred && !activated.contains(e.tool.name()))
            .map(|e| (e.tool.name().to_string(), short_desc(e.tool.description())))
            .collect();
        v.sort();
        v
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn schemas(&self) -> Vec<(String, Value)> {
        let mut schemas: Vec<_> = self
            .tools
            .iter()
            .map(|(name, entry)| (name.clone(), entry.tool.input_schema()))
            .collect();
        schemas.sort_by(|a, b| a.0.cmp(&b.0));
        schemas
    }

    /// Set the session admission gate shared with nested code-mode calls.
    pub fn set_dispatch_gate(&mut self, execution: Arc<tokio::sync::RwLock<()>>) {
        self.execution = execution;
    }

    pub async fn dispatch(&self, name: &str, input: Value, cx: &ToolCx) -> ToolResult {
        self.start_dispatch(name, input, cx).wait().await
    }

    /// Retain completion ownership across interruption or dropped wait futures.
    pub fn start_dispatch(
        &self,
        name: &str,
        input: Value,
        cx: &ToolCx,
    ) -> bro_tools::InvocationHandle {
        let Some(entry) = self.tools.get(name) else {
            return bro_tools::InvocationHandle::completed(ToolResult::Error(format!(
                "unknown tool: {name}"
            )));
        };
        // Cell controls must remain callable while nested work owns the gate:
        // exec and wait may themselves await a nested call or cancel it.
        let execution = if matches!(
            name,
            bro_code_mode::PUBLIC_TOOL_NAME | bro_code_mode::WAIT_TOOL_NAME
        ) {
            None
        } else {
            Some(self.execution.clone())
        };
        bro_tools::start_tool_invocation(entry.tool.clone(), input, cx.clone(), execution)
    }

    /// Whether `name` is safe to dispatch concurrently with other tools — i.e.
    /// the tool declares itself `read_only` and so records no edits and touches
    /// no shared mutable workspace state. Unknown tools and tools that leave the
    /// default annotation (`read_only: false`) are treated as **mutating** and
    /// must be serialized. This is the conservative default for MCP tools, whose
    /// effects the harness cannot inspect, and for any builtin that writes.
    pub fn read_only(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|e| e.tool.annotations().read_only)
            .unwrap_or(false)
    }
}

fn tool_definition_bytes(tool: &dyn Tool) -> usize {
    let grammar = tool
        .freeform_grammar()
        .map(|grammar| json!({"syntax":grammar.syntax,"definition":grammar.definition}));
    json!({"name":tool.name(),"description":tool.description(),"input_schema":tool.input_schema(),"grammar":grammar}).to_string().len()
}

fn insert_admitted(
    tools: &mut HashMap<String, Entry>,
    tool: Arc<dyn Tool>,
    tier: Tier,
) -> anyhow::Result<()> {
    let name = tool.name();
    anyhow::ensure!(
        !name.trim().is_empty() && name != TOOL_SEARCH,
        "invalid or reserved tool name: {name}"
    );
    if let Some(existing) = tools.get_mut(name) {
        anyhow::ensure!(
            Arc::ptr_eq(&existing.tool, &tool),
            "duplicate canonical tool name: {name}"
        );
        if tier == Tier::Pinned || (existing.tier == Tier::Deferred && tier == Tier::Eager) {
            existing.tier = tier;
        }
        return Ok(());
    }
    let definition_bytes = tool_definition_bytes(tool.as_ref());
    tools.insert(
        name.to_owned(),
        Entry {
            tool,
            tier,
            definition_bytes,
        },
    );
    Ok(())
}

fn short_desc(d: &str) -> String {
    let line = d.lines().next().unwrap_or("").trim();
    if line.len() > 100 {
        format!(
            "{}…",
            &line[..line
                .char_indices()
                .take(100)
                .last()
                .map(|(i, _)| i)
                .unwrap_or(line.len())]
        )
    } else {
        line.to_string()
    }
}

// ---------------------------------------------------------------------------
// tool_search meta-tool
// ---------------------------------------------------------------------------

struct DeferredEntry {
    name: String,
    description: String,
    schema: Value,
    definition_bytes: usize,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolSearchInput {
    query: String,
    #[serde(default)]
    include_schemas: bool,
    #[serde(default = "default_search_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default = "activate_by_default")]
    activate: bool,
}

fn default_search_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}
fn activate_by_default() -> bool {
    true
}

struct ToolSearchTool {
    catalog: Arc<Vec<DeferredEntry>>,
    activated: Arc<Mutex<HashSet<String>>>,
}

fn search_score(entry: &DeferredEntry, query: &str, terms: &[&str]) -> usize {
    let name = entry.name.to_lowercase();
    let description = entry.description.to_lowercase();
    let words: HashSet<_> = name
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let exact = usize::from(name == query) * 100_000;
    exact
        + terms
            .iter()
            .map(|term| {
                if words.contains(term) {
                    1_000
                } else if name.starts_with(term) {
                    500
                } else if name.contains(term) {
                    100
                } else {
                    usize::from(description.contains(term))
                }
            })
            .sum::<usize>()
}

fn compact_search_description(description: &str) -> String {
    let line = description.lines().next().unwrap_or("").trim();
    let end = line
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= 240)
        .last()
        .unwrap_or(0);
    if line.len() <= 240 {
        line.to_owned()
    } else {
        format!("{}...", &line[..end])
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH
    }
    fn description(&self) -> &str {
        "Search the admitted deferred catalog and optionally activate matches. Use keywords or select:name1,name2; limit defaults to 8 (maximum 16), offset pages through stable ranked matches. Results use compact descriptions; include_schemas opts into schemas that fit this result's byte budget. activate=false inspects without adding wire schemas. Activated definitions remain available for this session: at most 32 deferred tools and 64 KiB of definitions. There is no in-session deactivation or automatic eviction of promised schemas; at capacity inspect without activation or start a fresh session."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type":"object", "properties": {
                "query":{"type":"string","description":"Keywords, or select:nameA,nameB. Exact selection permits at most 64 names; query is at most 4096 bytes."},
                "include_schemas":{"type":"boolean","description":"Include input schemas when they fit the result budget; omitted schemas are explicitly marked."},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_SEARCH_LIMIT,"default":DEFAULT_SEARCH_LIMIT},
                "offset":{"type":"integer","minimum":0,"default":0},
                "activate":{"type":"boolean","default":true,"description":"False inspects matches without making new tools visible in the wire catalog."}
            }, "required":["query"], "additionalProperties":false
        })
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ToolSearchInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(error) => return ToolResult::Error(format!("bad tool_search input: {error}")),
        };
        let query = args.query.trim();
        if query.is_empty() || query.len() > 4096 || !(1..=MAX_SEARCH_LIMIT).contains(&args.limit) {
            return ToolResult::Error("tool_search requires a nonempty query of at most 4096 bytes and limit from 1 to 16".into());
        }
        let matches: Vec<&DeferredEntry> = if let Some(selection) = query.strip_prefix("select:") {
            let names: Vec<_> = selection
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .collect();
            if names.is_empty() || names.len() > 64 {
                return ToolResult::Error(
                    "select requires 1 to 64 exact tool names; use limit and offset to page".into(),
                );
            }
            let mut seen = HashSet::new();
            names
                .into_iter()
                .filter(|name| seen.insert(*name))
                .filter_map(|name| self.catalog.iter().find(|entry| entry.name == name))
                .collect()
        } else {
            let query = query.to_lowercase();
            let terms: Vec<_> = query.split_whitespace().collect();
            if terms.len() > 16 {
                return ToolResult::Error("tool_search permits at most 16 keyword terms".into());
            }
            let mut scored: Vec<_> = self
                .catalog
                .iter()
                .filter_map(|entry| {
                    let score = search_score(entry, &query, &terms);
                    (score > 0).then_some((score, entry))
                })
                .collect();
            scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.name.cmp(&right.1.name)));
            scored.into_iter().map(|(_, entry)| entry).collect()
        };
        let total_matches = matches.len();
        let mut page: Vec<_> = matches
            .into_iter()
            .skip(args.offset)
            .take(args.limit)
            .collect();
        let mut activated = self.activated.lock().unwrap();
        let budget = if cx.output_budget == 0 {
            MAX_SEARCH_RESULT_BYTES
        } else {
            cx.output_budget.min(MAX_SEARCH_RESULT_BYTES)
        };
        let mut entries: Vec<Value> = page.iter().map(|entry| {
            let description = compact_search_description(&entry.description);
            let mut value = json!({"name":entry.name,"description":description,"active":activated.contains(&entry.name) || args.activate});
            if description != entry.description { value["description_truncated"] = json!(true); }
            if args.include_schemas { value["input_schema"] = entry.schema.clone(); }
            value
        }).collect();
        let make_result = |page: &[&DeferredEntry], entries: &[Value]| {
            let next_offset = args.offset.saturating_add(page.len());
            json!({
                "loaded":if args.activate { page.iter().map(|entry| entry.name.clone()).collect::<Vec<_>>() } else { Vec::new() },
                "tools":entries, "total_matches":total_matches, "offset":args.offset,
                "next_offset":if next_offset < total_matches { Some(next_offset) } else { None },
                "remaining":{"count":self.catalog.iter().filter(|entry| !activated.contains(&entry.name) && !(args.activate && page.iter().any(|selected| selected.name == entry.name))).count()},
                "activation":{"requested":args.activate,"max_tools":MAX_ACTIVE_TOOLS,"max_definition_bytes":MAX_ACTIVE_DEFINITION_BYTES,"retention":"session; no deactivation"},
                "note":if args.activate { "Loaded schemas are callable on subsequent turns. Continue this query with next_offset to reach other matches." } else { "Inspection only. Unactivated matches are not added to the wire catalog." }
            })
        };
        // Omit optional schemas before reducing the page. No tool is activated
        // until the complete honest receipt fits the producer byte budget.
        if make_result(&page, &entries).to_string().len() > budget {
            for (index, entry) in entries.iter_mut().enumerate() {
                if entry
                    .as_object_mut()
                    .unwrap()
                    .remove("input_schema")
                    .is_some()
                {
                    entry["schema_omitted"] = json!(true);
                    entry["input_schema_bytes"] = json!(page[index].schema.to_string().len());
                }
            }
        }
        while make_result(&page, &entries).to_string().len() > budget && !page.is_empty() {
            page.pop();
            entries.pop();
        }
        if page.is_empty() && total_matches > args.offset
            || make_result(&page, &entries).to_string().len() > budget
        {
            return ToolResult::Error(
                "tool_search result budget is too small for one match; nothing activated".into(),
            );
        }
        let result = make_result(&page, &entries);
        if args.activate {
            let mut projected = activated.clone();
            projected.extend(page.iter().map(|entry| entry.name.clone()));
            let bytes = self
                .catalog
                .iter()
                .filter(|entry| projected.contains(&entry.name))
                .map(|entry| entry.definition_bytes)
                .sum::<usize>();
            if projected.len() > MAX_ACTIVE_TOOLS || bytes > MAX_ACTIVE_DEFINITION_BYTES {
                return ToolResult::Error(format!(
                    "error.tool_activation_limit: this page would retain {} tools and {bytes} definition bytes; limit is {MAX_ACTIVE_TOOLS} tools and {MAX_ACTIVE_DEFINITION_BYTES} bytes. Nothing activated. Use activate=false to inspect, or start a fresh session with focused discovery. Existing schemas cannot be deactivated in this conversation.",
                    projected.len()
                ));
            }
            *activated = projected;
        }
        ToolResult::Json(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_catalog(count: usize) -> Vec<Arc<dyn Tool>> {
        (0..count)
            .map(|index| mk(format!("fixture_{index:02}"), "Fixture search utility"))
            .collect()
    }

    async fn search(registry: &Registry, input: Value) -> Value {
        match registry
            .dispatch(TOOL_SEARCH, input, &test_cx(Default::default()))
            .await
        {
            ToolResult::Json(value) => value,
            other => panic!("expected discovery JSON: {other:?}"),
        }
    }

    #[test]
    fn registry_rejects_distinct_canonical_duplicates_and_reserved_names() {
        assert!(
            Registry::new(
                vec![mk("duplicate", "one")],
                vec![mk("duplicate", "two")],
                &PinPolicy::default(),
                &ToolFilter::default()
            )
            .is_err()
        );
        assert!(
            Registry::new(
                vec![mk(TOOL_SEARCH, "external")],
                vec![],
                &PinPolicy::default(),
                &ToolFilter::default()
            )
            .is_err()
        );
        let shared = mk("shared", "same implementation");
        let registry = Registry::new(
            vec![shared.clone()],
            vec![shared],
            &PinPolicy::default(),
            &ToolFilter::default(),
        )
        .unwrap();
        assert_eq!(
            registry
                .wire_specs()
                .iter()
                .filter(|spec| spec.name == "shared")
                .count(),
            1
        );
        assert!(
            registry.manifest().is_empty(),
            "duplicate deferred placement cannot hide an eager tool"
        );
    }

    #[tokio::test]
    async fn broad_discovery_pages_stably_beyond_eight_without_activation() {
        let registry = Registry::new(
            vec![],
            fixture_catalog(21),
            &PinPolicy::default(),
            &ToolFilter::default(),
        )
        .unwrap();
        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            let result = search(
                &registry,
                json!({"query":"fixture","offset":offset,"activate":false}),
            )
            .await;
            assert_eq!(result["total_matches"], 21);
            assert_eq!(result["loaded"], json!([]));
            names.extend(
                result["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap().to_owned()),
            );
            let Some(next) = result["next_offset"].as_u64() else {
                break;
            };
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(
            names,
            (0..21)
                .map(|index| format!("fixture_{index:02}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(registry.activation_state(), json!([]));
        assert_eq!(registry.wire_specs().len(), 1);
    }

    #[tokio::test]
    async fn discovery_ranks_names_and_bounds_exact_selection() {
        let registry = Registry::new(
            vec![],
            vec![
                mk("aaa", "fixture keyword"),
                mk("fixture", "exact name"),
                mk("fixture_tail", "name token"),
            ],
            &PinPolicy::default(),
            &ToolFilter::default(),
        )
        .unwrap();
        let result = search(&registry, json!({"query":"fixture","activate":false})).await;
        assert_eq!(result["tools"][0]["name"], "fixture");
        assert_eq!(result["tools"][1]["name"], "fixture_tail");
        let result = search(
            &registry,
            json!({"query":"select:aaa,fixture_tail,fixture","limit":1,"offset":1}),
        )
        .await;
        assert_eq!(result["loaded"], json!(["fixture_tail"]));
        assert_eq!(result["next_offset"], 2);
        for input in [
            json!({"query":"fixture","limit":17}),
            json!({"query":"fixture","limit":0}),
            json!({"query":format!("select:{}", vec!["aaa";65].join(","))}),
        ] {
            assert!(
                registry
                    .dispatch(TOOL_SEARCH, input, &test_cx(Default::default()))
                    .await
                    .is_error()
            );
        }
    }

    #[tokio::test]
    async fn activation_cap_is_atomic_and_never_evicts_history() {
        let registry = Registry::new(
            vec![],
            fixture_catalog(40),
            &PinPolicy::default(),
            &ToolFilter::default(),
        )
        .unwrap();
        for offset in [0, 8, 16, 24] {
            search(&registry, json!({"query":"fixture","offset":offset})).await;
        }
        let before = registry.activation_state();
        assert_eq!(before.as_array().unwrap().len(), MAX_ACTIVE_TOOLS);
        let result = registry
            .dispatch(
                TOOL_SEARCH,
                json!({"query":"fixture","offset":32}),
                &test_cx(Default::default()),
            )
            .await;
        assert!(result.is_error());
        assert!(
            result
                .into_content()
                .0
                .contains("error.tool_activation_limit")
        );
        assert_eq!(registry.activation_state(), before);
        let inspected = search(
            &registry,
            json!({"query":"fixture","offset":32,"activate":false}),
        )
        .await;
        assert_eq!(inspected["tools"][0]["name"], "fixture_32");
        assert_eq!(inspected["tools"][0]["active"], false);
        assert_eq!(registry.activation_state(), before);
        registry.restore_activations(&json!(
            (0..33)
                .map(|index| format!("fixture_{index:02}"))
                .collect::<Vec<_>>()
        ));
        assert!(registry.validate_resume_tool_schemas().is_err());
        assert_eq!(
            registry.activation_state().as_array().unwrap().len(),
            33,
            "validation must refuse, not erase history"
        );
    }

    #[tokio::test]
    async fn definition_byte_cap_and_result_budget_are_independent() {
        let huge = mk("oversized", "x".repeat(MAX_ACTIVE_DEFINITION_BYTES));
        let registry = Registry::new(
            vec![],
            vec![huge],
            &PinPolicy::default(),
            &ToolFilter::default(),
        )
        .unwrap();
        let refused = registry
            .dispatch(
                TOOL_SEARCH,
                json!({"query":"select:oversized"}),
                &test_cx(Default::default()),
            )
            .await;
        assert!(refused.is_error());
        assert_eq!(registry.activation_state(), json!([]));
        let inspected = search(
            &registry,
            json!({"query":"select:oversized","activate":false}),
        )
        .await;
        assert!(inspected["tools"][0]["description"].as_str().unwrap().len() <= 243);
        assert_eq!(inspected["tools"][0]["description_truncated"], true);

        let schema = json!({"type":"object","properties":{"large":{"enum":["x".repeat(12_000)]}}});
        let registry = Registry::new(
            vec![],
            vec![mk_schema("large_schema", "large schema fixture", schema)],
            &PinPolicy::default(),
            &ToolFilter::default(),
        )
        .unwrap();
        let mut cx = test_cx(Default::default());
        cx.output_budget = 1024;
        let result = registry
            .dispatch(
                TOOL_SEARCH,
                json!({"query":"select:large_schema","include_schemas":true}),
                &cx,
            )
            .await;
        let ToolResult::Json(result) = result else {
            panic!("expected bounded receipt: {result:?}");
        };
        assert!(result.to_string().len() <= 1024);
        assert_eq!(result["tools"][0]["schema_omitted"], true);
        assert!(result["tools"][0].get("input_schema").is_none());
        assert_eq!(registry.activation_state(), json!(["large_schema"]));
        assert!(registry.wire_specs().iter().any(|spec| {
            spec.name == "large_schema"
                && spec.schema["properties"]["large"]["enum"][0]
                    .as_str()
                    .unwrap()
                    .len()
                    == 12_000
        }));
    }

    #[test]
    fn pin_policy_prefix_and_exact() {
        let p = PinPolicy {
            patterns: vec!["bbox_hybrid_*".into(), "exact_tool".into()],
        };
        assert!(p.matches("bbox_hybrid_search"));
        assert!(p.matches("bbox_hybrid_search_v2"));
        assert!(p.matches("exact_tool"));
        assert!(!p.matches("bbox_stats"));
        assert!(!p.matches("exact_tool_x"));
    }

    fn mk(name: impl Into<String>, desc: impl Into<String>) -> Arc<dyn Tool> {
        mk_schema(
            name,
            desc,
            json!({"type": "object", "properties": {"session_id":{"type":"string"}}}),
        )
    }

    fn mk_schema(name: impl Into<String>, desc: impl Into<String>, schema: Value) -> Arc<dyn Tool> {
        struct T(String, String, Value);
        #[async_trait]
        impl Tool for T {
            fn name(&self) -> &str {
                &self.0
            }
            fn description(&self) -> &str {
                &self.1
            }
            fn input_schema(&self) -> Value {
                self.2.clone()
            }
            async fn call(&self, _i: Value, _c: &ToolCx) -> ToolResult {
                ToolResult::Text("ok".into())
            }
        }
        Arc::new(T(name.into(), desc.into(), schema))
    }

    fn test_cx(defaults: bro_tools::ToolArgDefaults) -> ToolCx {
        ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(std::sync::Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(std::sync::Mutex::new(bro_tools::EditSink::default())),
            cancellation: Default::default(),
            output_budget: 16 * 1024,
            child_env: Arc::new(Default::default()),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(defaults),
            shell_env: Arc::new(Default::default()),
        }
    }

    #[tokio::test]
    async fn dispatch_applies_defaults_and_adds_json_rider() {
        struct Echo;
        #[async_trait]
        impl Tool for Echo {
            fn name(&self) -> &str {
                "mcp__blackbox__bbox_note"
            }
            fn description(&self) -> &str {
                "echo"
            }
            fn input_schema(&self) -> Value {
                json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string"},
                        "session_id": {"type": "string"}
                    }
                })
            }
            async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
                ToolResult::Json(json!({"input": input}))
            }
        }

        let defaults = bro_tools::ToolArgDefaults::parse_map(std::collections::BTreeMap::from([(
            "default:mcp.bbox_note.session_id".to_string(),
            "host-session".to_string(),
        )]))
        .unwrap();
        let reg = Registry::new(
            vec![],
            vec![Arc::new(Echo) as Arc<dyn Tool>],
            &PinPolicy { patterns: vec![] },
            &ToolFilter::default(),
        )
        .unwrap();
        let cx = test_cx(defaults);
        let result = reg
            .dispatch("mcp__blackbox__bbox_note", json!({"kind": "done"}), &cx)
            .await;
        let ToolResult::Json(v) = result else {
            panic!("expected json result");
        };
        assert_eq!(v["input"]["session_id"], "host-session");
        assert!(v.get("defaults_applied").is_none());
        assert_eq!(
            cx.tool_observations.drain().observations[0].context["defaults_applied"]["session_id"],
            "host-session"
        );
    }

    #[tokio::test]
    async fn dispatch_returns_pin_conflict_error_with_separate_observation() {
        let defaults = bro_tools::ToolArgDefaults::parse_map(std::collections::BTreeMap::from([(
            "pin:mcp.bbox_note.session_id".to_string(),
            "host-session".to_string(),
        )]))
        .unwrap();
        let reg = Registry::new(
            vec![],
            vec![mk("mcp__blackbox__bbox_note", "note")],
            &PinPolicy { patterns: vec![] },
            &ToolFilter::default(),
        )
        .unwrap();
        let result = reg
            .dispatch(
                "mcp__blackbox__bbox_note",
                json!({"session_id": "model-session"}),
                &test_cx(defaults),
            )
            .await;
        let ToolResult::Error(text) = result else {
            panic!("expected pin conflict error");
        };
        assert!(text.contains("pin conflict"));
        assert!(text.contains("host-session"));
        assert!(text.contains("model-session"));
    }

    #[test]
    fn tiers_and_activation() {
        let pin = PinPolicy {
            patterns: vec!["bbox_hybrid_*".into()],
        };
        let builtins = vec![mk("file_read", "read a file")];
        let mcp = vec![
            mk("bbox_hybrid_search", "hybrid corpus search"),
            mk("bbox_stats", "corpus stats"),
        ];
        let reg = Registry::new(builtins, mcp, &pin, &ToolFilter::default()).unwrap();

        // Pinned = hybrid search tool + tool_search; Eager = file_read; both in wire.
        let wire: Vec<String> = reg.wire_specs().into_iter().map(|s| s.name).collect();
        assert!(wire.contains(&"bbox_hybrid_search".to_string()));
        assert!(wire.contains(&TOOL_SEARCH.to_string()));
        assert!(wire.contains(&"file_read".to_string()));
        // bbox_stats is deferred → not in wire yet, but in the manifest.
        assert!(!wire.contains(&"bbox_stats".to_string()));
        assert!(reg.manifest().iter().any(|(n, _)| n == "bbox_stats"));

        // Activate it → now in wire, gone from manifest.
        reg.activated
            .lock()
            .unwrap()
            .insert("bbox_stats".to_string());
        let wire: Vec<String> = reg.wire_specs().into_iter().map(|s| s.name).collect();
        assert!(wire.contains(&"bbox_stats".to_string()));
        assert!(!reg.manifest().iter().any(|(n, _)| n == "bbox_stats"));
    }

    #[tokio::test]
    async fn denied_dependency_removes_wrapper_from_registry_and_discovery() {
        let builtins: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::bindings::build_gate::BuildGate),
            Arc::new(bro_tools::ShellRun),
        ];
        let registry = Registry::new(
            builtins,
            vec![],
            &PinPolicy::default(),
            &ToolFilter::from_csv(Some("shell_run"), None),
        )
        .unwrap();
        assert!(!registry.contains("build.gate"));
        assert!(
            !registry
                .wire_specs()
                .iter()
                .any(|spec| spec.name == "build.gate")
        );
        assert!(
            !registry
                .manifest()
                .iter()
                .any(|(name, _)| name == "build.gate")
        );
        let result = registry
            .dispatch(
                "build.gate",
                json!({"command":"exit 97"}),
                &test_cx(bro_tools::ToolArgDefaults::default()),
            )
            .await;
        assert!(result.is_error());
        assert!(result.into_content().0.contains("unknown tool"));
    }

    #[tokio::test]
    async fn activations_roundtrip_with_current_catalog_and_policy() {
        let pin = PinPolicy { patterns: vec![] };
        let reg = Registry::with_options(
            vec![mk("file_read", "old read"), mk("file_write", "old write")],
            vec![mk("mcp__fixture__removed", "removed")],
            &pin,
            &ToolFilter::default(),
            true,
        )
        .unwrap();
        reg.dispatch(
            TOOL_SEARCH,
            json!({"query":"select:file_read,file_write,mcp__fixture__removed"}),
            &test_cx(bro_tools::ToolArgDefaults::default()),
        )
        .await;
        let saved = reg.activation_state();
        assert_eq!(
            saved,
            json!(["file_read", "file_write", "mcp__fixture__removed"])
        );
        let filter = ToolFilter::from_csv(Some("file_write"), None);
        let resumed = Registry::with_options(
            vec![
                mk("file_read", "current read"),
                mk("file_write", "current write"),
            ],
            vec![mk("mcp__fixture__new", "new tool")],
            &pin,
            &filter,
            true,
        )
        .unwrap();
        resumed.restore_activations(&saved);
        assert_eq!(resumed.activation_state(), json!(["file_read"]));
        let wire = resumed.wire_specs();
        assert_eq!(
            wire.iter()
                .find(|s| s.name == "file_read")
                .unwrap()
                .description,
            "current read"
        );
        assert!(!wire.iter().any(|s| s.name == "file_write"
            || s.name == "mcp__fixture__removed"
            || s.name == "mcp__fixture__new"));
        assert_eq!(
            resumed.manifest(),
            vec![("mcp__fixture__new".into(), "new tool".into())]
        );
        resumed.restore_activations(&json!([null, 7, "unknown"]));
        assert_eq!(resumed.activation_state(), json!([]));
    }

    #[test]
    fn prior_activation_survives_pinned_or_eager_resume_then_catalog_loss() {
        for pinned in [true, false] {
            let saved = json!(["file_read"]);
            let pin = PinPolicy {
                patterns: if pinned {
                    vec!["file_read".into()]
                } else {
                    vec![]
                },
            };
            let mut first_resume = Registry::with_options(
                vec![mk("file_read", "current read")],
                vec![],
                &pin,
                &ToolFilter::default(),
                pinned,
            )
            .unwrap();
            // Only the saved activation survives; neither snapshot nor event
            // history still contains the original search receipt.
            first_resume.restore_resume_activations(Some(&saved), &json!([]));
            first_resume.validate_resume_tool_schemas().unwrap();
            assert!(first_resume.activated.lock().unwrap().is_empty());
            assert_eq!(first_resume.activation_state(), saved);

            let mut second_resume = Registry::new(
                vec![mk("file_write", "unactivated tool")],
                vec![],
                &PinPolicy::default(),
                &ToolFilter::default(),
            )
            .unwrap();
            second_resume
                .restore_resume_activations(Some(&first_resume.activation_state()), &json!([]));
            let error = second_resume.validate_resume_tool_schemas().unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("error.resume_tool_schema_missing")
            );
            assert!(error.to_string().contains("file_read"));
            assert_eq!(second_resume.activation_state(), json!([]));
        }
    }

    #[test]
    fn defer_builtins_hides_flat_surface_but_keeps_pinned() {
        // code-mode `only`: exec/wait (pinned) stay in wire; flat builtins drop
        // out of wire and into the tool_search-loadable manifest.
        let pin = PinPolicy {
            patterns: vec!["exec".into()],
        };
        let builtins = vec![mk("exec", "code surface"), mk("file_read", "read a file")];

        // optional/off (defer_builtins=false): both visible in wire.
        let optional = Registry::with_options(
            builtins.clone(),
            vec![],
            &pin,
            &ToolFilter::default(),
            false,
        )
        .unwrap();
        let wire: Vec<String> = optional.wire_specs().into_iter().map(|s| s.name).collect();
        assert!(wire.contains(&"exec".to_string()));
        assert!(
            wire.contains(&"file_read".to_string()),
            "optional: {wire:?}"
        );

        // only (defer_builtins=true): exec pinned/visible, file_read deferred.
        let only =
            Registry::with_options(builtins, vec![], &pin, &ToolFilter::default(), true).unwrap();
        let wire: Vec<String> = only.wire_specs().into_iter().map(|s| s.name).collect();
        assert!(wire.contains(&"exec".to_string()), "only wire: {wire:?}");
        assert!(
            !wire.contains(&"file_read".to_string()),
            "only wire: {wire:?}"
        );
        assert!(
            only.manifest().iter().any(|(n, _)| n == "file_read"),
            "deferred builtin must be tool_search-loadable"
        );
    }

    /// Helper: collect every tool name the registry would expose — wire (pinned
    /// + eager + activated) plus the deferred manifest. A denied tool appears in
    ///   neither.
    fn all_known(reg: &Registry) -> Vec<String> {
        let mut v: Vec<String> = reg.wire_specs().into_iter().map(|s| s.name).collect();
        v.extend(reg.manifest().into_iter().map(|(n, _)| n));
        v
    }

    #[test]
    fn filter_denies_builtins_by_bare_name() {
        let pin = PinPolicy { patterns: vec![] };
        let builtins = vec![
            mk("file_read", "read"),
            mk("shell_run", "shell"),
            mk("git_ro_status", "git status"),
            mk("git_local_commit", "commit"),
        ];
        // Deny the shell + the git write family by glob; keep git read.
        let filter = ToolFilter::from_csv(Some("shell_*,git_local_*"), None);
        let reg = Registry::new(builtins, vec![], &pin, &filter).unwrap();
        let known = all_known(&reg);

        assert!(known.contains(&"file_read".to_string()), "kept: {known:?}");
        assert!(
            known.contains(&"git_ro_status".to_string()),
            "kept: {known:?}"
        );
        assert!(
            !known.contains(&"shell_run".to_string()),
            "denied: {known:?}"
        );
        assert!(
            !known.contains(&"git_local_commit".to_string()),
            "denied: {known:?}"
        );
    }

    #[test]
    fn allowlist_is_exclusive_over_builtins() {
        let pin = PinPolicy { patterns: vec![] };
        let builtins = vec![
            mk("file_read", "read"),
            mk("file_edit", "edit"),
            mk("shell_run", "shell"),
        ];
        // Explore-style: only read tools admitted.
        let filter = ToolFilter::from_csv(None, Some("file_read"));
        let reg = Registry::new(builtins, vec![], &pin, &filter).unwrap();
        let known = all_known(&reg);

        assert!(known.contains(&"file_read".to_string()), "{known:?}");
        assert!(!known.contains(&"file_edit".to_string()), "{known:?}");
        assert!(!known.contains(&"shell_run".to_string()), "{known:?}");
        // tool_search survives an exclusive allow-list (decision b).
        assert!(known.contains(&TOOL_SEARCH.to_string()), "{known:?}");
    }

    #[test]
    fn tool_search_survives_allowlist_but_dies_on_explicit_deny() {
        let pin = PinPolicy { patterns: vec![] };

        // Narrow allow-list that does NOT name tool_search → it still appears.
        let kept = Registry::new(
            vec![mk("file_read", "read")],
            vec![],
            &pin,
            &ToolFilter::from_csv(None, Some("file_read")),
        )
        .unwrap();
        assert!(
            all_known(&kept).contains(&TOOL_SEARCH.to_string()),
            "tool_search must survive an allow-list it isn't named in"
        );

        // Explicit deny → tool_search is gone.
        let gone = Registry::new(
            vec![mk("file_read", "read")],
            vec![],
            &pin,
            &ToolFilter::from_csv(Some("tool_search"), None),
        )
        .unwrap();
        assert!(
            !all_known(&gone).contains(&TOOL_SEARCH.to_string()),
            "explicit deny must remove tool_search"
        );
    }

    #[tokio::test]
    async fn tool_search_compact_by_default_and_schema_opt_in() {
        let reg = Registry::new(
            vec![],
            vec![mk("bbox_stats", "corpus stats")],
            &PinPolicy { patterns: vec![] },
            &ToolFilter::default(),
        )
        .unwrap();
        let cx = test_cx(bro_tools::ToolArgDefaults::default());

        let compact = reg
            .dispatch("tool_search", json!({"query":"stats"}), &cx)
            .await;
        match compact {
            ToolResult::Json(v) => {
                assert_eq!(v["loaded"], json!(["bbox_stats"]));
                assert!(v["tools"][0]["input_schema"].is_null(), "{v}");
                assert_eq!(v["remaining"]["count"], json!(0));
            }
            other => panic!("expected json, got {other:?}"),
        }

        let reg = Registry::new(
            vec![],
            vec![mk("bbox_stats", "corpus stats")],
            &PinPolicy { patterns: vec![] },
            &ToolFilter::default(),
        )
        .unwrap();
        let verbose = reg
            .dispatch(
                "tool_search",
                json!({"query":"stats","include_schemas":true}),
                &cx,
            )
            .await;
        match verbose {
            ToolResult::Json(v) => {
                assert!(v["tools"][0]["input_schema"].is_object(), "{v}");
            }
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[test]
    fn mcp_placement_splits_model_registry_and_denied_presence() {
        let mcp = vec![
            mk("mcp__srv__in_only", "in only"),
            mk("mcp__srv__out_only", "out only"),
            mk("mcp__srv__both", "both"),
            mk("mcp__srv__denied", "denied"),
        ];
        let filter = ToolFilter::from_csv(Some("mcp__srv__denied"), None);
        let filtered: Vec<_> = mcp
            .into_iter()
            .filter(|tool| filter.permits(tool.name()))
            .collect();
        let placements = crate::mcp::ToolPlacementMap::from([
            (
                "mcp__srv__in_only".to_string(),
                crate::mcp::ToolPlacement::InBox,
            ),
            (
                "mcp__srv__both".to_string(),
                crate::mcp::ToolPlacement::Both,
            ),
        ]);
        let (in_box, out_box) = crate::mcp::split_mcp_tools_by_placement(&filtered, &placements);
        let in_names: Vec<_> = in_box.iter().map(|tool| tool.name()).collect();
        assert_eq!(in_names, vec!["mcp__srv__in_only", "mcp__srv__both"]);

        let reg = Registry::new(vec![], out_box, &PinPolicy { patterns: vec![] }, &filter).unwrap();
        assert!(!reg.contains("mcp__srv__in_only"));
        assert!(reg.contains("mcp__srv__out_only"));
        assert!(reg.contains("mcp__srv__both"));
        assert!(!reg.contains("mcp__srv__denied"));
        assert!(!in_names.contains(&"mcp__srv__denied"));
    }
}
