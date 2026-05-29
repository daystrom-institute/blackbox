//! Tool registry with a three-tier deferral model — our own uniform,
//! client-side equivalent of Claude Code's Tool Search, so behavior is
//! identical across all transports (the Anthropic-native `defer_loading`
//! server feature isn't available on GLM/DeepSeek/OpenAI endpoints).
//!
//! Tiers:
//!
//! - `Pinned`: always in the wire `tools` array (first) AND surfaced
//!   prominently in the system prompt. Elevated by `PinPolicy` (default
//!   `bbox_slice_*`), plus `tool_search` itself.
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Pinned,
    Eager,
    Deferred,
}

/// Which tools get elevated to `Pinned`. Patterns are exact names or a
/// trailing-`*` prefix glob. Default elevates the slice tools; override with
/// `BRO_HARNESS_PIN_TOOLS` (comma-separated).
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
            Err(_) => vec!["bbox_slice_*".to_string()],
        };
        Self { patterns }
    }

    pub fn matches(&self, name: &str) -> bool {
        self.patterns.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == p,
        })
    }
}

struct Entry {
    tool: Arc<dyn Tool>,
    tier: Tier,
}

pub struct Registry {
    tools: HashMap<String, Entry>,
    activated: Arc<Mutex<HashSet<String>>>,
}

