//! Probe substrate for the macro layer (Phase 4, P4a).
//!
//! A "probe" evaluates a read-only code-nav query at macro plan time and
//! stores a normalized JSON result into [`crate::macros::expr::Context`]
//! under the probe's name slot, where refusal predicates and later
//! operations can reference it via dotted paths like `"binding.exists"`.
//!
//! # Components
//!
//! - [`ProbeSpec`] — typed, serde-tagged enum. Deserialized from
//!   [`crate::macros::model::MacroProbe::spec`]. **Bounded** to v1 kinds;
//!   unknown discriminants are serde errors (fail closed).
//! - [`ProbeOutput`] — normalized result stored in `Context.probes[name]`.
//!   Shape: `{ exists: bool, count: usize, <kind-arrays> }`. Stable across
//!   probe reruns so refusal predicates can rely on dotted paths.
//! - [`ProbeRunner`] — trait; single `run_probe` method.
//! - [`UnavailableProbeRunner`] — fail-closed default: every call returns
//!   `error.probe_backend_unavailable`.
//! - [`CodeNavProbeRunner`] — real runner backed by `code_query`,
//!   `code_symbols_live`, and optionally `java_workspace_symbols`.
//!
//! # Safety invariants
//!
//! - Probe execution is **read-only** (no file mutations).
//! - [`ProbeSpec`] is **data** (typed enum; no arbitrary code).
//! - **Containment**: `CodeQuery.file` is resolved under
//!   `invocation.project_dir`; absolute paths outside the project and
//!   `..` traversal are rejected.
//! - **Caps**: query string length, returned items per probe, and total
//!   normalized `value` JSON size are all bounded. Exceeded caps set
//!   `truncated=true` and add a diagnostic message.
//! - **Fail closed (RX-V3)**: `WorkspaceSymbol` without an LSP session
//!   returns `Err("error.lsp_unavailable")`.
//! - `project_local` per symbol: only symbols whose file path is under
//!   the project dir are counted in `exists`/`count`.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::code_nav::semantic::{WorkspaceSymbolItem, java_workspace_symbols};
use crate::code_nav::{
    CodeQueryParams, CodeQueryResponse, CodeSymbolSearchParams, CodeSymbolSearchResponse,
};
use crate::lsp::LspSessionManager;
use crate::macros::expr::Context;
use crate::macros::model::{MacroInvocation, MacroSemanticStatus};
use crate::projects::ProjectRecord;

// ---------------------------------------------------------------------------
// Caps
// ---------------------------------------------------------------------------

/// Maximum query string length (bytes) accepted by the runner.
const MAX_QUERY_BYTES: usize = 4 * 1024;

/// Maximum items returned per probe (matches / symbols / items arrays).
const MAX_ITEMS_PER_PROBE: usize = 200;

/// Maximum total JSON size of a normalized `ProbeOutput.value` (bytes).
const MAX_VALUE_JSON_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// ProbeSpec
// ---------------------------------------------------------------------------

/// Typed discriminated-union spec for a named probe slot.
///
/// Serde tag is `"kind"` so definitions look like:
/// ```json
/// { "kind": "code_query", "file": "src/Foo.java", "query": "(class_declaration) @c" }
/// ```
///
/// **Unknown `kind` values are a deserialization error** — the spec is
/// bounded to v1 kinds (fail closed). Do not add free-form kinds here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeSpec {
    /// Syntactic tree-sitter query over a single file within the project.
    ///
    /// `file` is a project-relative path (or bare filename). Absolute paths
    /// and `..` traversal are rejected at run time (containment check).
    CodeQuery {
        /// Project-relative file path to query.
        file: String,
        /// Tree-sitter S-expression query, e.g. `"(class_declaration) @c"`.
        query: String,
        /// Optional language override (e.g. `"java"`, `"rust"`).
        #[serde(default)]
        language: Option<String>,
    },

    /// Walk-and-parse project symbol scan (syntactic, no server index).
    ///
    /// Backed by `code_symbols_live` — works without the daemon's tantivy
    /// index.
    CodeSymbols {
        /// Optional substring filter applied to item name / kind / path.
        #[serde(default)]
        query: Option<String>,
        /// Optional item kind filter, e.g. `["class_declaration"]`.
        #[serde(default)]
        item_kinds: Option<Vec<String>>,
        /// Optional language filter, e.g. `["java"]`.
        #[serde(default)]
        languages: Option<Vec<String>>,
    },

    /// Semantic workspace-symbol lookup via JDTLS.
    ///
    /// Requires an active [`LspSessionManager`]. Fails closed with
    /// `error.lsp_unavailable` when the LSP is absent or unavailable
    /// (mirrors RX-V3 — never silently downgrades to syntactic).
    WorkspaceSymbol {
        /// Query string forwarded to JDTLS `workspace/symbol`.
        query: String,
    },
}

// ---------------------------------------------------------------------------
// ProbeOutput
// ---------------------------------------------------------------------------

/// Normalized result stored in `Context.probes[name]`.
///
/// `value` always carries at least `{ "exists": bool, "count": usize }` plus
/// kind-specific arrays so refusal predicates can use stable dotted paths
/// like `"binding.exists"` or `"binding.count"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeOutput {
    /// Normalized JSON value stored into `Context.probes[name]`.
    ///
    /// Shape per kind:
    /// - `code_query`: `{ exists, count, matches: [...capped...] }`
    /// - `code_symbols`: `{ exists, count, items: [...capped...] }`
    /// - `workspace_symbol`: `{ exists, count, symbols: [...] }` where each
    ///   symbol carries `project_local: bool`.
    pub value: Value,

    /// Semantic verification tier achieved for this probe.
    pub semantic_status: MacroSemanticStatus,

    /// True when the result arrays were truncated to stay under caps.
    pub truncated: bool,

    /// Diagnostic messages (cap exceeded, parse warnings, etc.).
    pub diagnostics: Vec<String>,
}

// ---------------------------------------------------------------------------
// ProbeRunner trait
// ---------------------------------------------------------------------------

/// Executes a named probe spec and returns a normalized [`ProbeOutput`].
///
/// Implementations must be `Send + Sync` so they can be boxed and stored in
/// [`crate::macros::planner_ctx::MacroPlannerContext`].
///
/// # Contract
///
/// - **Read-only**: no files are mutated.
/// - **Fail closed**: unknown / unavailable backends return `Err`, never
///   silently degrade.
pub trait ProbeRunner: Send + Sync {
    /// Run the probe identified by `name` against `spec`.
    ///
    /// `ctx` provides the current evaluation context (inputs + prior probes).
    /// `invocation` provides `project_dir` and other invocation metadata.
    fn run_probe(
        &self,
        name: &str,
        spec: &ProbeSpec,
        ctx: &Context,
        invocation: &MacroInvocation,
    ) -> Result<ProbeOutput>;
}

// ---------------------------------------------------------------------------
// UnavailableProbeRunner — fail-closed default
// ---------------------------------------------------------------------------

/// Fail-closed stub returned when no real code-nav backend is wired up.
///
/// Every `run_probe` call returns `Err("error.probe_backend_unavailable")`.
/// This is the default in [`crate::macros::planner_ctx::MacroPlannerContext`]
/// until a real runner is injected at service startup.
pub struct UnavailableProbeRunner;

impl ProbeRunner for UnavailableProbeRunner {
    fn run_probe(
        &self,
        name: &str,
        _spec: &ProbeSpec,
        _ctx: &Context,
        _invocation: &MacroInvocation,
    ) -> Result<ProbeOutput> {
        Err(anyhow!(
            "error.probe_backend_unavailable: no probe runner is wired up; \
             cannot evaluate probe '{name}'"
        ))
    }
}

// ---------------------------------------------------------------------------
// CodeNavProbeRunner — real runner
// ---------------------------------------------------------------------------

/// Real probe runner backed by code-nav primitives.
///
/// `lsp` is required only for [`ProbeSpec::WorkspaceSymbol`]. Syntactic
/// probes (`CodeQuery`, `CodeSymbols`) work without it.
pub struct CodeNavProbeRunner {
    /// Optional LSP session manager for `workspace_symbol` probes.
    pub lsp: Option<LspSessionManager>,
    /// Registered projects passed to `code_symbols_live` containment check.
    pub registered: Vec<ProjectRecord>,
}

impl CodeNavProbeRunner {
    /// Construct a runner with an optional LSP manager.
    pub fn new(lsp: Option<LspSessionManager>, registered: Vec<ProjectRecord>) -> Self {
        Self { lsp, registered }
    }