impl Registry {
    /// Built-ins default `Eager`, MCP tools default `Deferred`; `pin` elevates
    /// matching names (from either origin) to `Pinned`. A `tool_search`
    /// meta-tool is added as `Pinned` unless explicitly denied.
    ///
    /// `filter` gates **built-ins** by their bare name (`shell_run`, `git_*`,
    /// …) — the same allow/deny plane that filters MCP tools, applied uniformly
    /// so a profile can deny a built-in family. (MCP tools arrive already
    /// filtered by their fully-qualified name in `load_mcp_tools`, where the
    /// `mcp__server__tool` name exists; here their `.name()` is bare, so they
    /// are not re-filtered.) `tool_search` honors an explicit deny but ignores
    /// allow-list exclusion, so a narrow allow-list doesn't strand a bro that
    /// needs to load deferred tools.
    pub fn new(
        builtins: Vec<Arc<dyn Tool>>,
        mcp: Vec<Arc<dyn Tool>>,
        pin: &PinPolicy,
        filter: &ToolFilter,
    ) -> Self {
        let mut tools: HashMap<String, Entry> = HashMap::new();
        for t in builtins {
            if !filter.permits(t.name()) {
                continue;
            }
            let tier = if pin.matches(t.name()) {
                Tier::Pinned
            } else {
                Tier::Eager
            };
            tools.insert(t.name().to_string(), Entry { tool: t, tier });
        }
        for t in mcp {
            let tier = if pin.matches(t.name()) {
                Tier::Pinned
            } else {
                Tier::Deferred
            };
            // Last writer wins; an MCP tool may intentionally shadow a built-in.
            tools.insert(t.name().to_string(), Entry { tool: t, tier });
        }

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
                    tool: search,
                    tier: Tier::Pinned,
                },
            );
        }

        Self { tools, activated }
    }

    fn spec_of(e: &Entry) -> ToolSpec {
        ToolSpec {
            name: e.tool.name().to_string(),
            description: e.tool.description().to_string(),
            schema: e.tool.input_schema(),
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

    pub async fn dispatch(&self, name: &str, input: Value, cx: &ToolCx) -> ToolResult {
        match self.tools.get(name) {
            Some(e) => e.tool.call(input, cx).await,
            None => ToolResult::Error(format!("unknown tool: {name}")),
        }
    }
}

fn short_desc(d: &str) -> String {
    let line = d.lines().next().unwrap_or("").trim();
    if line.len() > 100 {
        format!("{}…", &line[..line.char_indices().take(100).last().map(|(i, _)| i).unwrap_or(line.len())])
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
}

struct ToolSearchTool {
    catalog: Arc<Vec<DeferredEntry>>,
    activated: Arc<Mutex<HashSet<String>>>,
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH
    }
    fn description(&self) -> &str {
        "Search for and load additional tools not in the always-available set. Pass a keyword query (e.g. \"slice edit\") or `select:name1,name2` for exact names. Returns the matching tool schemas and makes them callable on subsequent turns."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keyword query, or `select:nameA,nameB` to load exact tool names."
                }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let query = input["query"].as_str().unwrap_or("").trim().to_string();
        if query.is_empty() {
            return ToolResult::Error("query is required".into());
        }

        let matches: Vec<&DeferredEntry> = if let Some(sel) = query.strip_prefix("select:") {
            let names: HashSet<&str> = sel.split(',').map(|s| s.trim()).collect();
            self.catalog
                .iter()
                .filter(|e| names.contains(e.name.as_str()))
                .collect()
        } else {
            let terms: Vec<String> = query
                .to_lowercase()
                .split_whitespace()
                .map(String::from)
                .collect();
            let mut scored: Vec<(usize, &DeferredEntry)> = self
                .catalog
                .iter()
                .filter_map(|e| {
                    let hay = format!("{} {}", e.name, e.description).to_lowercase();
                    let score = terms.iter().filter(|t| hay.contains(t.as_str())).count();
                    (score > 0).then_some((score, e))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
            scored.into_iter().take(8).map(|(_, e)| e).collect()
        };

        if matches.is_empty() {
            return ToolResult::Text(format!("no tools matched '{query}'"));
        }

        let mut activated = self.activated.lock().unwrap();
        let mut loaded = Vec::new();
        for e in &matches {
            activated.insert(e.name.clone());
            loaded.push(json!({
                "name": e.name,
                "description": e.description,
                "input_schema": e.schema,
            }));
        }
        ToolResult::Json(json!({
            "loaded": matches.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
            "tools": loaded,
            "note": "These tools are now callable.",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_policy_prefix_and_exact() {
        let p = PinPolicy {
            patterns: vec!["bbox_slice_*".into(), "exact_tool".into()],
        };
        assert!(p.matches("bbox_slice_read"));
        assert!(p.matches("bbox_slice_copy"));
        assert!(p.matches("exact_tool"));
        assert!(!p.matches("bbox_stats"));
        assert!(!p.matches("exact_tool_x"));
    }

    fn mk(name: &'static str, desc: &'static str) -> Arc<dyn Tool> {
        struct T(&'static str, &'static str);
        #[async_trait]
        impl Tool for T {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                self.1
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object", "properties": {}})
            }
            async fn call(&self, _i: Value, _c: &ToolCx) -> ToolResult {
                ToolResult::Text("ok".into())
            }
        }
        Arc::new(T(name, desc))
    }

    #[test]
    fn tiers_and_activation() {
        let pin = PinPolicy {
            patterns: vec!["bbox_slice_*".into()],
        };
        let builtins = vec![mk("file_read", "read a file")];
        let mcp = vec![
            mk("bbox_slice_read", "read a slice"),
            mk("bbox_stats", "corpus stats"),
        ];
        let reg = Registry::new(builtins, mcp, &pin, &ToolFilter::default());

        // Pinned = slice tool + tool_search; Eager = file_read; both in wire.
        let wire: Vec<String> = reg.wire_specs().into_iter().map(|s| s.name).collect();
        assert!(wire.contains(&"bbox_slice_read".to_string()));
        assert!(wire.contains(&TOOL_SEARCH.to_string()));
        assert!(wire.contains(&"file_read".to_string()));
        // bbox_stats is deferred → not in wire yet, but in the manifest.
        assert!(!wire.contains(&"bbox_stats".to_string()));
        assert!(reg.manifest().iter().any(|(n, _)| n == "bbox_stats"));

        // Activate it → now in wire, gone from manifest.
        reg.activated.lock().unwrap().insert("bbox_stats".to_string());
        let wire: Vec<String> = reg.wire_specs().into_iter().map(|s| s.name).collect();
        assert!(wire.contains(&"bbox_stats".to_string()));
        assert!(!reg.manifest().iter().any(|(n, _)| n == "bbox_stats"));
    }

    /// Helper: collect every tool name the registry would expose — wire (pinned
    /// + eager + activated) plus the deferred manifest. A denied tool appears in
    /// neither.
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
        let reg = Registry::new(builtins, vec![], &pin, &filter);
        let known = all_known(&reg);

        assert!(known.contains(&"file_read".to_string()), "kept: {known:?}");
        assert!(known.contains(&"git_ro_status".to_string()), "kept: {known:?}");
        assert!(!known.contains(&"shell_run".to_string()), "denied: {known:?}");
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
        let reg = Registry::new(builtins, vec![], &pin, &filter);
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
        );
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
        );
        assert!(
            !all_known(&gone).contains(&TOOL_SEARCH.to_string()),
            "explicit deny must remove tool_search"
        );
    }
}