    // -----------------------------------------------------------------------
    // Containment check
    // -----------------------------------------------------------------------

    /// Resolve `file` under `project_dir` and verify containment.
    ///
    /// Rejects:
    /// - Absolute paths that resolve outside `project_dir`.
    /// - Any path component equal to `..`.
    /// - Paths that, after canonicalization, do not start with
    ///   `canonical_project_dir`.
    fn resolve_contained_file(project_dir: &str, file: &str) -> Result<PathBuf> {
        // Reject raw `..` components before any path join (pre-join guard).
        if file.split('/').any(|c| c == "..") || file.split('\\').any(|c| c == "..") {
            return Err(anyhow!(
                "error.path_traversal: probe file '{file}' contains '..'; \
                 path traversal is not allowed"
            ));
        }

        let project_path = PathBuf::from(project_dir);

        // If the provided file path is absolute, reject it outright unless it
        // is already under project_dir (we check after canonicalization below).
        let raw = if Path::new(file).is_absolute() {
            PathBuf::from(file)
        } else {
            project_path.join(file)
        };

        // Canonicalize project dir for the containment comparison.
        let canonical_project = project_path
            .canonicalize()
            .map_err(|e| anyhow!("error.project_dir_invalid: {e}"))?;

        // Canonicalize the candidate path (requires it to exist).
        let canonical_file = raw
            .canonicalize()
            .map_err(|e| anyhow!("error.file_not_found: probe file '{file}': {e}"))?;

        if !canonical_file.starts_with(&canonical_project) {
            return Err(anyhow!(
                "error.path_traversal: probe file '{}' is outside project dir '{}'",
                canonical_file.display(),
                canonical_project.display(),
            ));
        }

        Ok(canonical_file)
    }

    // -----------------------------------------------------------------------
    // CodeQuery
    // -----------------------------------------------------------------------

    fn run_code_query(
        &self,
        file: &str,
        query: &str,
        language: Option<&str>,
        project_dir: &str,
    ) -> Result<ProbeOutput> {
        // Cap query length.
        if query.len() > MAX_QUERY_BYTES {
            return Err(anyhow!(
                "error.query_too_long: probe query is {} bytes (max {})",
                query.len(),
                MAX_QUERY_BYTES
            ));
        }

        // Containment check — resolve file under project_dir.
        let contained = Self::resolve_contained_file(project_dir, file)?;
        let contained_str = contained.to_string_lossy().into_owned();

        let params = CodeQueryParams {
            file: contained_str,
            query: query.to_owned(),
            project_dir: None, // file is already absolute after containment check
            language: language.map(|s| s.to_owned()),
            limit: Some(MAX_ITEMS_PER_PROBE),
            include_text: Some(true),
        };

        let json_str = crate::code_nav::code_query(&params)?;
        let response: CodeQueryResponse = serde_json::from_str(&json_str)?;

        let mut diagnostics = Vec::new();
        let mut truncated = response.truncated;

        // Cap the matches array.
        let captures: Vec<Value> = response
            .captures
            .iter()
            .take(MAX_ITEMS_PER_PROBE)
            .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
            .collect();
        if captures.len() < response.captures.len() {
            truncated = true;
            diagnostics.push(format!(
                "matches array capped at {} (total matching_captures={})",
                captures.len(),
                response.matching_captures
            ));
        }

        let count = response.matching_captures;
        let exists = count > 0;

        // Build normalized value; enforce JSON size cap.
        let value = Self::cap_value(
            serde_json::json!({
                "exists": exists,
                "count": count,
                "matches": captures
            }),
            &mut truncated,
            &mut diagnostics,
            "matches",
        );

        Ok(ProbeOutput {
            value,
            semantic_status: MacroSemanticStatus::SyntaxOnly,
            truncated,
            diagnostics,
        })
    }

    // -----------------------------------------------------------------------
    // CodeSymbols
    // -----------------------------------------------------------------------

    fn run_code_symbols(
        &self,
        query: Option<&str>,
        item_kinds: Option<&[String]>,
        languages: Option<&[String]>,
        project_dir: &str,
    ) -> Result<ProbeOutput> {
        // Cap query length.
        if let Some(q) = query {
            if q.len() > MAX_QUERY_BYTES {
                return Err(anyhow!(
                    "error.query_too_long: probe query is {} bytes (max {})",
                    q.len(),
                    MAX_QUERY_BYTES
                ));
            }
        }

        let params = CodeSymbolSearchParams {
            project_dir: project_dir.to_owned(),
            query: query.map(|s| s.to_owned()),
            languages: languages.map(|v| v.to_vec()),
            item_kinds: item_kinds.map(|v| v.to_vec()),
            path_contains: None,
            limit: Some(MAX_ITEMS_PER_PROBE),
            file_limit: None,
            include_attributes: None,
            mode: Some("live".to_owned()),
        };

        let json_str = crate::code_nav::code_symbols_live(&params, &self.registered)?;
        let response: CodeSymbolSearchResponse = serde_json::from_str(&json_str)?;

        let mut diagnostics = Vec::new();
        let mut truncated = response.truncated;

        // Cap items array.
        let items: Vec<Value> = response
            .items
            .iter()
            .take(MAX_ITEMS_PER_PROBE)
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        if items.len() < response.items.len() {
            truncated = true;
            diagnostics.push(format!(
                "items array capped at {} (total matching_items={})",
                items.len(),
                response.matching_items
            ));
        }

        let count = response.matching_items;
        let exists = count > 0;

        let value = Self::cap_value(
            serde_json::json!({
                "exists": exists,
                "count": count,
                "items": items
            }),
            &mut truncated,
            &mut diagnostics,
            "items",
        );

        Ok(ProbeOutput {
            value,
            semantic_status: MacroSemanticStatus::SyntaxOnly,
            truncated,
            diagnostics,
        })
    }

    // -----------------------------------------------------------------------
    // WorkspaceSymbol
    // -----------------------------------------------------------------------

    fn run_workspace_symbol(&self, query: &str, project_dir: &str) -> Result<ProbeOutput> {
        // RX-V3: fail closed when LSP is absent.
        let lsp = self
            .lsp
            .as_ref()
            .ok_or_else(|| anyhow!("error.lsp_unavailable: no LspSessionManager configured"))?;

        // Cap query length.
        if query.len() > MAX_QUERY_BYTES {
            return Err(anyhow!(
                "error.query_too_long: probe query is {} bytes (max {})",
                query.len(),
                MAX_QUERY_BYTES
            ));
        }

        let project_path = PathBuf::from(project_dir)
            .canonicalize()
            .map_err(|e| anyhow!("error.project_dir_invalid: {e}"))?;

        // RX-V3: java_workspace_symbols already propagates lsp_unavailable errors.
        let report = java_workspace_symbols(lsp, &project_path, query)?;

        let mut diagnostics = Vec::new();
        let mut truncated = false;

        // Annotate each symbol with `project_local` (path under project_dir).
        let all_symbols: Vec<Value> = report
            .symbols
            .iter()
            .map(|sym| annotate_symbol(sym, &project_path))
            .collect();

        // Count only project-local symbols for `exists`/`count`.
        let local_count = all_symbols
            .iter()
            .filter(|v| {
                v.get("project_local")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false)
            })
            .count();

        // Cap the symbols array.
        let symbols: Vec<Value> = all_symbols.into_iter().take(MAX_ITEMS_PER_PROBE).collect();

        if symbols.len() < report.symbol_count {
            truncated = true;
            diagnostics.push(format!(
                "symbols array capped at {} (total symbol_count={})",
                symbols.len(),
                report.symbol_count
            ));
        }

        let exists = local_count > 0;

        let value = Self::cap_value(
            serde_json::json!({
                "exists": exists,
                "count": local_count,
                "symbols": symbols
            }),
            &mut truncated,
            &mut diagnostics,
            "symbols",
        );

        Ok(ProbeOutput {
            value,
            semantic_status: MacroSemanticStatus::LspVerified,
            truncated,
            diagnostics,
        })
    }

    // -----------------------------------------------------------------------
    // JSON size cap helper
    // -----------------------------------------------------------------------

    /// Enforce the `MAX_VALUE_JSON_BYTES` cap on the serialized value.
    ///
    /// Extracts `array_field` from `val`, progressively pops entries until
    /// the serialized size fits, then rebuilds `val` with the trimmed array.
    /// Sets `truncated = true` and appends a diagnostic when trimming occurs.
    fn cap_value(
        mut val: Value,
        truncated: &mut bool,
        diagnostics: &mut Vec<String>,
        array_field: &'static str,
    ) -> Value {
        let mut serialized = serde_json::to_string(&val).unwrap_or_default();
        if serialized.len() <= MAX_VALUE_JSON_BYTES {
            return val;
        }

        // Extract the inner array for trimming.
        let mut arr: Vec<Value> = match val.as_object_mut() {
            Some(obj) => match obj.remove(array_field) {
                Some(Value::Array(v)) => v,
                _ => return val,
            },
            None => return val,
        };

        // Trim the inner array until the value fits.
        loop {
            // Rebuild val with the current arr.
            if let Some(obj) = val.as_object_mut() {
                obj.insert(array_field.to_owned(), Value::Array(arr.clone()));
            }
            serialized = serde_json::to_string(&val).unwrap_or_default();
            if serialized.len() <= MAX_VALUE_JSON_BYTES || arr.is_empty() {
                break;
            }
            arr.pop();
        }

        *truncated = true;
        diagnostics.push(format!(
            "probe value JSON size capped at {} bytes; {array_field} trimmed to {} entries",
            MAX_VALUE_JSON_BYTES,
            arr.len()
        ));

        val
    }
}

/// Annotate a `WorkspaceSymbolItem` with a `project_local: bool` field.
fn annotate_symbol(sym: &WorkspaceSymbolItem, project_dir: &Path) -> Value {
    let is_local = if sym.path.starts_with("jdt://")
        || sym.path.starts_with("jdt:\\")
        || sym.path.contains("classpath")
    {
        // Clearly a JDT classpath URI — not project local.
        false
    } else {
        let p = Path::new(&sym.path);
        // Check via starts_with (canonicalization may fail for virtual paths).
        let canonical_sym = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        canonical_sym.starts_with(project_dir)
    };

    let mut v = serde_json::to_value(sym).unwrap_or(Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("project_local".to_owned(), Value::Bool(is_local));
    }
    v
}

impl ProbeRunner for CodeNavProbeRunner {
    fn run_probe(
        &self,
        _name: &str,
        spec: &ProbeSpec,
        _ctx: &Context,
        invocation: &MacroInvocation,
    ) -> Result<ProbeOutput> {
        match spec {
            ProbeSpec::CodeQuery {
                file,
                query,
                language,
            } => self.run_code_query(file, query, language.as_deref(), &invocation.project_dir),
            ProbeSpec::CodeSymbols {
                query,
                item_kinds,
                languages,
            } => self.run_code_symbols(
                query.as_deref(),
                item_kinds.as_deref(),
                languages.as_deref(),
                &invocation.project_dir,
            ),
            ProbeSpec::WorkspaceSymbol { query } => {
                self.run_workspace_symbol(query, &invocation.project_dir)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn minimal_invocation(project_dir: &str) -> MacroInvocation {
        MacroInvocation {
            macro_id: "test.probe".into(),
            version: None,
            project_dir: project_dir.to_owned(),
            inputs: serde_json::Map::new(),
            anchors: None,
            operator_opt_outs: vec![],
        }
    }

    // -----------------------------------------------------------------------
    // ProbeSpec deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn probe_spec_code_query_deserialize() {
        let json = json!({
            "kind": "code_query",
            "file": "src/Foo.java",
            "query": "(class_declaration) @c"
        });
        let spec: ProbeSpec = serde_json::from_value(json).expect("deserialize CodeQuery");
        assert!(matches!(
            spec,
            ProbeSpec::CodeQuery { file, query, language: None }
            if file == "src/Foo.java" && query == "(class_declaration) @c"
        ));
    }

    #[test]
    fn probe_spec_code_query_with_language_deserialize() {
        let json = json!({
            "kind": "code_query",
            "file": "src/Foo.java",
            "query": "(method_declaration) @m",
            "language": "java"
        });
        let spec: ProbeSpec = serde_json::from_value(json).expect("deserialize CodeQuery+lang");
        assert!(matches!(
            spec,
            ProbeSpec::CodeQuery { language: Some(l), .. } if l == "java"
        ));
    }

    #[test]
    fn probe_spec_code_symbols_deserialize() {
        let json = json!({
            "kind": "code_symbols",
            "query": "PaymentService",
            "item_kinds": ["class_declaration"],
            "languages": ["java"]
        });
        let spec: ProbeSpec = serde_json::from_value(json).expect("deserialize CodeSymbols");
        assert!(matches!(
            spec,
            ProbeSpec::CodeSymbols { query: Some(q), .. } if q == "PaymentService"
        ));
    }

    #[test]
    fn probe_spec_code_symbols_minimal_deserialize() {
        let json = json!({ "kind": "code_symbols" });
        let spec: ProbeSpec =
            serde_json::from_value(json).expect("deserialize CodeSymbols minimal");
        assert!(matches!(
            spec,
            ProbeSpec::CodeSymbols {
                query: None,
                item_kinds: None,
                languages: None
            }
        ));
    }

    #[test]
    fn probe_spec_workspace_symbol_deserialize() {
        let json = json!({
            "kind": "workspace_symbol",
            "query": "PaymentService"
        });
        let spec: ProbeSpec = serde_json::from_value(json).expect("deserialize WorkspaceSymbol");
        assert!(matches!(
            spec,
            ProbeSpec::WorkspaceSymbol { query: q } if q == "PaymentService"
        ));
    }

    #[test]
    fn probe_spec_unknown_kind_rejected() {
        let json = json!({
            "kind": "find_usages",
            "symbol": "Foo"
        });
        let result: Result<ProbeSpec, _> = serde_json::from_value(json);
        assert!(
            result.is_err(),
            "unknown kind 'find_usages' should be rejected"
        );
    }

    #[test]
    fn probe_spec_empty_kind_rejected() {
        let json = json!({ "kind": "composite" });
        let result: Result<ProbeSpec, _> = serde_json::from_value(json);
        assert!(result.is_err(), "unknown kind should be rejected");
    }

    // -----------------------------------------------------------------------
    // ProbeSpec round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn probe_spec_serde_round_trip() {
        let specs = vec![
            ProbeSpec::CodeQuery {
                file: "src/Foo.java".into(),
                query: "(class_declaration) @c".into(),
                language: Some("java".into()),
            },
            ProbeSpec::CodeSymbols {
                query: Some("Service".into()),
                item_kinds: Some(vec!["class_declaration".into()]),
                languages: Some(vec!["java".into()]),
            },
            ProbeSpec::WorkspaceSymbol {
                query: "PaymentService".into(),
            },
        ];
        for spec in specs {
            let json = serde_json::to_string(&spec).expect("serialize");
            let back: ProbeSpec = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(spec, back, "round-trip failed for {json}");
        }
    }

    // -----------------------------------------------------------------------
    // UnavailableProbeRunner — fail closed for all kinds
    // -----------------------------------------------------------------------

    fn unavailable_runner() -> UnavailableProbeRunner {
        UnavailableProbeRunner
    }

    #[test]
    fn unavailable_runner_code_query_fails_closed() {
        let runner = unavailable_runner();
        let spec = ProbeSpec::CodeQuery {
            file: "src/Foo.java".into(),
            query: "(class_declaration) @c".into(),
            language: None,
        };
        let inv = minimal_invocation("/tmp");
        let err = runner
            .run_probe("binding", &spec, &Context::default(), &inv)
            .unwrap_err();
        assert!(
            err.to_string().contains("error.probe_backend_unavailable"),
            "expected backend_unavailable, got: {err}"
        );
    }

    #[test]
    fn unavailable_runner_code_symbols_fails_closed() {
        let runner = unavailable_runner();
        let spec = ProbeSpec::CodeSymbols {
            query: None,
            item_kinds: None,
            languages: None,
        };
        let inv = minimal_invocation("/tmp");
        let err = runner
            .run_probe("syms", &spec, &Context::default(), &inv)
            .unwrap_err();
        assert!(
            err.to_string().contains("error.probe_backend_unavailable"),
            "expected backend_unavailable, got: {err}"
        );
    }

    #[test]
    fn unavailable_runner_workspace_symbol_fails_closed() {
        let runner = unavailable_runner();
        let spec = ProbeSpec::WorkspaceSymbol {
            query: "Foo".into(),
        };
        let inv = minimal_invocation("/tmp");
        let err = runner
            .run_probe("ws", &spec, &Context::default(), &inv)
            .unwrap_err();
        assert!(
            err.to_string().contains("error.probe_backend_unavailable"),
            "expected backend_unavailable, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Containment checks
    // -----------------------------------------------------------------------

    #[test]
    fn containment_rejects_dotdot_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_dir = dir.path().to_str().unwrap().to_owned();
        let result = CodeNavProbeRunner::resolve_contained_file(&project_dir, "../escape");
        assert!(result.is_err(), "expected containment error for ../escape");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("error.path_traversal") || msg.contains(".."),
            "error should mention traversal: {msg}"
        );
    }

    #[test]
    fn containment_rejects_absolute_outside_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_dir = dir.path().to_str().unwrap().to_owned();
        // /tmp is almost certainly outside a newly created tempdir
        let result = CodeNavProbeRunner::resolve_contained_file(&project_dir, "/etc/passwd");
        assert!(
            result.is_err(),
            "expected containment error for absolute path outside project"
        );
    }

    #[test]
    fn containment_rejects_nested_dotdot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_dir = dir.path().to_str().unwrap().to_owned();
        let result =
            CodeNavProbeRunner::resolve_contained_file(&project_dir, "src/main/../../../etc/x");
        assert!(
            result.is_err(),
            "expected containment error for nested dotdot"
        );
    }

    #[test]
    fn containment_accepts_relative_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_dir = dir.path().to_str().unwrap().to_owned();
        let file_path = dir.path().join("src").join("Foo.java");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, "class Foo {}").unwrap();

        let result = CodeNavProbeRunner::resolve_contained_file(&project_dir, "src/Foo.java");
        assert!(result.is_ok(), "expected Ok for valid contained file");
        assert!(result.unwrap().starts_with(dir.path()));
    }

    // -----------------------------------------------------------------------
    // WorkspaceSymbol with lsp=None → error.lsp_unavailable
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_symbol_no_lsp_fails_closed() {
        let runner = CodeNavProbeRunner::new(None, vec![]);
        let spec = ProbeSpec::WorkspaceSymbol {
            query: "Foo".into(),
        };
        let inv = minimal_invocation("/tmp");
        let err = runner
            .run_probe("ws", &spec, &Context::default(), &inv)
            .unwrap_err();
        assert!(
            err.to_string().contains("error.lsp_unavailable"),
            "expected lsp_unavailable, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // CodeQuery over tempdir fixture
    // -----------------------------------------------------------------------

    #[test]
    fn code_query_runner_returns_normalized_output() {
        // Create a small Rust source file in a tempdir and probe it.
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let file_path = src_dir.join("foo.rs");
        fs::write(&file_path, "pub struct Foo {}\npub struct Bar {}\n").unwrap();

        // Register the project dir so code_symbols_live is happy (probe uses
        // code_query, not code_symbols; register is a no-op guard here).
        let runner = CodeNavProbeRunner::new(None, vec![]);

        let spec = ProbeSpec::CodeQuery {
            file: "src/foo.rs".to_owned(),
            query: "(struct_item) @s".to_owned(),
            language: Some("rust".to_owned()),
        };
        let inv = minimal_invocation(dir.path().to_str().unwrap());
        let output = runner
            .run_probe("structs", &spec, &Context::default(), &inv)
            .expect("probe should succeed");

        // Normalized shape checks.
        let v = &output.value;
        assert!(v.get("exists").is_some(), "value must have 'exists'");
        assert!(v.get("count").is_some(), "value must have 'count'");
        assert!(v.get("matches").is_some(), "value must have 'matches'");
        assert_eq!(v["exists"], json!(true));
        assert!(
            v["count"].as_u64().unwrap_or(0) >= 2,
            "expected at least 2 struct captures"
        );
        assert_eq!(output.semantic_status, MacroSemanticStatus::SyntaxOnly);
    }

    // -----------------------------------------------------------------------
    // Caps: truncate large result
    // -----------------------------------------------------------------------

    #[test]
    fn code_query_cap_truncates_large_result() {
        // Create a file with many structs to exercise the cap.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut content = String::new();
        // Produce more structs than MAX_ITEMS_PER_PROBE (200). We use 210.
        for i in 0..210 {
            content.push_str(&format!("pub struct Struct{i} {{}}\n"));
        }
        let file_path = dir.path().join("many.rs");
        fs::write(&file_path, &content).unwrap();

        let runner = CodeNavProbeRunner::new(None, vec![]);
        let spec = ProbeSpec::CodeQuery {
            file: "many.rs".to_owned(),
            query: "(struct_item) @s".to_owned(),
            language: Some("rust".to_owned()),
        };
        let inv = minimal_invocation(dir.path().to_str().unwrap());
        let output = runner
            .run_probe("structs", &spec, &Context::default(), &inv)
            .expect("probe should succeed");

        let matches = output.value["matches"].as_array().expect("matches array");
        assert!(
            matches.len() <= MAX_ITEMS_PER_PROBE,
            "matches must be capped at MAX_ITEMS_PER_PROBE"
        );
        // Truncated flag or count > cap → truncated should be set.
        // (code_query itself may already truncate at limit=MAX_ITEMS_PER_PROBE,
        //  so we only assert the array is bounded.)
        assert!(
            output.value["count"].as_u64().unwrap_or(0) >= 200,
            "count should reflect total matches"
        );
    }

    // -----------------------------------------------------------------------
    // JSON size cap helper
    // -----------------------------------------------------------------------

    #[test]
    fn cap_value_trims_array_when_over_limit() {
        // Build a value whose array entries are large enough to exceed the cap.
        let large_entry = "x".repeat(10_000);
        let items: Vec<Value> = (0..20).map(|_| json!(large_entry)).collect();
        let val = json!({
            "exists": true,
            "count": 20usize,
            "items": items
        });
        let mut truncated = false;
        let mut diagnostics = Vec::new();

        let result = CodeNavProbeRunner::cap_value(val, &mut truncated, &mut diagnostics, "items");

        let serialized = serde_json::to_string(&result).unwrap();
        assert!(
            serialized.len() <= MAX_VALUE_JSON_BYTES,
            "capped value must fit in MAX_VALUE_JSON_BYTES"
        );
        assert!(truncated, "truncated must be set");
        assert!(!diagnostics.is_empty(), "diagnostics must mention the cap");
    }

    // -----------------------------------------------------------------------
    // project_local annotation
    // -----------------------------------------------------------------------

    /// Build a minimal JSON representation of a WorkspaceSymbolItem and
    /// deserialize it, so we avoid constructing the complex CodeRefactorHandoff
    /// struct directly in tests.
    fn make_ws_symbol_item_json(name: &str, path: &str) -> Value {
        json!({
            "name": name,
            "kind": "Class",
            "kind_number": 5u32,
            "path": path,
            "line": 0u32,
            "character": 0u32,
            "handoff": {
                "project_refs": {
                    "tool": "bbox_refactor_project_refs",
                    "arguments": {
                        "file": path,
                        "limit": 20,
                        "include_excerpt": false
                    }
                },
                "note": "test"
            }
        })
    }

    #[test]
    fn annotate_symbol_marks_local_vs_jdt() {
        use crate::code_nav::semantic::WorkspaceSymbolItem;

        let dir = tempfile::tempdir().expect("tempdir");
        let project_dir = dir.path().to_path_buf();

        // Create a real file for the local symbol.
        let local_file = dir.path().join("Foo.java");
        fs::write(&local_file, "class Foo {}").unwrap();
        let local_path = local_file.to_string_lossy().into_owned();

        let local_sym: WorkspaceSymbolItem =
            serde_json::from_value(make_ws_symbol_item_json("Foo", &local_path))
                .expect("deserialize local sym");
        let jdt_sym: WorkspaceSymbolItem = serde_json::from_value(make_ws_symbol_item_json(
            "ArrayList",
            "jdt://contents/rt.jar/java.util/ArrayList.class",
        ))
        .expect("deserialize jdt sym");

        let local_annotated = annotate_symbol(&local_sym, &project_dir);
        let jdt_annotated = annotate_symbol(&jdt_sym, &project_dir);

        assert_eq!(
            local_annotated.get("project_local"),
            Some(&json!(true)),
            "local file should be project_local=true"
        );
        assert_eq!(
            jdt_annotated.get("project_local"),
            Some(&json!(false)),
            "jdt:// path should be project_local=false"
        );
    }
}
