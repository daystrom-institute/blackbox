//! Transport-agnostic building blocks shared by the OpenAI Responses transports
//! (HTTP-SSE today; a WebSocket transport next — see
//! `design/bro-harness/brodex-websocket-transport.md`).
//!
//! Everything here is independent of the wire/connection mechanism: the auth
//! enum + resolution, the request-body builder, the SSE/event reconstruction,
//! the identity+auth header *values* (each transport applies them to its own
//! request type — reqwest builder vs WS handshake), and the small pure mappers.
//! Each transport owns only its connection lifecycle and framing.

use super::{StopReason, ToolCall, ToolResult, ToolSpec, TurnOpts, TurnOutput, Usage};
use anyhow::{Context, Result};
use bro_protocol::SERVICE_TIER_DEFAULT;
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

/// Auth material for a Responses request.
#[derive(Clone)]
pub(super) enum Auth {
    /// Standard OpenAI: `Authorization: Bearer <key>`.
    ApiKey(String),
    /// ChatGPT backend: bearer access token + account id.
    ChatGpt {
        access_token: String,
        account_id: String,
    },
}

/// The conversation + identity state shared by the HTTP-SSE and WebSocket
/// Responses transports. Owning this in one place lets the routing transport
/// keep a single buffer across a WS→HTTP fallback (no two-buffer divergence).
///
/// `Clone` backs transactional previews: a compaction request prepares a
/// copy, syncs its Lite catalog, and builds the body there, while the
/// authoritative state (and its baseline) changes only on success.
#[derive(Clone)]
pub(super) struct ResponsesState {
    pub auth: Auth,
    /// Stable per-session id (codex `session-id` header + `prompt_cache_key`).
    pub session_id: String,
    /// Per-turn id (codex `thread-id` header); rotated on each new user turn.
    pub thread_id: String,
    /// Flat Responses `input[]` buffer.
    pub input: Vec<Value>,
    custom_tool_call_ids: HashSet<String>,
    /// Hash of the ambient system section (deferred-tool manifest) currently
    /// persisted in `input`. The manifest is injected as a buffer-persisted
    /// `developer` item once and re-injected only when its content changes
    /// (codex idiom: persistent developer context, not per-request ephemera) —
    /// re-delivering it every request made the model treat the catalog as a
    /// fresh event each turn (thread-9dfe1da5). Reset on compaction so the
    /// rebuilt buffer regains the manifest.
    pub ambient_hash: Option<u64>,
    /// Server retry advice (`Retry-After`) carried across request paths: a
    /// rejected WS handshake, WS retries, the WS→HTTP fallback, HTTP retries
    /// and in-band stream failures all hand their deadline here so the next
    /// request waits out the server's window. Deadline-based, so advice goes
    /// inert the moment it passes; it never extends any retry budget. Not
    /// persisted: snapshots survive resume, half-waited rate limits do not.
    pending_retry_after: Option<super::http::RetryAfter>,
    /// Length of the leading provider-canonical protected prefix of `input`:
    /// a window returned by the public `/responses/compact` endpoint, which
    /// must survive normalization (and resume) byte-for-byte. Normalization
    /// never drops, rewrites or pads items inside the prefix, even where the
    /// local pairing rules would (a retained output whose call is now covered
    /// by the encrypted summary is structurally valid). Locally rebuilt
    /// histories (inline summarizer, v2 retention) keep this at zero.
    pub protected_prefix: usize,
    /// Usage returned by remote compaction streams. Compaction spends real
    /// tokens whether or not its envelope validates, so the transport
    /// accumulates it on every observed terminal and the loop drains it via
    /// [`Transport::take_compaction_usage`] after `compact()` returns.
    compaction_usage: super::Usage,
    /// Responses Lite capability for the active model (catalog
    /// `use_responses_lite`). Switching it never rewrites authoritative
    /// history: the normal wire filters Lite-only items per request.
    pub responses_lite: bool,
    /// Incremental tool-catalog baseline. `None` is absent (next sync
    /// renders the full catalog); a restored baseline survives only while
    /// its item chain stays retained. Compaction resets it via
    /// [`Self::reset_lite_baseline`].
    lite_baseline: Option<super::responses_lite::CatalogBaseline>,
}

impl ResponsesState {
    pub(super) fn new(auth: Auth) -> Self {
        Self {
            auth,
            session_id: String::new(),
            thread_id: new_id(),
            input: Vec::new(),
            custom_tool_call_ids: HashSet::new(),
            ambient_hash: None,
            protected_prefix: 0,
            pending_retry_after: None,
            compaction_usage: super::Usage::default(),
            responses_lite: false,
            lite_baseline: None,
        }
    }

    /// Remember server retry advice. An expired pending deadline never masks
    /// fresh advice (the server has moved on); among live deadlines the later
    /// one wins, so no request can start before either window allows.
    /// `None` (no advice) never clobbers a pending deadline.
    pub(super) fn defer_retry_until(&mut self, advice: Option<super::http::RetryAfter>) {
        let Some(advice) = advice else { return };
        self.pending_retry_after = Some(match self.pending_retry_after {
            Some(pending)
                if !pending.remaining_delay().is_zero()
                    && pending.deadline() > advice.deadline() =>
            {
                pending
            }
            _ => advice,
        });
    }

    /// The pending advice without consuming it, for callers that wait out the
    /// window first (cancellation-safe: a wait dropped mid-sleep leaves the
    /// deadline pending) and for assertions.
    pub(super) fn pending_retry_advice(&self) -> Option<super::http::RetryAfter> {
        self.pending_retry_after
    }

    /// Remove the pending advice only once its window has fully elapsed, so a
    /// cancelled wait never lets the next attempt bypass it.
    pub(super) fn take_elapsed_retry_advice(&mut self) -> Option<super::http::RetryAfter> {
        if self
            .pending_retry_after
            .is_some_and(|advice| advice.remaining_delay().is_zero())
        {
            self.pending_retry_after.take()
        } else {
            None
        }
    }

    /// Record usage observed on a remote compaction stream.
    pub(super) fn add_compaction_usage(&mut self, usage: &super::Usage) {
        self.compaction_usage.add(usage);
    }

    /// Drain accumulated remote-compaction usage (resets the accumulator).
    pub(super) fn take_compaction_usage(&mut self) -> super::Usage {
        std::mem::take(&mut self.compaction_usage)
    }

    /// Enable or disable the Responses Lite request shape for this session.
    /// Toggling never mutates authoritative history: Lite-only items stay in
    /// the buffer and the ordinary wire filters them per request.
    pub(super) fn configure_responses_lite(&mut self, enabled: bool) {
        self.responses_lite = enabled;
    }

    /// Whether the active session speaks the Responses Lite shape (WS code
    /// gates its Lite header and interrupt protocol on this).
    pub(super) fn uses_responses_lite(&self) -> bool {
        self.responses_lite
    }

    /// Drop the incremental catalog baseline so the next sync re-renders the
    /// full catalog. Compaction (which rebuilds history) must reset it.
    pub(super) fn reset_lite_baseline(&mut self) {
        self.lite_baseline = None;
    }

    /// Sync the Lite tool catalog into authoritative history and return the
    /// newly added estimate. The initial full render enters as a stable
    /// prefix before the first user item (never inside the protected
    /// prefix); later deltas append here, at the sampling boundary. The
    /// baseline is committed in the same step as the insertion: an uncoupled
    /// or cap-length chain re-renders the full catalog first.
    pub(super) fn sync_lite_catalog(&mut self, tools: &[ToolSpec], opts: &TurnOpts) -> Result<u64> {
        let declarations = lite_tool_declarations(tools, opts);
        let catalog = super::responses_lite::LiteToolCatalog::new(&declarations)?;
        // Retained catalog history (old definitions, deltas, or notices)
        // decides the recovery shape: stale items later in the buffer would
        // semantically override a fresh prefix, so recovery must have the
        // last word at the sampling boundary instead.
        let retained_catalog_history = self
            .input
            .iter()
            .any(|item| item["type"].as_str() == Some("additional_tools"));
        let (transition, first_catalog) = match &self.lite_baseline {
            Some(baseline)
                if baseline.chain_len() < super::responses_lite::MAX_CHAIN_LINKS
                    && baseline.is_coupled_to(&self.input) =>
            {
                (
                    catalog.render_diff(
                        super::responses_lite::PreviousCatalogState::Known(baseline),
                        &self.session_id,
                    ),
                    false,
                )
            }
            // Recovery: the baseline is unusable but stale catalog items are
            // still retained (lost baseline, compaction that kept old
            // definitions, or a chain at the cap). The current catalog, even
            // empty, is re-stated authoritatively at the end.
            Some(_) | None if retained_catalog_history => {
                (catalog.render_recovery(&self.session_id), false)
            }
            // Unusable baseline over clean history: a genuinely fresh catalog
            // enters as the stable prefix.
            Some(_) => (
                catalog.render_diff(
                    super::responses_lite::PreviousCatalogState::Unknown,
                    &self.session_id,
                ),
                true,
            ),
            None => (
                catalog.render_diff(
                    super::responses_lite::PreviousCatalogState::Absent,
                    &self.session_id,
                ),
                true,
            ),
        };
        let mut added_tokens = 0u64;
        for item in &transition.items {
            added_tokens = added_tokens.saturating_add(crate::context::budget::item_tokens(item));
        }
        if first_catalog && !transition.items.is_empty() {
            // Stable prefix: before the first user item, and never inside the
            // provider-canonical protected prefix.
            let position = self
                .input
                .iter()
                .position(|item| {
                    item["type"].as_str().is_none_or(|kind| kind == "message")
                        && item["role"].as_str() == Some("user")
                })
                .unwrap_or(self.input.len())
                .max(self.protected_prefix);
            for (offset, item) in transition.items.into_iter().enumerate() {
                self.input.insert(position + offset, item);
            }
        } else {
            self.input.extend(transition.items);
        }
        self.lite_baseline = Some(transition.baseline);
        Ok(added_tokens)
    }

    /// Build a compaction request against a throwaway copy of this state:
    /// the copy syncs its Lite catalog and builds the body, while the
    /// authoritative buffer and baseline change only on success (the caller
    /// then resets the baseline and re-syncs against the rebuilt history).
    pub(super) fn preview_lite_body(
        &self,
        tools: &[ToolSpec],
        opts: &TurnOpts,
        enabled: bool,
    ) -> Result<Value> {
        let mut preview = self.clone();
        preview.responses_lite = enabled;
        if enabled {
            preview.sync_lite_catalog(tools, opts)?;
        }
        Ok(preview.build_body(tools, opts))
    }

    /// Persist the ambient section into the buffer when it changed since the
    /// last injection. Call once per turn before building the request body.
    pub(super) fn sync_ambient(&mut self, ambient: Option<&str>) {
        let Some(text) = ambient.filter(|s| !s.is_empty()) else {
            return;
        };
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let hash = h.finish();
        if self.ambient_hash == Some(hash) {
            return;
        }
        self.input.push(json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": text}],
        }));
        self.ambient_hash = Some(hash);
    }

    pub(super) fn push_user_text(&mut self, text: &str) {
        // A new user turn begins: rotate the per-turn `thread-id` (codex mints a
        // fresh ThreadId per turn while `session-id` stays stable).
        self.thread_id = new_id();
        self.input.push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        }));
    }

    pub(super) fn push_user_text_blocks(&mut self, blocks: Vec<String>) {
        self.thread_id = new_id();
        let content: Vec<Value> = blocks
            .into_iter()
            .map(|text| json!({"type": "input_text", "text": text}))
            .collect();
        self.input.push(json!({
            "type": "message",
            "role": "user",
            "content": content,
        }));
    }

    pub(super) fn push_tool_results(&mut self, results: Vec<ToolResult>) {
        for r in results {
            if self.custom_tool_call_ids.remove(&r.id) {
                self.input.push(json!({
                    "type": "custom_tool_call_output",
                    "call_id": r.id,
                    "output": r.content,
                }));
            } else {
                self.input.push(json!({
                    "type": "function_call_output",
                    "call_id": r.id,
                    "output": r.content,
                }));
            }
        }
    }

    pub(super) fn snapshot(&self) -> Value {
        // v2 shape carries the ambient hash so resume doesn't re-inject an
        // unchanged manifest, and the protected-prefix length so a
        // provider-canonical compaction window survives resume verbatim.
        // restore() still accepts the legacy bare array.
        json!({
            "input": self.input,
            "ambient_hash": self.ambient_hash,
            "protected_prefix": self.protected_prefix,
            "responses_lite": self.responses_lite,
            "lite_tools": self.lite_baseline.as_ref().map(|baseline| baseline.to_side()),
        })
    }

    pub(super) fn restore(&mut self, snapshot: Value) {
        if let Some(arr) = snapshot.as_array() {
            // Legacy snapshot (bare input array): no recorded hash — the next
            // sync_ambient re-injects once, which is safe. Nothing is
            // protected: the history was normalized by an older build.
            self.input = arr.clone();
            self.custom_tool_call_ids = pending_custom_tool_call_ids(&self.input);
            self.ambient_hash = None;
            self.protected_prefix = 0;
            self.responses_lite = false;
            self.lite_baseline = None;
        } else if let Some(arr) = snapshot.get("input").and_then(Value::as_array) {
            self.input = arr.clone();
            self.custom_tool_call_ids = pending_custom_tool_call_ids(&self.input);
            self.ambient_hash = snapshot.get("ambient_hash").and_then(Value::as_u64);
            self.responses_lite = snapshot
                .get("responses_lite")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.lite_baseline = match super::responses_lite::CatalogBaseline::from_side(
                snapshot.get("lite_tools").unwrap_or(&Value::Null),
            ) {
                // A restored baseline is trusted only while its whole item
                // chain survived into the restored history; otherwise the
                // next sync re-renders the full catalog.
                super::responses_lite::BaselineRestore::Known(baseline)
                    if baseline.is_coupled_to(&self.input) =>
                {
                    Some(baseline)
                }
                _ => None,
            };
            // Defensive clamp: checkpoint validation rejects an over-long
            // prefix, so this only guards an unvalidated restore from
            // arithmetic surprises, never from data loss.
            self.protected_prefix = snapshot
                .get("protected_prefix")
                .and_then(Value::as_u64)
                .map_or(0, |len| (len as usize).min(self.input.len()));
        }
    }

    pub(super) fn normalize_for_prompt(&mut self) {
        normalize_responses_suffix(&mut self.input, self.protected_prefix);
        self.custom_tool_call_ids = pending_custom_tool_call_ids(&self.input);
    }

    pub(super) fn build_body(&self, tools: &[ToolSpec], opts: &TurnOpts) -> Value {
        if self.responses_lite {
            build_lite_body(&self.input, &self.session_id, opts)
        } else {
            // Lite-only items stay in the authoritative buffer across mode
            // toggles; the ordinary wire never carries them.
            let input = self
                .input
                .iter()
                .filter(|item| item["type"].as_str() != Some("additional_tools"))
                .cloned()
                .collect::<Vec<_>>();
            build_body(&input, &self.session_id, tools, opts)
        }
    }

    pub(super) fn parse_sse(&mut self, sse: &str) -> Result<TurnOutput> {
        parse_sse(&mut self.input, &mut self.custom_tool_call_ids, sse)
    }

    pub(super) fn identity_auth_headers(&self) -> Vec<(&'static str, String)> {
        identity_auth_headers(&self.session_id, &self.thread_id, &self.auth)
    }
}

/// Resolve auth from env: an explicit `OPENAI_API_KEY` selects the standard
/// OpenAI path; otherwise fall back to the Codex ChatGPT OAuth in
/// `~/.codex/auth.json` (loading + refreshing cooperatively with the Codex CLI).
pub(super) async fn resolve_auth(http: &reqwest::Client) -> Result<Auth> {
    if let Some(key) = super::session_var("OPENAI_API_KEY") {
        return Ok(Auth::ApiKey(key));
    }
    let auth = super::codex_auth::load_fresh(http)
        .await
        .context("no OPENAI_API_KEY and could not load/refresh Codex ChatGPT auth")?;
    Ok(Auth::ChatGpt {
        access_token: auth.access_token,
        account_id: auth.account_id,
    })
}

/// The HTTP `/responses` endpoint for the resolved auth: `{OPENAI_BASE_URL}/responses`
/// for an API key, or the ChatGPT backend (`OPENAI_RESPONSES_URL`) for OAuth.
pub(super) fn http_endpoint(auth: &Auth) -> String {
    match auth {
        Auth::ApiKey(_) => {
            let base = super::session_var("OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
                .trim_end_matches('/')
                .to_string();
            format!("{base}/responses")
        }
        Auth::ChatGpt { .. } => super::session_var("OPENAI_RESPONSES_URL")
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex/responses".to_string()),
    }
}

/// The WebSocket `/responses` URL — the HTTP endpoint with the scheme swapped to
/// `wss`/`ws` (codex's `websocket_url_for_path`).
pub(super) fn ws_endpoint(auth: &Auth) -> String {
    let http = http_endpoint(auth);
    if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http
    }
}

/// The codex-style identity + auth headers shared by every Responses request —
/// HTTP body POST and WS handshake alike. `session-id` is the stable per-session
/// id; `thread-id` is the current turn. `OpenAI-Beta: responses=experimental` is
/// intentionally absent (defunct in codex `main`). Returns `(name, value)` pairs
/// so each transport can apply them to its own request type; transport-specific
/// headers (content-type/accept on HTTP, the websockets beta on the WS
/// handshake) are added by the caller.
pub(super) fn identity_auth_headers(
    session_id: &str,
    thread_id: &str,
    auth: &Auth,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("originator", originator()),
        ("user-agent", user_agent()),
        ("thread-id", thread_id.to_string()),
    ];
    if !session_id.is_empty() {
        headers.push(("session-id", session_id.to_string()));
    }
    match auth {
        Auth::ApiKey(k) => headers.push(("authorization", format!("Bearer {k}"))),
        Auth::ChatGpt {
            access_token,
            account_id,
        } => {
            headers.push(("authorization", format!("Bearer {access_token}")));
            headers.push(("chatgpt-account-id", account_id.clone()));
        }
    }
    headers
}

/// Build the Responses request body (pure; no I/O). The base instructions plus
/// stable overlay go in `instructions`; the volatile `developer` item is never
/// persisted into the buffer. `input` is the conversation buffer; `session_id`
/// keys the prompt cache.
pub(super) fn build_body(
    input: &[Value],
    session_id: &str,
    tools: &[ToolSpec],
    opts: &TurnOpts,
) -> Value {
    let mut tool_defs: Vec<Value> = tools.iter().map(responses_tool_definition).collect();
    if opts.web_search {
        tool_defs.push(json!({"type": "web_search"}));
    }

    // The base prompt and cache-stable overlay go in `instructions` (cached via
    // prompt_cache_key). The ambient manifest is buffer-persisted by
    // `sync_ambient` (hash-gated on change), so the only per-request ephemera
    // left is the volatile tail (nudges / structured-output reminders),
    // appended as a trailing `developer` item that never persists into the
    // buffer. On most turns volatile is empty and no ephemeral item is sent.
    let mut input = input.to_vec();
    if let Some(volatile) = opts.system.volatile_text() {
        input.push(json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": volatile}],
        }));
    }
    let mut body = json!({
        "model": opts.model,
        "input": input,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "stream": true,
        "store": false,
    });
    // The ChatGPT backend rejects an empty/missing `instructions` field
    // ("Instructions are required"); always send a non-empty value.
    let instructions = response_instructions(opts);
    body["instructions"] = json!(instructions);
    if !tool_defs.is_empty() {
        body["tools"] = json!(tool_defs);
    }
    // Stable cache key (codex uses the thread id): keeps the cached prefix
    // pinned to this session instead of relying on implicit server keying.
    if !session_id.is_empty() {
        body["prompt_cache_key"] = json!(session_id);
    }
    // `/fast` lever: forward the priority/flex tier when set (and not the
    // standard-routing sentinel, which the backend rejects as a no-op).
    if let Some(tier) = service_tier_for_request(opts.service_tier.as_deref()) {
        body["service_tier"] = json!(tier);
    }
    // Stateless replay needs encrypted reasoning even when the provider chooses
    // the effort default. Explicit effort controls generation, not continuity.
    if model_supports_reasoning(&opts.model) {
        body["include"] = json!(["reasoning.encrypted_content"]);
        if let Some(e) = &opts.effort {
            let mut reasoning = json!({ "effort": normalize_effort(e) });
            if let Some(summary) = reasoning_summary() {
                reasoning["summary"] = json!(summary);
            }
            body["reasoning"] = reasoning;
        }
    } else if opts.effort.is_some() {
        tracing::warn!(
            model = %opts.model,
            "effort requested but model is not reasoning-capable; omitting reasoning"
        );
    }
    body
}

fn responses_tool_definition(t: &ToolSpec) -> Value {
    if let Some(grammar) = &t.grammar {
        json!({
            "type": "custom",
            "name": t.name,
            "description": t.description,
            "format": {
                "type": "grammar",
                "syntax": grammar.syntax,
                "definition": grammar.definition,
            },
        })
    } else {
        json!({
            "type": "function",
            "name": t.name,
            "description": t.description,
            "parameters": t.schema,
            "strict": false,
        })
    }
}

/// The wire-schema authority's rendered Lite declarations: the same flat
/// function/custom definitions the ordinary Responses `tools` parameter
/// would carry, plus the `web_search` builtin when enabled. Namespaces are
/// supported by the diff helper; the harness catalog renders flat today.
fn lite_tool_declarations(tools: &[ToolSpec], opts: &TurnOpts) -> Vec<Value> {
    let mut declarations: Vec<Value> = tools.iter().map(responses_tool_definition).collect();
    if opts.web_search {
        declarations.push(json!({"type": "web_search"}));
    }
    declarations
}

/// Build the Responses Lite request body (pure; no I/O). Mirrors codex's
/// `build_responses_request` Lite arm: no `tools` parameter (definitions
/// travel as history-carried `additional_tools` items), an empty
/// `instructions` field with the base+stable prompt re-entering as a
/// request-only developer message carrying a stable uuid v5 id bound to the
/// session and its canonical payload, `reasoning.context` `all_turns`, and
/// `parallel_tool_calls` false. The message is never persisted into the
/// buffer, so retries and resumed sessions reproduce it verbatim.
pub(super) fn build_lite_body(input: &[Value], session_id: &str, opts: &TurnOpts) -> Value {
    let mut input = input.to_vec();
    let instructions = response_instructions(opts);
    let session_namespace = Uuid::new_v5(&Uuid::NAMESPACE_OID, session_id.as_bytes());
    let id = format!(
        "msg_{}",
        Uuid::new_v5(&session_namespace, instructions.as_bytes())
    );
    input.insert(
        0,
        json!({
            "type": "message",
            "id": id,
            "role": "developer",
            "content": [{"type": "input_text", "text": instructions}],
        }),
    );
    if let Some(volatile) = opts.system.volatile_text() {
        input.push(json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": volatile}],
        }));
    }
    let mut body = json!({
        "model": opts.model,
        "input": input,
        "instructions": "",
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "stream": true,
        "store": false,
    });
    // The `tools` parameter is intentionally omitted: definitions ride in
    // history as `additional_tools` developer items.
    if !session_id.is_empty() {
        body["prompt_cache_key"] = json!(session_id);
    }
    if let Some(tier) = service_tier_for_request(opts.service_tier.as_deref()) {
        body["service_tier"] = json!(tier);
    }
    if model_supports_reasoning(&opts.model) {
        body["include"] = json!(["reasoning.encrypted_content"]);
        // Lite keeps reasoning continuity across turns; effort and summary
        // join when requested.
        let mut reasoning = json!({"context": "all_turns"});
        if let Some(effort) = &opts.effort {
            reasoning["effort"] = json!(normalize_effort(effort));
            if let Some(summary) = reasoning_summary() {
                reasoning["summary"] = json!(summary);
            }
        }
        body["reasoning"] = reasoning;
    } else if opts.effort.is_some() {
        tracing::warn!(
            model = %opts.model,
            "effort requested but model is not reasoning-capable; omitting reasoning"
        );
    }
    body
}

fn response_instructions(opts: &TurnOpts) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(base) = opts
        .base_instructions
        .as_ref()
        .and_then(super::BaseInstructions::text)
    {
        parts.push(base);
    }
    if let Some(stable) = opts.system.stable_text() {
        parts.push(stable);
    }
    if parts.is_empty() {
        "You are a helpful coding assistant operating non-interactively.".to_string()
    } else {
        parts.join("\n\n")
    }
}

/// Parse the SSE body: accumulate completed output items, append them to the
/// `input` buffer (so the next turn carries context), and normalize. Shared by
/// every Responses transport — the downstream event vocabulary is identical
/// across HTTP-SSE and WebSocket.
///
/// Error wrapping follows the loop's recovery contract: a *pure* context-window
/// rejection (the failure event arrived before any output, tool or native
/// effect was observed) keeps only the typed [`super::ContextWindowExceeded`]
/// cause, so the loop's compact-and-retry guard can recover the turn. Any
/// other failure, including a context-window rejection after observed output,
/// is wrapped as a [`super::FailedTurnObservation`] so the guard refuses a
/// replay that could duplicate already-emitted effects.
pub(super) fn parse_sse(
    input: &mut Vec<Value>,
    custom_tool_call_ids: &mut HashSet<String>,
    sse: &str,
) -> Result<TurnOutput> {
    parse_sse_validated(input, custom_tool_call_ids, sse).map_err(|error| {
        if error
            .downcast_ref::<super::FailedTurnObservation>()
            .is_some()
        {
            // Already carries the durable observation evidence.
            error
        } else if error
            .chain()
            .any(|cause| cause.is::<super::ContextWindowExceeded>())
        {
            // Pure rejection: typed cause only, preserved for recovery.
            error
        } else {
            responses_failure(error, sse)
        }
    })
}

/// Output items of one completed Responses stream, without touching history.
/// Server-side compaction uses this: its only output is the encrypted
/// compaction item, which the caller splices into a rebuilt history itself.
/// A failed, incomplete, or unterminated stream is an error.
pub(super) fn parse_sse_output_items(sse: &str) -> Result<Vec<Value>> {
    let mut output_items: Vec<Value> = Vec::new();
    let mut final_output = None;
    let mut terminal = false;
    for line in sse.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let ev: Value = serde_json::from_str(data).context("invalid Responses SSE JSON")?;
        anyhow::ensure!(!terminal, "Responses event after terminal response");
        match ev["type"].as_str().unwrap_or("") {
            "response.output_item.done" => {
                let item = ev
                    .get("item")
                    .context("completed output item missing payload")?;
                anyhow::ensure!(
                    item.is_object() && item["type"].as_str().is_some_and(|kind| !kind.is_empty()),
                    "invalid Responses output item"
                );
                output_items.push(item.clone());
            }
            "response.completed" | "response.incomplete" => {
                terminal = true;
                let r = &ev["response"];
                anyhow::ensure!(r.is_object(), "Responses terminal missing response object");
                anyhow::ensure!(
                    ev["type"] == "response.completed"
                        && r["status"]
                            .as_str()
                            .is_none_or(|status| status == "completed"),
                    "Responses stream did not complete"
                );
                if let Some(output) = r.get("output").and_then(Value::as_array)
                    && !output.is_empty()
                {
                    final_output = Some(output.clone());
                }
            }
            "response.failed" | "error" => {
                let err = if ev["type"] == "response.failed" {
                    &ev["response"]["error"]
                } else {
                    &ev["error"]
                };
                let code = err["code"]
                    .as_str()
                    .or_else(|| ev["code"].as_str())
                    .unwrap_or("");
                let message = err["message"]
                    .as_str()
                    .or_else(|| ev["message"].as_str())
                    .unwrap_or(data);
                if matches!(code, "context_length_exceeded" | "context_window_exceeded") {
                    return Err(anyhow::Error::new(super::ContextWindowExceeded(
                        classify_stream_error(code, message),
                    )));
                }
                anyhow::bail!(classify_stream_error(code, message));
            }
            _ => {}
        }
    }
    anyhow::ensure!(terminal, "Responses stream closed before terminal response");
    Ok(final_output.unwrap_or(output_items))
}

pub(super) fn responses_failure(error: anyhow::Error, sse: &str) -> anyhow::Error {
    super::rejected_provider_response(error, "responses", json!({"sse": sse}))
}

fn validate_completed_item(added: &Value, completed: &Value) -> Result<()> {
    for field in ["type", "id", "call_id", "name"] {
        if let Some(value) = added[field].as_str().filter(|value| !value.is_empty()) {
            anyhow::ensure!(
                completed[field].as_str() == Some(value),
                "Responses output item identity changed during stream"
            );
        }
    }
    Ok(())
}

fn parse_sse_validated(
    input: &mut Vec<Value>,
    custom_tool_call_ids: &mut HashSet<String>,
    sse: &str,
) -> Result<TurnOutput> {
    let mut output_items: Vec<Value> = Vec::new();
    let mut usage = Usage::default();
    let mut stop = StopReason::Done;
    let mut end_turn = None;
    let mut terminal = false;
    let mut open_items = std::collections::HashMap::new();
    let mut final_output = None;
    let mut observed_call_items = HashSet::new();
    // Provider work observed before any failure: output items, tool argument
    // streams, native calls. A rejection that arrives before any of these is
    // pure: nothing was emitted, so the loop may recover it.
    let mut observed_effects = false;

    for line in sse.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let ev: Value = serde_json::from_str(data).context("invalid Responses SSE JSON")?;
        anyhow::ensure!(!terminal, "Responses event after terminal response");
        let kind = ev["type"].as_str().unwrap_or("");
        observed_effects |= observes_provider_effects(kind);
        match kind {
            "response.output_item.added" => {
                let index = ev["output_index"]
                    .as_u64()
                    .context("output item missing index")?;
                let item = ev
                    .get("item")
                    .context("added output item missing payload")?;
                anyhow::ensure!(
                    open_items.insert(index, item.clone()).is_none(),
                    "duplicate open output item"
                );
            }
            "response.output_item.done" => {
                let item = ev
                    .get("item")
                    .context("completed output item missing payload")?;
                if let Some(index) = ev["output_index"].as_u64()
                    && let Some(added) = open_items.remove(&index)
                {
                    validate_completed_item(&added, item)?;
                }
                output_items.push(item.clone());
            }
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done" => {
                let id = ev["item_id"]
                    .as_str()
                    .context("tool argument event missing item_id")?;
                observed_call_items.insert(id.to_string());
            }
            "response.completed" | "response.incomplete" => {
                terminal = true;
                let r = &ev["response"];
                anyhow::ensure!(r.is_object(), "Responses terminal missing response object");
                let incomplete = ev["type"] == "response.incomplete";
                if let Some(status) = r["status"].as_str() {
                    anyhow::ensure!(
                        status
                            == if incomplete {
                                "incomplete"
                            } else {
                                "completed"
                            },
                        "inconsistent Responses terminal status"
                    );
                }
                if let Some(output) = r.get("output") {
                    final_output = Some(
                        output
                            .as_array()
                            .context("terminal output must be an array")?
                            .clone(),
                    );
                }
                end_turn = r["end_turn"].as_bool();
                // OpenAI Responses `input_tokens` is cache-INCLUSIVE; the
                // cached subset lives in `input_tokens_details.cached_tokens`.
                // Subtract it so `input_tokens` stays fresh.
                let total_input = r["usage"]["input_tokens"].as_u64().unwrap_or(0);
                let cached = r["usage"]["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage = Usage {
                    input_tokens: total_input.saturating_sub(cached),
                    output_tokens: r["usage"]["output_tokens"].as_u64().unwrap_or(0),
                    cached_input_tokens: cached,
                    cache_creation_input_tokens: 0,
                };
                if incomplete {
                    stop = StopReason::Length;
                    // Otherwise-silent path: the model stopped short (e.g.
                    // max_output_tokens, content filter). Surface the reason
                    // so a spurious-stop turn is diagnosable from the log.
                    tracing::warn!(
                        reason = %r["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("unknown"),
                        "responses turn incomplete; stopping short"
                    );
                }
            }
            "response.failed" | "error" => {
                let err = if ev["type"] == "response.failed" {
                    &ev["response"]["error"]
                } else {
                    &ev["error"]
                };
                let code = err["code"]
                    .as_str()
                    .or_else(|| ev["code"].as_str())
                    .unwrap_or("");
                let message = err["message"]
                    .as_str()
                    .or_else(|| ev["message"].as_str())
                    .unwrap_or(data);
                // A context-window rejection is recoverable when nothing was
                // observed first: surface a typed cause so the agent loop can
                // compact + retry rather than fail the turn. The same code
                // after observed output stays terminal, because a replay
                // could duplicate the emitted effects, so the typed cause is
                // wrapped with the durable observation and the loop guard
                // refuses the retry. Flows through both the HTTP path and the
                // WS path (WsOutcome::Api), since both classify via this parser.
                if is_context_window_code(code) {
                    let typed = anyhow::Error::new(super::ContextWindowExceeded(
                        classify_stream_error(code, message),
                    ));
                    if observed_effects {
                        return Err(responses_failure(typed, sse));
                    }
                    return Err(typed);
                }
                anyhow::bail!(classify_stream_error(code, message));
            }
            _ => {}
        }
    }

    anyhow::ensure!(terminal, "Responses stream closed before terminal response");
    // The ChatGPT Responses stream can finish with output:[] after publishing
    // every item through output_item.done. That terminal is metadata-only, as
    // in Codex's OutputItemDone + Completed event contract. It cannot close an
    // unfinished item. A nonempty terminal snapshot still reconciles strictly.
    if let Some(output) = final_output.filter(|output| !output.is_empty()) {
        for (index, added) in &open_items {
            let completed = usize::try_from(*index)
                .ok()
                .and_then(|index| output.get(index))
                .context("terminal output omitted an unfinished item")?;
            validate_completed_item(added, completed)?;
        }
        for done in &output_items {
            anyhow::ensure!(
                output.contains(done),
                "terminal output disagrees with completed item"
            );
        }
        output_items = output;
    } else {
        anyhow::ensure!(
            open_items.is_empty(),
            "Responses stream contains unfinished output items"
        );
    }

    for id in observed_call_items {
        anyhow::ensure!(
            output_items
                .iter()
                .any(|item| item["id"].as_str() == Some(&id)),
            "Responses argument stream has no completed tool item"
        );
    }

    // Echo the model's output items back into the buffer for continuity.
    // Reasoning items need care under `store:false` (required by the ChatGPT
    // backend): a reasoning item replayed *by reference* (`rs_…` with no
    // payload) 404s ("Item with id … not found") because it isn't persisted
    // server-side. But because we request `include:["reasoning.encrypted_content"]`,
    // reasoning items come back carrying `encrypted_content` — self-contained
    // and safe to replay, preserving cross-turn reasoning continuity. So:
    // keep reasoning items that carry `encrypted_content`; drop the rest;
    // keep every non-reasoning item.

    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    let mut call_ids = HashSet::new();
    let mut new_custom_ids = Vec::new();
    for item in &output_items {
        anyhow::ensure!(
            item.is_object() && item["type"].as_str().is_some_and(|kind| !kind.is_empty()),
            "invalid Responses output item"
        );
        anyhow::ensure!(
            stop == StopReason::Length
                || item["status"]
                    .as_str()
                    .is_none_or(|status| status == "completed"),
            "unfinished Responses output item"
        );
        match item["type"].as_str().unwrap_or("") {
            "message" => {
                if let Some(parts) = item["content"].as_array() {
                    for p in parts {
                        if p["type"] == "output_text"
                            && let Some(t) = p["text"].as_str()
                        {
                            text.push_str(t);
                        }
                    }
                }
            }
            // Reasoning items carry summary/content text — surface for
            // display only (not replayed; `store:false` drops them server-side).
            "reasoning" => {
                for key in ["summary", "content"] {
                    if let Some(parts) = item[key].as_array() {
                        for p in parts {
                            if let Some(t) = p["text"].as_str() {
                                thinking.push_str(t);
                            }
                        }
                    }
                }
            }
            "function_call" | "custom_tool_call" => {
                anyhow::ensure!(
                    stop != StopReason::Length,
                    "incomplete response contains client tool calls"
                );
                let call_id = item["call_id"]
                    .as_str()
                    .context("tool call missing call_id")?;
                let name = item["name"].as_str().context("tool call missing name")?;
                super::validate_tool_identity(call_id, name)?;
                anyhow::ensure!(call_ids.insert(call_id), "duplicate tool call id");
                let args = if item["type"] == "function_call" {
                    super::parse_tool_arguments(
                        item["arguments"]
                            .as_str()
                            .context("tool arguments must be a JSON string")?,
                    )?
                } else {
                    let source = item["input"]
                        .as_str()
                        .context("custom tool input must be a string")?;
                    new_custom_ids.push(call_id.to_string());
                    json!({"source": source})
                };
                tool_calls.push(ToolCall {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    args,
                });
            }
            _ => {} // reasoning / other items carried in buffer, not surfaced
        }
    }

    if !tool_calls.is_empty() {
        stop = StopReason::ToolCalls;
    }

    input.extend(
        output_items
            .iter()
            .filter(|item| {
                item["type"].as_str() != Some("reasoning")
                    || item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .is_some()
            })
            .cloned(),
    );
    custom_tool_call_ids.extend(new_custom_ids);

    Ok(TurnOutput {
        observation_content: None,
        text,
        thinking,
        tool_calls,
        stop,
        end_turn,
        usage,
    })
}

/// Largest split `≤ limit` whose kept tail `[split..]` is a valid standalone
/// Responses input — no `function_call_output` orphaned from the
/// `function_call` (matched by `call_id`) it answers. `None` if none exists.
pub(super) fn responses_split(input: &[Value], limit: usize) -> Option<usize> {
    (1..limit).rev().find(|&s| {
        let tail = &input[s..];
        let function_calls: HashSet<&str> = tail
            .iter()
            .filter(|it| it["type"] == "function_call")
            .filter_map(|it| it["call_id"].as_str())
            .collect();
        let custom_calls: HashSet<&str> = tail
            .iter()
            .filter(|it| it["type"] == "custom_tool_call")
            .filter_map(|it| it["call_id"].as_str())
            .collect();
        !tail.iter().any(|it| {
            (it["type"] == "function_call_output"
                && it["call_id"]
                    .as_str()
                    .is_some_and(|c| !function_calls.contains(c)))
                || (it["type"] == "custom_tool_call_output"
                    && it["call_id"]
                        .as_str()
                        .is_some_and(|c| !custom_calls.contains(c)))
        })
    })
}

pub(super) fn normalize_responses_input(input: &mut Vec<Value>) {
    normalize_responses_suffix(input, 0);
}

/// Repair the replay buffer's tool-call pairing, touching only
/// `input[protected..]`. Items inside the protected prefix are
/// provider-canonical (a public `/responses/compact` window) and pass through
/// untouched: a retained output whose call is now covered by the encrypted
/// summary stays, and a retained call without an output is never padded with
/// an invented "aborted" result. Call-id pairing is resolved across the whole
/// buffer so a prefix/suffix boundary never orphans a pair that straddles it.
pub(super) fn normalize_responses_suffix(input: &mut Vec<Value>, protected: usize) {
    let protected = protected.min(input.len());
    let function_calls: HashSet<String> = input
        .iter()
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| item["call_id"].as_str().map(str::to_string))
        .collect();
    let custom_calls: HashSet<String> = input
        .iter()
        .filter(|item| item["type"] == "custom_tool_call")
        .filter_map(|item| item["call_id"].as_str().map(str::to_string))
        .collect();

    let mut index = 0usize;
    input.retain(|item| {
        let is_protected = index < protected;
        index += 1;
        if is_protected {
            return true;
        }
        match item["type"].as_str() {
            Some("function_call_output") => item["call_id"]
                .as_str()
                .is_some_and(|id| function_calls.contains(id)),
            Some("custom_tool_call_output") => item["call_id"]
                .as_str()
                .is_some_and(|id| custom_calls.contains(id)),
            _ => true,
        }
    });

    let function_outputs: HashSet<String> = input
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .filter_map(|item| item["call_id"].as_str().map(str::to_string))
        .collect();
    let custom_outputs: HashSet<String> = input
        .iter()
        .filter(|item| item["type"] == "custom_tool_call_output")
        .filter_map(|item| item["call_id"].as_str().map(str::to_string))
        .collect();
    let mut missing_outputs = Vec::new();
    for (idx, item) in input.iter().enumerate() {
        if idx < protected {
            continue;
        }
        if item["type"] == "function_call"
            && let Some(call_id) = item["call_id"].as_str()
            && !function_outputs.contains(call_id)
        {
            missing_outputs.push((
                idx,
                json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": "aborted",
                }),
            ));
        }
        if item["type"] == "custom_tool_call"
            && let Some(call_id) = item["call_id"].as_str()
            && !custom_outputs.contains(call_id)
        {
            missing_outputs.push((
                idx,
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": call_id,
                    "output": "aborted",
                }),
            ));
        }
    }
    for (idx, output) in missing_outputs.into_iter().rev() {
        input.insert(idx + 1, output);
    }
}

fn pending_custom_tool_call_ids(input: &[Value]) -> HashSet<String> {
    let outputs: HashSet<String> = input
        .iter()
        .filter(|item| item["type"] == "custom_tool_call_output")
        .filter_map(|item| item["call_id"].as_str().map(str::to_string))
        .collect();
    input
        .iter()
        .filter(|item| item["type"] == "custom_tool_call")
        .filter_map(|item| item["call_id"].as_str())
        .filter(|id| !outputs.contains(*id))
        .map(str::to_string)
        .collect()
}

/// Render a slice of the Responses input buffer to a plain-text transcript for
/// summarization. Tool outputs are truncated to keep the prompt bounded.
pub(super) fn render_responses_transcript(items: &[Value], tool_cap: usize) -> String {
    let mut s = String::new();
    for it in items {
        match it["type"].as_str().unwrap_or("") {
            "message" => {
                let role = it["role"].as_str().unwrap_or("?");
                s.push_str(&format!("\n## {role}\n"));
                if let Some(parts) = it["content"].as_array() {
                    for p in parts {
                        if let Some(t) = p["text"].as_str() {
                            s.push_str(t);
                        }
                    }
                }
                s.push('\n');
            }
            "function_call" => s.push_str(&format!(
                "\n## assistant\n[tool_call {} {}]\n",
                it["name"].as_str().unwrap_or(""),
                it["arguments"].as_str().unwrap_or("")
            )),
            "function_call_output" => s.push_str(&format!(
                "\n## tool\n[tool_result {}]\n",
                super::truncate(it["output"].as_str().unwrap_or(""), tool_cap)
            )),
            "custom_tool_call" => s.push_str(&format!(
                "\n## assistant\n[custom_tool_call {} {}]\n",
                it["name"].as_str().unwrap_or(""),
                it["input"].as_str().unwrap_or("")
            )),
            "custom_tool_call_output" => s.push_str(&format!(
                "\n## tool\n[custom_tool_result {}]\n",
                super::truncate(it["output"].as_str().unwrap_or(""), tool_cap)
            )),
            _ => {}
        }
    }
    s
}

/// Map an effort token onto codex's `ReasoningEffort` range
/// (`none/minimal/low/medium/high/xhigh/max/ultra`). GPT-5.6 introduced real
/// `max` and `ultra` reasoning levels (Sol/Terra expose `ultra`; Luna exposes
/// `max`), so they pass through verbatim rather than collapsing to `high`. The
/// daemon allocator gate ([`Provider::model_efforts`]) ensures `max`/`ultra`
/// only reach models that accept them; an unrecognized token falls back to
/// `medium`.
pub(super) fn normalize_effort(e: &str) -> &'static str {
    match e.trim().to_ascii_lowercase().as_str() {
        "none" => "none",
        "minimal" | "min" => "minimal",
        "low" => "low",
        "medium" | "med" => "medium",
        "high" => "high",
        "xhigh" | "x-high" | "extra-high" => "xhigh",
        "max" => "max",
        "ultra" => "ultra",
        _ => "medium",
    }
}

/// Fresh random id for the `session-id`/`thread-id` headers.
pub(super) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Request originator — codex's first-party value by default so the ChatGPT
/// backend routes/accounts the request as it expects. Overridable to match
/// codex's own `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`, or `BRO_HARNESS_ORIGINATOR`.
pub(super) fn originator() -> String {
    std::env::var("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
        .or_else(|_| std::env::var("BRO_HARNESS_ORIGINATOR"))
        .unwrap_or_else(|_| "codex_cli_rs".to_string())
}

/// Descriptive `User-Agent` in codex's shape (`<originator>/<ver> (<os>; <arch>)`),
/// fully overridable via `BRO_HARNESS_USER_AGENT`.
pub(super) fn user_agent() -> String {
    std::env::var("BRO_HARNESS_USER_AGENT").unwrap_or_else(|_| {
        format!(
            "{}/{} ({}; {})",
            originator(),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    })
}

/// True unless the model is a known non-reasoning family. Codex gates reasoning
/// on a per-model capability flag from its catalog; lacking that catalog we
/// fail open (unknown ⇒ reasoning allowed, since the ChatGPT backend only ever
/// serves reasoning models) but suppress the obvious GPT-3/4 families so an
/// effort value can't 400 a non-reasoning model.
pub(super) fn model_supports_reasoning(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    const NON_REASONING_PREFIXES: &[&str] = &["gpt-4", "gpt-3", "chatgpt-4"];
    !NON_REASONING_PREFIXES.iter().any(|p| m.starts_with(p))
}

/// Reasoning summary mode (codex default `auto`). `BRO_HARNESS_REASONING_SUMMARY`
/// overrides; `none`/`off`/empty omits the field.
pub(super) fn reasoning_summary() -> Option<String> {
    match std::env::var("BRO_HARNESS_REASONING_SUMMARY") {
        Ok(v) if matches!(v.trim().to_ascii_lowercase().as_str(), "none" | "off" | "") => None,
        Ok(v) => Some(v.trim().to_string()),
        Err(_) => Some("auto".to_string()),
    }
}

/// Normalize a requested service tier: forward it unless it's empty or the
/// standard-routing sentinel (which the backend rejects as a no-op). Codex's
/// `service_tier_for_request` does the same drop.
pub(super) fn service_tier_for_request(tier: Option<&str>) -> Option<String> {
    let t = tier?.trim();
    if t.is_empty() || t.eq_ignore_ascii_case(SERVICE_TIER_DEFAULT) {
        return None;
    }
    Some(t.to_string())
}

/// Classify a Responses stream error (`response.failed` / `error`) into a clear,
/// actionable message. Mirrors codex's error-code mapping
/// (`codex-api/src/sse/responses.rs`).
pub(super) fn classify_stream_error(code: &str, message: &str) -> String {
    match code {
        "context_length_exceeded" | "context_window_exceeded" => {
            format!("context window exceeded ({message}); compact the conversation and retry")
        }
        "insufficient_quota" | "usage_not_included" => {
            format!("quota/usage limit [{code}]: {message}")
        }
        "server_is_overloaded" | "slow_down" => format!("server overloaded [{code}]: {message}"),
        "" => format!("responses stream error: {message}"),
        _ => format!("responses stream error [{code}]: {message}"),
    }
}

/// True when the code names a context-window rejection. OpenAI exposes both
/// spellings depending on surface (`context_length_exceeded` on the public
/// API, `context_window_exceeded` in streams).
pub(super) fn is_context_window_code(code: &str) -> bool {
    matches!(code, "context_length_exceeded" | "context_window_exceeded")
}

/// Server retry advice carried inside a streamed failure envelope. Codex's
/// backend repeats the rejection headers in `error.headers`; the value uses
/// the same grammar as the HTTP header (delay seconds or an HTTP-date).
pub(super) fn in_band_retry_after(ev: &Value) -> Option<super::http::RetryAfter> {
    let headers = ev["error"]["headers"]
        .as_object()
        .or_else(|| ev["response"]["error"]["headers"].as_object())?;
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.as_str())?;
    super::http::RetryAfter::from_header(value)
}

/// The machine-readable code of a streamed failure event.
pub(super) fn stream_error_code<'a>(ev: &'a Value) -> &'a str {
    if ev["type"] == "response.failed" {
        &ev["response"]["error"]["code"]
    } else {
        &ev["error"]["code"]
    }
    .as_str()
    .or_else(|| ev["code"].as_str())
    .unwrap_or("")
}

pub(super) fn stream_error_message<'a>(ev: &'a Value, data: &'a str) -> &'a str {
    if ev["type"] == "response.failed" {
        &ev["response"]["error"]["message"]
    } else {
        &ev["error"]["message"]
    }
    .as_str()
    .or_else(|| ev["message"].as_str())
    .unwrap_or(data)
}

/// Stream failure codes that are known-transient and therefore retryable
/// within a bounded budget. The list is an explicit allowlist rather than
/// "everything else": an unrecognized failure code keeps the previous
/// terminal behavior, so an unknown envelope can never buy retries (and a
/// no-code envelope, which vendors emit liberally, stays terminal).
pub(super) fn retryable_stream_code(code: &str) -> bool {
    matches!(
        code,
        "server_is_overloaded"
            | "slow_down"
            | "rate_limit_exceeded"
            | "overloaded_error"
            | "temporarily_overloaded"
    )
}

/// Classification of a streamed failure event (`response.failed` / `error`)
/// for the stream retry loops. Mirrors codex's `parse_failed_response` split:
/// quota, policy, auth and invalid-request codes are terminal regardless of
/// what a status header suggested; the known-transient overload and
/// rate-limit codes are retryable within the caller's bounded budget, with
/// any server advice setting only the timing. Unknown codes stay terminal.
pub(super) enum StreamFailure {
    /// Propagate as-is. Carries the typed context-window cause when the code
    /// names one (the caller decides whether recovery is safe).
    Terminal(anyhow::Error),
    /// Retryable within the caller's bounded budget; advice may set timing.
    Retryable {
        error: anyhow::Error,
        advice: Option<super::http::RetryAfter>,
    },
}

pub(super) fn classify_stream_failure(
    code: &str,
    message: &str,
    advice: Option<super::http::RetryAfter>,
) -> StreamFailure {
    if is_context_window_code(code) {
        return StreamFailure::Terminal(anyhow::Error::new(super::ContextWindowExceeded(
            classify_stream_error(code, message),
        )));
    }
    if retryable_stream_code(code) {
        return StreamFailure::Retryable {
            error: anyhow::anyhow!(classify_stream_error(code, message)),
            advice,
        };
    }
    StreamFailure::Terminal(anyhow::anyhow!(classify_stream_error(code, message)))
}

/// True for events that evidence provider work: output items, tool argument
/// streams, native calls. Lifecycle-only events (`response.created`,
/// terminal events) do not count, so a rejection arriving before any of these
/// is pure. Mirrors [`ResponsesStreamTrace`]'s replay-safety logic.
fn observes_provider_effects(kind: &str) -> bool {
    kind.starts_with("response.")
        && !matches!(
            kind,
            "response.created"
                | "response.in_progress"
                | "response.completed"
                | "response.incomplete"
                | "response.failed"
        )
}

/// Classify a non-2xx HTTP response, surfacing any error code from the body
/// envelope so failures are diagnosable from the log.
pub(super) fn classify_http_error(status: reqwest::StatusCode, body: &str) -> String {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["code"]
                .as_str()
                .or_else(|| v["error"]["type"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if code.is_empty() {
        format!("openai responses {status}: {body}")
    } else {
        format!("openai responses {status} [{code}]: {body}")
    }
}

/// Typed context-window cause for a non-2xx HTTP rejection whose error
/// envelope carries a context code. A rejected request produced no output,
/// tool or native effect, so the cause is always safe for the loop's
/// compact-and-retry recovery; nothing was emitted that a replay could
/// duplicate.
pub(super) fn http_context_window_exceeded(
    status: reqwest::StatusCode,
    body: &str,
) -> Option<anyhow::Error> {
    let code = super::http::json_error_code(body)?;
    is_context_window_code(&code).then(|| {
        anyhow::Error::new(super::ContextWindowExceeded(classify_http_error(
            status, body,
        )))
    })
}

/// Diagnostic breadcrumb for a single Responses stream attempt.
///
/// Keep this transport-neutral: HTTP-SSE and WebSocket both consume the same
/// typed event payloads, so a fault should carry the same shape regardless of
/// framing.
#[derive(Debug, Default, Clone)]
pub(super) struct ResponsesStreamTrace {
    request_id: Option<String>,
    response_id: Option<String>,
    last_event_type: Option<String>,
    event_count: u64,
    bytes_consumed: u64,
    emitted_text: bool,
    observed_output: bool,
    terminal_seen: bool,
}

impl ResponsesStreamTrace {
    pub(super) fn new(request_id: Option<String>) -> Self {
        Self {
            request_id,
            ..Self::default()
        }
    }

    pub(super) fn observe_chunk(&mut self, len: usize) {
        self.bytes_consumed = self.bytes_consumed.saturating_add(len as u64);
    }

    pub(super) fn observe_event(&mut self, ev: &Value) {
        self.event_count = self.event_count.saturating_add(1);
        if let Some(kind) = ev["type"].as_str() {
            self.last_event_type = Some(kind.to_string());
            self.observed_output |= kind.starts_with("response.")
                && !matches!(
                    kind,
                    "response.created"
                        | "response.in_progress"
                        | "response.completed"
                        | "response.incomplete"
                        | "response.failed"
                );
        }
        if let Some(id) = ev["response"]["id"].as_str() {
            self.response_id = Some(id.to_string());
        }
    }

    pub(super) fn replay_safe(&self) -> bool {
        !self.observed_output && !self.emitted_text
    }

    pub(super) fn mark_emitted_text(&mut self) {
        self.emitted_text = true;
    }

    pub(super) fn mark_terminal_seen(&mut self) {
        self.terminal_seen = true;
    }

    pub(super) fn terminal_seen(&self) -> bool {
        self.terminal_seen
    }

    /// Bytes consumed so far, for the collector's bounded-buffer check.
    pub(super) fn bytes_consumed(&self) -> u64 {
        self.bytes_consumed
    }

    /// Parsed events so far.
    pub(super) fn event_count(&self) -> u64 {
        self.event_count
    }

    pub(super) fn fault_context(&self, transport: &str, attempt: u32, max_attempts: u32) -> String {
        format!(
            "{transport} stream fault diagnostics: request_id={}, response_id={}, last_event={}#{}, attempt={attempt}/{max_attempts}, bytes_consumed={}, emitted_text={}, terminal_seen={}",
            self.request_id.as_deref().unwrap_or("unknown"),
            self.response_id.as_deref().unwrap_or("unknown"),
            self.last_event_type.as_deref().unwrap_or("none"),
            self.event_count,
            self.bytes_consumed,
            self.emitted_text,
            self.terminal_seen,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{BaseInstructions, SystemPrompt};
    use bro_protocol::SERVICE_TIER_PRIORITY;

    fn state() -> ResponsesState {
        let mut s = ResponsesState::new(Auth::ApiKey("k".into()));
        s.session_id = "sess-1".into();
        s.input = vec![json!({
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}],
        })];
        s
    }

    fn advice(delay: std::time::Duration) -> crate::transport::http::RetryAfter {
        crate::transport::http::RetryAfter::from_delay(delay).expect("valid advice")
    }

    #[test]
    fn expired_advice_never_masks_a_fresh_deadline() {
        // An earlier advice can elapse unobserved (its request never ran: the
        // turn was cancelled mid-wait). A later failure then carries fresh
        // advice that must still gate the next request.
        let mut s = state();
        s.defer_retry_until(Some(advice(std::time::Duration::ZERO)));
        s.defer_retry_until(Some(advice(std::time::Duration::from_secs(3600))));
        let pending = s.pending_retry_advice().expect("fresh advice survives");
        assert!(
            pending.remaining_delay() > std::time::Duration::from_secs(3500),
            "an expired pending deadline must not discard a fresh, later one"
        );
        // None never clobbers the pending window.
        s.defer_retry_until(None);
        assert!(s.pending_retry_advice().is_some());
        // An elapsed window clears once consumed.
        let mut drained = state();
        drained.defer_retry_until(Some(advice(std::time::Duration::ZERO)));
        assert!(drained.take_elapsed_retry_advice().is_some());
        assert!(drained.pending_retry_advice().is_none());
    }

    #[test]
    fn live_advice_keeps_the_later_deadline() {
        let mut s = state();
        s.defer_retry_until(Some(advice(std::time::Duration::from_secs(60))));
        s.defer_retry_until(Some(advice(std::time::Duration::from_secs(5))));
        assert!(
            s.pending_retry_advice().expect("pending").remaining_delay()
                > std::time::Duration::from_secs(55),
            "two live windows keep the more restrictive deadline"
        );
        // Reversed order converges on the same deadline.
        let mut s = state();
        s.defer_retry_until(Some(advice(std::time::Duration::from_secs(5))));
        s.defer_retry_until(Some(advice(std::time::Duration::from_secs(60))));
        assert!(
            s.pending_retry_advice().expect("pending").remaining_delay()
                > std::time::Duration::from_secs(55)
        );
    }
    fn opts(system: SystemPrompt) -> TurnOpts {
        TurnOpts {
            model: "gpt-5-codex".into(),
            max_tokens: 16,
            base_instructions: None,
            system,
            effort: None,
            web_search: false,
            service_tier: None,
        }
    }

    fn opts_with_base(base: &str, system: SystemPrompt) -> TurnOpts {
        let mut opts = opts(system);
        opts.base_instructions = Some(BaseInstructions::new(base));
        opts
    }

    fn function_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("{name} description"),
            schema: json!({"type": "object"}),
            grammar: None,
        }
    }

    fn developer_texts(input: &[Value]) -> Vec<String> {
        input
            .iter()
            .filter(|it| it["role"] == "developer")
            .map(|it| it["content"][0]["text"].as_str().unwrap_or("").to_string())
            .collect()
    }

    fn response_events(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    #[test]
    fn responses_rejects_invalid_calls_atomically_and_retains_evidence() {
        let valid =
            json!({"type":"custom_tool_call","call_id":"custom-1","name":"exec","input":"text(1)"});
        for args in [
            json!("{"),
            json!(""),
            json!("null"),
            json!("[]"),
            json!("1"),
            Value::Null,
            json!({}),
        ] {
            let invalid = json!({"type":"function_call","call_id":"call-1","name":"file_write","arguments":args});
            let sse = response_events(&[
                json!({"type":"response.output_item.done","item":valid}),
                json!({"type":"response.output_item.done","item":invalid}),
                json!({"type":"response.completed","response":{"status":"completed"}}),
            ]);
            let mut s = state();
            s.push_user_text("fixture");
            let before = s.input.clone();
            let error = s
                .parse_sse(&sse)
                .err()
                .expect("invalid arguments cannot authorize defaults");
            assert_eq!(s.input, before);
            assert!(
                s.custom_tool_call_ids.is_empty(),
                "failed batch cannot leave a custom-call routing entry"
            );
            let evidence = error
                .downcast_ref::<super::super::FailedTurnObservation>()
                .unwrap();
            assert_eq!(evidence.tool_diagnostics[0]["evidence"]["sse"], sse);
        }
    }

    #[test]
    fn responses_requires_terminal_and_completed_consistent_calls() {
        let call =
            json!({"type":"function_call","call_id":"call-1","name":"file_write","arguments":"{}"});
        let done = json!({"type":"response.output_item.done","output_index":0,"item":call});
        let completed = json!({"type":"response.completed","response":{"status":"completed"}});
        let cases = [
            vec![done.clone()],
            vec![
                done.clone(),
                json!({"type":"response.incomplete","response":{"status":"incomplete"}}),
            ],
            vec![
                json!({"type":"response.output_item.added","output_index":0,"item":call}),
                completed.clone(),
            ],
            vec![done.clone(), done.clone(), completed.clone()],
            vec![
                done.clone(),
                json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","call_id":"call-1","name":"file_write","arguments":"{\"changed\":true}"}]}}),
            ],
            vec![
                done.clone(),
                json!({"type":"response.completed","response":{"status":"in_progress"}}),
            ],
            vec![completed.clone(), done.clone()],
            vec![
                json!({"type":"response.function_call_arguments.delta","item_id":"fc-1","delta":"{}"}),
                completed.clone(),
            ],
            vec![
                json!({"type":"response.output_item.added","output_index":0,"item":call}),
                json!({"type":"response.completed","response":{"output":[]}}),
            ],
            vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"different"}}),
                done.clone(),
                completed.clone(),
            ],
        ];
        for events in cases {
            let mut s = state();
            let before = s.input.clone();
            assert!(
                s.parse_sse(&response_events(&events)).is_err(),
                "{events:?}"
            );
            assert_eq!(s.input, before);
        }
        let mut s = state();
        let output = s.parse_sse(&response_events(&[
            json!({"type":"response.output_item.added","output_index":0,"item":call}),
            json!({"type":"response.completed","response":{"status":"completed","output":[call]}}),
        ])).unwrap();
        assert_eq!(
            output.tool_calls[0].args,
            json!({}),
            "complete terminal snapshot is authoritative"
        );
    }

    #[test]
    fn responses_completed_empty_output_preserves_streamed_items_and_native_history() {
        // Sanitized shape from the observed ChatGPT backend stream:
        // completed item events carry output; response.completed repeats only
        // an empty output array, terminal metadata and usage. No live ciphertext
        // or user prompt is needed to reproduce the wire contract.
        for custom in [false, true] {
            let reasoning = json!({"id":"rs_fixture","type":"reasoning","content":[],"summary":[],"encrypted_content":"fixture-opaque-state"});
            let call = if custom {
                json!({"id":"fc_fixture","type":"custom_tool_call","status":"completed","call_id":"call_fixture","name":"exec","input":"text('fixture');"})
            } else {
                json!({"id":"fc_fixture","type":"function_call","status":"completed","call_id":"call_fixture","name":"file_read","arguments":"{\"file_path\":\"fixture.txt\"}"})
            };
            let mut added = call.clone();
            added["status"] = json!("in_progress");
            let payload_field = if custom { "input" } else { "arguments" };
            added[payload_field] = json!("");
            let argument_event = if custom {
                "response.custom_tool_call_input.done"
            } else {
                "response.function_call_arguments.done"
            };
            let mut s = state();
            let output = s.parse_sse(&response_events(&[
                json!({"type":"response.created","response":{"id":"resp_fixture","status":"in_progress","output":[]}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_fixture","type":"reasoning","summary":[]}}),
                json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
                json!({"type":"response.output_item.added","output_index":1,"item":added}),
                json!({"type":argument_event,"item_id":"fc_fixture","output_index":1,(payload_field):call[payload_field]}),
                json!({"type":"response.output_item.done","output_index":1,"item":call}),
                json!({"type":"response.completed","response":{"id":"resp_fixture","status":"completed","output":[],"usage":{"input_tokens":123,"output_tokens":17,"input_tokens_details":{"cached_tokens":23}}}}),
            ])).unwrap();
            assert_eq!(output.stop, StopReason::ToolCalls);
            assert_eq!(output.tool_calls.len(), 1);
            assert_eq!(output.tool_calls[0].id, "call_fixture");
            assert_eq!(
                output.tool_calls[0].args,
                if custom {
                    json!({"source":"text('fixture');"})
                } else {
                    json!({"file_path":"fixture.txt"})
                }
            );
            assert_eq!(output.usage.input_tokens, 100);
            assert_eq!(output.usage.cached_input_tokens, 23);
            assert!(s.input.contains(&reasoning));
            assert!(s.input.contains(&call));
            assert_eq!(s.custom_tool_call_ids.contains("call_fixture"), custom);
        }
    }

    #[test]
    fn responses_empty_terminal_output_does_not_complete_missing_or_changed_items() {
        let terminal =
            json!({"type":"response.completed","response":{"status":"completed","output":[]}});
        let added = json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_fixture","type":"function_call","call_id":"call_fixture","name":"file_read","arguments":""}});
        let done = json!({"type":"response.output_item.done","output_index":0,"item":{"id":"fc_fixture","type":"function_call","status":"completed","call_id":"call_changed","name":"file_read","arguments":"{}"}});
        for events in [
            vec![added.clone(), terminal.clone()],
            vec![added, done.clone(), terminal.clone()],
            vec![done.clone(), done, terminal.clone()],
            vec![
                json!({"type":"response.function_call_arguments.done","item_id":"fc_missing","arguments":"{}"}),
                terminal,
            ],
        ] {
            let mut s = state();
            let before = s.input.clone();
            let sse = response_events(&events);
            let failure = s
                .parse_sse(&sse)
                .err()
                .expect("invalid item stream must fail");
            assert_eq!(s.input, before);
            let evidence = failure
                .downcast_ref::<super::super::FailedTurnObservation>()
                .unwrap();
            assert_eq!(evidence.tool_diagnostics[0]["evidence"]["sse"], sse);
        }
    }

    #[test]
    fn responses_partial_output_disables_replay_even_without_visible_text() {
        for event in [
            json!({"type":"response.output_item.added","item":{"type":"function_call"}}),
            json!({"type":"response.web_search_call.in_progress"}),
            json!({"type":"response.reasoning_text.delta","delta":"thinking"}),
        ] {
            let mut trace = ResponsesStreamTrace::default();
            trace.observe_event(&json!({"type":"response.created"}));
            assert!(trace.replay_safe());
            trace.observe_event(&event);
            assert!(
                !trace.replay_safe(),
                "observed provider work must not be silently replayed"
            );
        }
    }

    #[test]
    fn responses_preserves_text_only_incomplete_status() {
        let output = state().parse_sse(&response_events(&[
            json!({"type":"response.incomplete","response":{"status":"incomplete","output":[{
                "type":"message","status":"incomplete","content":[{"type":"output_text","text":"partial"}]
            }]}}),
        ])).unwrap();
        assert_eq!(output.stop, StopReason::Length);
        assert_eq!(output.text, "partial");
    }

    #[test]
    fn sync_ambient_injects_once_and_regates_on_change() {
        let mut s = state();
        s.sync_ambient(Some("MANIFEST v1"));
        assert_eq!(developer_texts(&s.input), vec!["MANIFEST v1"]);

        // Same content: no duplicate item.
        s.sync_ambient(Some("MANIFEST v1"));
        assert_eq!(developer_texts(&s.input).len(), 1);

        // None / empty: no change, hash retained.
        s.sync_ambient(None);
        s.sync_ambient(Some(""));
        assert_eq!(developer_texts(&s.input).len(), 1);

        // Changed content: re-injected.
        s.sync_ambient(Some("MANIFEST v2"));
        assert_eq!(
            developer_texts(&s.input),
            vec!["MANIFEST v1", "MANIFEST v2"]
        );
    }

    #[test]
    fn snapshot_round_trips_ambient_hash_and_accepts_legacy_shape() {
        let mut s = state();
        s.sync_ambient(Some("MANIFEST"));
        let snap = s.snapshot();

        let mut restored = ResponsesState::new(Auth::ApiKey("k".into()));
        restored.restore(snap);
        assert_eq!(restored.ambient_hash, s.ambient_hash);
        // Unchanged manifest after resume: no re-injection.
        let before = developer_texts(&restored.input).len();
        restored.sync_ambient(Some("MANIFEST"));
        assert_eq!(developer_texts(&restored.input).len(), before);

        // Legacy bare-array snapshot: hash cleared → one safe re-injection.
        let mut legacy = ResponsesState::new(Auth::ApiKey("k".into()));
        legacy.restore(json!([{"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]}]));
        assert_eq!(legacy.ambient_hash, None);
        legacy.sync_ambient(Some("MANIFEST"));
        assert_eq!(developer_texts(&legacy.input).len(), 1);
    }

    #[test]
    fn build_body_sends_no_ephemeral_item_without_volatile() {
        // Ambient rides the persisted buffer, not the per-request ephemera:
        // with no volatile tail, the request input matches the buffer exactly.
        let mut s = state();
        s.sync_ambient(Some("MANIFEST"));
        let body = s.build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("S".into()),
                ambient: Some("MANIFEST".into()),
                volatile: None,
            }),
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), s.input.len(), "no ephemeral developer item");
        // With a volatile nudge, exactly one ephemeral item is appended.
        let body = s.build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("S".into()),
                ambient: Some("MANIFEST".into()),
                volatile: Some("NUDGE".into()),
            }),
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), s.input.len() + 1);
        assert_eq!(input.last().unwrap()["content"][0]["text"], "NUDGE");
    }

    fn grammar_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("{name} description"),
            schema: json!({"type": "object"}),
            grammar: Some(bro_tools::FreeformGrammar {
                syntax: "lark".into(),
                definition: "start: SOURCE\nSOURCE: /[\\s\\S]+/".into(),
            }),
        }
    }

    #[test]
    fn stable_is_instructions_volatile_is_trailing_developer_item() {
        let body = state().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                ambient: None,
                volatile: Some("MANIFEST".into()),
            }),
        );
        assert_eq!(body["instructions"], "BASE");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "developer");
        assert_eq!(input[1]["content"][0]["text"], "MANIFEST");
        // Volatile never persisted into the buffer.
        assert_eq!(state().input.len(), 1);
    }

    #[test]
    fn base_leads_instructions_and_preserves_overlay_content() {
        let body = state().build_body(
            &[],
            &opts_with_base(
                "BASE",
                SystemPrompt {
                    stable: Some("OVERLAY".into()),
                    ambient: None,
                    volatile: Some("MANIFEST".into()),
                },
            ),
        );
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.starts_with("BASE"));
        assert!(instructions.contains("OVERLAY"));
        assert!(!instructions.contains("You are a helpful coding assistant"));

        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "developer");
        assert_eq!(input[1]["content"][0]["text"], "MANIFEST");
    }

    #[test]
    fn empty_stable_falls_back_to_nonempty_instructions() {
        let body = state().build_body(
            &[],
            &opts(SystemPrompt {
                stable: None,
                ambient: None,
                volatile: Some("V".into()),
            }),
        );
        assert!(!body["instructions"].as_str().unwrap().is_empty());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["role"], "developer");
    }

    #[test]
    fn empty_base_degrades_to_prior_stable_behavior() {
        let body = state().build_body(
            &[],
            &opts_with_base(
                "",
                SystemPrompt {
                    stable: Some("OVERLAY".into()),
                    ambient: None,
                    volatile: None,
                },
            ),
        );
        assert_eq!(body["instructions"], "OVERLAY");
    }

    #[test]
    fn normalize_synthesizes_missing_function_outputs_and_removes_orphan_outputs() {
        let mut input = vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
            json!({"type": "function_call", "call_id": "missing", "name": "x", "arguments": "{}"}),
            json!({"type": "function_call", "call_id": "matched", "name": "x", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "orphan", "output": "drop"}),
            json!({"type": "function_call_output", "call_id": "matched", "output": "keep"}),
        ];

        normalize_responses_input(&mut input);

        assert_eq!(input.len(), 5);
        assert!(input.iter().any(|item| item["type"] == "message"));
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "function_call" && item["call_id"] == "missing")
        );
        assert!(input.iter().any(|item| {
            item["type"] == "function_call_output"
                && item["call_id"] == "missing"
                && item["output"] == "aborted"
        }));
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "function_call" && item["call_id"] == "matched")
        );
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "function_call_output" && item["call_id"] == "matched")
        );
        assert!(!input.iter().any(|item| item["call_id"] == "orphan"));
    }

    #[test]
    fn stream_trace_formats_fault_diagnostics() {
        let mut trace = ResponsesStreamTrace::new(Some("req_123".into()));
        trace.observe_chunk(12);
        trace.observe_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_456"}
        }));
        trace.mark_emitted_text();

        let text = trace.fault_context("responses HTTP-SSE", 2, 4);

        assert!(text.contains("request_id=req_123"));
        assert!(text.contains("response_id=resp_456"));
        assert!(text.contains("last_event=response.created#1"));
        assert!(text.contains("attempt=2/4"));
        assert!(text.contains("bytes_consumed=12"));
        assert!(text.contains("emitted_text=true"));
        assert!(text.contains("terminal_seen=false"));
    }

    #[test]
    fn build_body_serializes_grammar_tools_as_custom_and_normal_tools_as_functions() {
        let body = state().build_body(
            &[grammar_spec("exec"), function_spec("wait")],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                ambient: None,
                volatile: None,
            }),
        );

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "custom");
        assert_eq!(tools[0]["name"], "exec");
        assert_eq!(tools[0]["description"], "exec description");
        assert_eq!(tools[0]["format"]["type"], "grammar");
        assert_eq!(tools[0]["format"]["syntax"], "lark");
        assert_eq!(
            tools[0]["format"]["definition"],
            "start: SOURCE\nSOURCE: /[\\s\\S]+/"
        );
        assert_eq!(tools[1]["type"], "function");
        assert_eq!(tools[1]["name"], "wait");
        assert_eq!(tools[1]["parameters"]["type"], "object");
    }

    #[test]
    fn parse_sse_maps_custom_tool_call_input_to_source_arg() {
        let mut s = state();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"call_id\":\"call-1\",\"name\":\"exec\",\"input\":\"console.log(1)\"}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );

        let out = s.parse_sse(sse).unwrap();

        assert_eq!(out.stop, StopReason::ToolCalls);
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].id, "call-1");
        assert_eq!(out.tool_calls[0].name, "exec");
        assert_eq!(out.tool_calls[0].args["source"], "console.log(1)");
    }

    #[test]
    fn custom_call_result_serializes_as_custom_tool_call_output() {
        let mut s = state();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"call_id\":\"call-1\",\"name\":\"exec\",\"input\":\"console.log(1)\"}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        s.parse_sse(sse).unwrap();

        s.push_tool_results(vec![ToolResult {
            id: "call-1".into(),
            content: "ok".into(),
            is_error: false,
        }]);

        let output = s.input.last().unwrap();
        assert_eq!(output["type"], "custom_tool_call_output");
        assert_eq!(output["call_id"], "call-1");
        assert_eq!(output["output"], "ok");
    }

    #[test]
    fn normalize_synthesizes_missing_custom_outputs_and_removes_orphan_custom_outputs() {
        let mut input = vec![
            json!({"type": "custom_tool_call", "call_id": "missing", "name": "exec", "input": "a()"}),
            json!({"type": "custom_tool_call", "call_id": "matched", "name": "exec", "input": "b()"}),
            json!({"type": "custom_tool_call_output", "call_id": "orphan", "output": "drop"}),
            json!({"type": "custom_tool_call_output", "call_id": "matched", "output": "keep"}),
        ];

        normalize_responses_input(&mut input);

        assert_eq!(input.len(), 4);
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "custom_tool_call" && item["call_id"] == "missing")
        );
        assert!(input.iter().any(|item| {
            item["type"] == "custom_tool_call_output"
                && item["call_id"] == "missing"
                && item["output"] == "aborted"
        }));
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "custom_tool_call" && item["call_id"] == "matched")
        );
        assert!(input.iter().any(|item| {
            item["type"] == "custom_tool_call_output" && item["call_id"] == "matched"
        }));
        assert!(!input.iter().any(|item| item["call_id"] == "orphan"));
    }

    #[test]
    fn modern_body_carries_cache_key_service_tier_and_reasoning() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            ambient: None,
            volatile: None,
        });
        o.effort = Some("medium".into());
        o.service_tier = Some(SERVICE_TIER_PRIORITY.into());
        let body = state().build_body(&[], &o);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["prompt_cache_key"], "sess-1");
        assert_eq!(body["service_tier"], SERVICE_TIER_PRIORITY);
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn default_effort_requests_encrypted_reasoning_and_preserves_it_for_next_request() {
        let o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            ambient: None,
            volatile: None,
        });
        assert!(o.effort.is_none());
        let mut s = state();
        let initial = s.build_body(&[], &o);
        assert_eq!(initial["include"], json!(["reasoning.encrypted_content"]));
        assert!(initial.get("reasoning").is_none());
        s.parse_sse(concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"ENC\",\"summary\":[]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        )).unwrap();
        // The same authoritative snapshot feeds HTTP replay after a resume or
        // WebSocket fallback, independently of the request's effort selection.
        let mut restored = state();
        restored.restore(s.snapshot());
        restored.push_user_text("continue");
        let next = restored.build_body(&[], &o);
        assert!(
            next["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "reasoning" && item["encrypted_content"] == "ENC")
        );
        assert_eq!(next["include"], initial["include"]);
    }

    #[test]
    fn default_service_tier_is_dropped() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            ambient: None,
            volatile: None,
        });
        o.service_tier = Some(SERVICE_TIER_DEFAULT.into());
        let body = state().build_body(&[], &o);
        assert!(body.get("service_tier").is_none());
    }

    #[test]
    fn reasoning_omitted_for_non_reasoning_model() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            ambient: None,
            volatile: None,
        });
        o.model = "gpt-4o".into();
        o.effort = Some("high".into());
        let body = state().build_body(&[], &o);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn reasoning_item_replayed_only_with_encrypted_content() {
        let mut s = state();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"ENC\",\"summary\":[]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"bare\"}]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        s.parse_sse(sse).unwrap();
        let reasoning: Vec<_> = s
            .input
            .iter()
            .filter(|i| i["type"] == "reasoning")
            .collect();
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0]["encrypted_content"], "ENC");
    }

    #[test]
    fn parse_sse_surfaces_reasoning_as_thinking() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"pondering\"}]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        let out = state().parse_sse(sse).unwrap();
        assert_eq!(out.text, "answer");
        assert_eq!(out.thinking, "pondering");
    }

    #[test]
    fn parse_sse_reads_responses_end_turn_follow_up_signal() {
        let false_out = state()
            .parse_sse("data: {\"type\":\"response.completed\",\"response\":{\"end_turn\":false,\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n")
            .unwrap();
        assert_eq!(false_out.end_turn, Some(false));

        let true_out = state()
            .parse_sse("data: {\"type\":\"response.completed\",\"response\":{\"end_turn\":true,\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n")
            .unwrap();
        assert_eq!(true_out.end_turn, Some(true));

        let absent_out = state()
            .parse_sse("data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n")
            .unwrap();
        assert_eq!(absent_out.end_turn, None);
    }

    #[test]
    fn responses_split_avoids_orphan_tool_output() {
        let input = vec![
            json!({"type": "message", "role": "user", "content": []}),
            json!({"type": "function_call", "call_id": "a", "name": "f", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "a", "output": "r"}),
            json!({"type": "message", "role": "assistant", "content": []}),
        ];
        // split=2 would orphan function_call_output(a) (its call at index 1 is
        // discarded); split=1 keeps the call/output pair together.
        assert_eq!(responses_split(&input, 3), Some(1));
        // Cutting only the final item is never offered (limit 1 → empty range).
        assert_eq!(responses_split(&input, 1), None);
    }

    #[test]
    fn pure_helpers() {
        assert_eq!(normalize_effort("minimal"), "minimal");
        // gpt-5.6 max/ultra pass through verbatim (no longer collapsed to high).
        assert_eq!(normalize_effort("max"), "max");
        assert_eq!(normalize_effort("ultra"), "ultra");
        assert_eq!(normalize_effort("xhigh"), "xhigh");
        assert_eq!(normalize_effort("bogus"), "medium");
        assert!(model_supports_reasoning("gpt-5-codex"));
        assert!(model_supports_reasoning("o3"));
        assert!(!model_supports_reasoning("gpt-4o"));
        assert!(!model_supports_reasoning("gpt-4.1"));
        assert_eq!(
            service_tier_for_request(Some(SERVICE_TIER_PRIORITY)).as_deref(),
            Some(SERVICE_TIER_PRIORITY)
        );
        assert_eq!(service_tier_for_request(Some(SERVICE_TIER_DEFAULT)), None);
        assert_eq!(service_tier_for_request(Some("")), None);
        assert_eq!(service_tier_for_request(None), None);
    }

    #[test]
    fn identity_auth_headers_shape() {
        let h = identity_auth_headers(
            "sess-1",
            "thread-1",
            &Auth::ChatGpt {
                access_token: "tok".into(),
                account_id: "acct".into(),
            },
        );
        let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("session-id"), Some("sess-1"));
        assert_eq!(get("thread-id"), Some("thread-1"));
        assert_eq!(get("authorization"), Some("Bearer tok"));
        assert_eq!(get("chatgpt-account-id"), Some("acct"));
        assert_eq!(get("originator"), Some("codex_cli_rs"));
        // No defunct beta header; api-key path carries no account id.
        assert!(h.iter().all(|(n, _)| *n != "OpenAI-Beta"));
        let api = identity_auth_headers("", "t", &Auth::ApiKey("k".into()));
        assert!(api.iter().all(|(n, _)| *n != "session-id")); // empty session id omitted
        assert!(api.iter().all(|(n, _)| *n != "chatgpt-account-id"));
    }

    #[test]
    fn classify_stream_error_names_codes() {
        assert!(
            classify_stream_error("context_length_exceeded", "too big").contains("context window")
        );
        assert!(classify_stream_error("server_is_overloaded", "busy").contains("overloaded"));
        assert!(classify_stream_error("", "boom").contains("boom"));
    }

    #[test]
    fn parse_sse_context_window_error_is_typed_recoverable() {
        // A context-window rejection must surface as the typed, recoverable
        // ContextWindowExceeded cause so the agent loop can compact + retry.
        let sse = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"context_window_exceeded\",\"message\":\"too big\"}}}\n";
        let err = match state().parse_sse(sse) {
            Ok(_) => panic!("must error on response.failed"),
            Err(e) => e,
        };
        assert!(
            crate::transport::is_context_window_exceeded(&err),
            "context_window_exceeded should be the typed recoverable error, got: {err:#}"
        );

        // Any other failure code stays a plain (non-recoverable) error.
        let other = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"busy\"}}}\n";
        let err = match state().parse_sse(other) {
            Ok(_) => panic!("must error on response.failed"),
            Err(e) => e,
        };
        assert!(
            !crate::transport::is_context_window_exceeded(&err),
            "overload must not be classified as context-window: {err:#}"
        );
    }

    /// LIVE PROBE (ignored, double-gated). Answers the open questions in
    /// design/bro-harness/brodex-compaction.md §5 against the real ChatGPT
    /// backend: does our account honor (a) the unary `responses/compact`
    /// endpoint and (b) the streaming `compaction_trigger` item? Self-validating:
    /// a normal `/responses` call first proves model+auth, so a compaction
    /// failure is attributable to the compaction surface, not setup.
    ///
    /// Run with:
    ///   BRO_HARNESS_LIVE_PROBE=1 [BRO_HARNESS_PROBE_MODEL=gpt-5.1-codex] \
    ///     cargo test -p bro-harness --bins probe_responses_compaction \
    ///     -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "live backend probe; set BRO_HARNESS_LIVE_PROBE=1"]
    async fn probe_responses_compaction() {
        if std::env::var("BRO_HARNESS_LIVE_PROBE").is_err() {
            eprintln!("skip: set BRO_HARNESS_LIVE_PROBE=1 to run the live probe");
            return;
        }
        let model = std::env::var("BRO_HARNESS_PROBE_MODEL")
            .unwrap_or_else(|_| "gpt-5.1-codex".to_string());
        let http = reqwest::Client::new();
        let auth = resolve_auth(&http)
            .await
            .expect("resolve auth (refreshes token)");
        let endpoint = http_endpoint(&auth);
        let compact_url = format!("{endpoint}/compact");
        let session_id = new_id();
        let thread_id = new_id();
        let headers = identity_auth_headers(&session_id, &thread_id, &auth);
        eprintln!("[probe] model={model} endpoint={endpoint}");

        let convo = json!([
            {"type":"message","role":"user","content":[{"type":"input_text",
                "text":"Step 1: remember the magic token BANANA-7. Acknowledge briefly."}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text",
                "text":"Acknowledged — magic token BANANA-7 noted."}]},
            {"type":"message","role":"user","content":[{"type":"input_text",
                "text":"Step 2: now compute 17 * 3 and explain in one line."}]}
        ]);
        let send = |url: String, body: serde_json::Value, sse: bool| {
            let http = http.clone();
            let headers = headers.clone();
            async move {
                let mut rb = http.post(&url).header("content-type", "application/json");
                rb = rb.header(
                    "accept",
                    if sse {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                );
                for (n, v) in &headers {
                    rb = rb.header(*n, v);
                }
                let resp = rb.json(&body).send().await.expect("send");
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                (status, text)
            }
        };
        let head = |s: &str, n: usize| s.chars().take(n).collect::<String>();

        // 1. Normal /responses — proves model + auth + headers.
        let (st, body) = send(
            endpoint.clone(),
            json!({"model": model, "input": convo, "instructions": "You are a helpful assistant.",
                   "stream": true, "store": false, "tool_choice": "auto", "parallel_tool_calls": false}),
            true,
        )
        .await;
        eprintln!(
            "\n[probe 1] NORMAL /responses -> {st}\n{}",
            head(&body, 800)
        );

        // 2. Unary /responses/compact — CompactionInput shape.
        let (st, body2) = send(
            compact_url.clone(),
            json!({"model": model, "input": convo, "instructions": "You are a helpful assistant.",
                   "tools": [], "parallel_tool_calls": false}),
            false,
        )
        .await;
        eprintln!(
            "\n[probe 2] UNARY /responses/compact -> {st}\n{}",
            head(&body2, 1500)
        );

        // 3. Streaming compaction_trigger on /responses.
        let mut trig = convo.as_array().unwrap().clone();
        trig.push(json!({"type": "compaction_trigger"}));
        let (st, body3) = send(
            endpoint.clone(),
            json!({"model": model, "input": trig, "instructions": "You are a helpful assistant.",
                   "stream": true, "store": false}),
            true,
        )
        .await;
        eprintln!(
            "\n[probe 3] STREAM compaction_trigger -> {st}\n{}",
            head(&body3, 1200)
        );

        // 4. REPLAY: feed the unary-compacted output (incl. the encrypted
        // compaction_summary) back as input + a fresh user turn. Proves the
        // canonical loop: does the backend accept the encrypted summary on
        // replay under store:false, and does the model answer from it?
        let parsed: serde_json::Value = serde_json::from_str(&body2).unwrap_or_else(|_| json!({}));
        match parsed["output"].as_array() {
            Some(out) if !out.is_empty() => {
                let kinds: Vec<String> = out
                    .iter()
                    .map(|i| i["type"].as_str().unwrap_or("?").to_string())
                    .collect();
                eprintln!("\n[probe 4] compacted output item types: {kinds:?}");
                let mut replay = out.clone();
                replay.push(json!({"type":"message","role":"user","content":[{"type":"input_text",
                    "text":"Using only the prior context, answer in one short line: what magic token did I give you, and what is 17*3?"}]}));
                let (st, body4) = send(
                    endpoint.clone(),
                    json!({"model": model, "input": replay, "instructions": "You are a helpful assistant.",
                           "stream": true, "store": false}),
                    true,
                )
                .await;
                eprintln!("[probe 4] REPLAY compacted history -> {st}");
                // Surface only the output_text deltas so we can see the answer.
                let answer: String = body4
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
                    .filter(|e| e["type"] == "response.output_text.delta")
                    .filter_map(|e| e["delta"].as_str().map(str::to_string))
                    .collect();
                eprintln!("[probe 4] model answer: {}", head(&answer, 400));
                eprintln!(
                    "[probe 4] mentions BANANA-7: {}",
                    answer.contains("BANANA-7")
                );
                eprintln!("[probe 4] mentions 51: {}", answer.contains("51"));
            }
            _ => eprintln!("\n[probe 4] skipped: no output array in unary compact body"),
        }
    }

    fn lite_state() -> ResponsesState {
        let mut s = state();
        s.configure_responses_lite(true);
        s
    }

    fn lite_opts(base: &str) -> TurnOpts {
        let mut o = opts(SystemPrompt::default());
        o.base_instructions = Some(BaseInstructions::new(base));
        o
    }

    #[test]
    fn lite_body_carries_codex_shape_and_stable_prefix_ids() {
        let mut s = lite_state();
        let tools = vec![function_spec("read"), function_spec("shell")];
        let options = lite_opts("You are a coding agent.");
        s.sync_lite_catalog(&tools, &options).unwrap();
        let body = s.build_body(&tools, &options);
        assert_eq!(body["instructions"], "");
        assert!(
            body.get("tools").is_none(),
            "Lite omits the tools parameter; definitions ride in history"
        );
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        let input = body["input"].as_array().unwrap();
        // Request-only base+stable prefix message leads with a stable id,
        // then the history-carried definitions, then the user item.
        assert_eq!(input[0]["role"], "developer");
        let msg_id = input[0]["id"].as_str().unwrap();
        assert!(msg_id.starts_with("msg_"), "{msg_id}");
        assert_eq!(
            input[0]["content"][0]["text"],
            json!("You are a coding agent.")
        );
        assert_eq!(input[1]["type"], "additional_tools");
        assert_eq!(input[1]["role"], "developer");
        let at_id = input[1]["id"].as_str().unwrap();
        assert!(at_id.starts_with("at_"), "{at_id}");
        assert_eq!(input[1]["tools"].as_array().unwrap().len(), 2);
        assert_eq!(input[2]["role"], "user");
        // The prefix message is request-only: never persisted.
        assert!(
            s.input
                .iter()
                .all(|item| item["id"].as_str() != Some(msg_id)),
            "the base-instructions message must not persist into the buffer"
        );
        // Stable identity across rebuilds.
        let again = s.build_body(&tools, &options);
        assert_eq!(again["input"][0]["id"], json!(msg_id));
        assert_eq!(again["input"][1]["id"], json!(at_id));
    }

    #[test]
    fn lite_catalog_initial_prefix_then_deltas_at_the_sampling_boundary() {
        let mut s = lite_state();
        let options = lite_opts("base");
        let tools = vec![function_spec("read")];
        let added = s.sync_lite_catalog(&tools, &options).unwrap();
        assert!(added > 0);
        // Initial definitions sit before the first user item.
        let position = s
            .input
            .iter()
            .position(|item| item["role"] == "user")
            .unwrap();
        assert_eq!(s.input[position - 1]["type"], "additional_tools");

        // Unchanged catalog: nothing appended, nothing charged.
        let added = s.sync_lite_catalog(&tools, &options).unwrap();
        assert_eq!(added, 0);
        assert_eq!(
            s.input
                .iter()
                .filter(|item| item["type"] == "additional_tools")
                .count(),
            1
        );

        // A changed tool appends a delta at the end (sampling boundary),
        // carrying only the changed declaration.
        let changed = {
            let mut spec = function_spec("read");
            spec.description = "read files faster".into();
            vec![spec]
        };
        let added = s.sync_lite_catalog(&changed, &options).unwrap();
        assert!(added > 0);
        let last = s.input.last().unwrap();
        assert_eq!(last["type"], "additional_tools");
        assert_eq!(last["tools"].as_array().unwrap().len(), 1);
        assert_eq!(last["tools"][0]["description"], json!("read files faster"));
    }

    #[test]
    fn lite_restore_drops_an_uncoupled_baseline_and_re_renders_fully() {
        let mut s = lite_state();
        let options = lite_opts("base");
        let tools = vec![function_spec("read")];
        s.sync_lite_catalog(&tools, &options).unwrap();
        let snapshot = s.snapshot();
        // Simulate lost definition history: restore into a state whose input
        // dropped the definitions item (a torn tail beyond the snapshot).
        let mut truncated = snapshot.clone();
        truncated["input"]
            .as_array_mut()
            .unwrap()
            .retain(|item| item["type"] != "additional_tools");
        let mut restored = lite_state();
        restored.restore(snapshot);
        assert!(
            restored.uses_responses_lite(),
            "the capability flag survives resume"
        );
        let mut lost = lite_state();
        lost.restore(truncated);
        // The uncoupled baseline rebuilds: the next sync re-renders the full
        // catalog before the first user item.
        let before = lost.input.len();
        lost.sync_lite_catalog(&tools, &options).unwrap();
        assert_eq!(lost.input.len(), before + 1);
        assert_eq!(
            lost.input
                .iter()
                .position(|item| item["role"] == "user")
                .unwrap(),
            1,
            "full re-render lands before the first user item"
        );
    }

    #[test]
    fn mode_toggle_filters_lite_items_per_request_only() {
        let mut s = lite_state();
        let options = lite_opts("base");
        let tools = vec![function_spec("read")];
        s.sync_lite_catalog(&tools, &options).unwrap();
        let lite_items = s
            .input
            .iter()
            .filter(|item| item["type"] == "additional_tools")
            .count();
        assert_eq!(lite_items, 1);

        s.configure_responses_lite(false);
        let body = s.build_body(&tools, &options);
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "additional_tools"),
            "the ordinary wire never carries Lite definitions items"
        );
        assert!(
            body.get("tools").is_some(),
            "the ordinary wire keeps its tools parameter"
        );
        // Authoritative history keeps the items across the toggle.
        assert_eq!(
            s.input
                .iter()
                .filter(|item| item["type"] == "additional_tools")
                .count(),
            1
        );
        s.configure_responses_lite(true);
        let body = s.build_body(&tools, &options);
        assert_eq!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "additional_tools")
                .count(),
            1
        );
    }

    #[test]
    fn preview_lite_body_prepares_a_copy_and_preserves_the_source() {
        let mut s = lite_state();
        let options = lite_opts("base");
        let tools = vec![function_spec("read")];
        let before = s.snapshot();
        let body = s.preview_lite_body(&tools, &options, true).unwrap();
        assert_eq!(
            s.snapshot(),
            before,
            "a failed or previewed compaction must preserve the exact source state"
        );
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "additional_tools")
        );
        // Deterministic preview: the same inputs produce the same body.
        let again = s.preview_lite_body(&tools, &options, true).unwrap();
        assert_eq!(body, again);
        // A non-Lite preview keeps the ordinary wire.
        let plain = s.preview_lite_body(&tools, &options, false).unwrap();
        assert!(
            plain["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "additional_tools")
        );
        assert!(plain.get("tools").is_some());
    }

    #[test]
    fn lite_recovery_restates_the_current_catalog_after_stale_history() {
        let mut s = lite_state();
        let options = lite_opts("base");
        let read = function_spec("read");
        let faster = {
            let mut spec = function_spec("read");
            spec.description = "read files faster".into();
            spec
        };

        // Build retained catalog history: initial definitions, a delta, and
        // a removal notice for the re-added tool.
        s.sync_lite_catalog(&[read.clone()], &options).unwrap();
        s.sync_lite_catalog(&[faster.clone()], &options).unwrap();
        s.sync_lite_catalog(&[], &options).unwrap();
        let stale_len = s.input.len();

        // Lose the baseline while the stale items stay retained (the chain's
        // initial link was dropped from history, e.g. a torn tail).
        s.input.remove(
            s.input
                .iter()
                .position(|item| item["type"] == "additional_tools")
                .unwrap(),
        );

        // Recovery with the original catalog re-stated: the reset notice and
        // current definitions land at the sampling boundary, AFTER every
        // stale delta and removal notice, so the old removal cannot override.
        let added = s.sync_lite_catalog(&[read.clone()], &options).unwrap();
        assert!(added > 0);
        assert_eq!(s.input.len(), stale_len + 1); // one item removed, two appended
        let notice = s.input[s.input.len() - 2];
        assert_eq!(notice["role"], "developer");
        let text = notice["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Tool definitions were reset"), "{text}");
        let current = s.input.last().unwrap();
        assert_eq!(current["type"], "additional_tools");
        assert_eq!(
            current["tools"][0]["description"],
            json!("read description"),
            "the current catalog has the last word"
        );
        // Unchanged follow-up: nothing new, nothing charged.
        let added = s.sync_lite_catalog(&[read], &options).unwrap();
        assert_eq!(added, 0);
        assert_eq!(s.input.len(), stale_len + 1);
    }

    #[test]
    fn lite_recovery_with_empty_current_catalog_resets_without_definitions() {
        let mut s = lite_state();
        let options = lite_opts("base");
        s.sync_lite_catalog(&[function_spec("read")], &options)
            .unwrap();
        assert!(
            s.input
                .iter()
                .any(|item| item["type"] == "additional_tools")
        );

        // Compaction kept the old definitions and root reset the baseline:
        // the empty current catalog must still supersede them explicitly.
        s.reset_lite_baseline();
        let added = s.sync_lite_catalog(&[], &options).unwrap();
        assert!(added > 0);
        let last = s.input.last().unwrap();
        assert_eq!(last["role"], "developer");
        let text = last["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no tools are currently available"), "{text}");
        // Unchanged follow-up: known-empty baseline couples through the
        // retained reset notice.
        let added = s.sync_lite_catalog(&[], &options).unwrap();
        assert_eq!(added, 0);
        assert_eq!(s.input.last().unwrap(), last);
    }

    #[test]
    fn reset_lite_baseline_forces_a_full_re_render_after_compaction() {
        let mut s = lite_state();
        let options = lite_opts("base");
        let tools = vec![function_spec("read")];
        s.sync_lite_catalog(&tools, &options).unwrap();
        // Compaction rebuilt history without definition items.
        s.input.retain(|item| item["type"] != "additional_tools");
        s.reset_lite_baseline();
        s.sync_lite_catalog(&tools, &options).unwrap();
        assert_eq!(
            s.input
                .iter()
                .filter(|item| item["type"] == "additional_tools")
                .count(),
            1
        );
    }

    #[test]
    fn lite_full_render_never_lands_inside_the_protected_prefix() {
        let mut s = lite_state();
        s.protected_prefix = 1;
        s.input.insert(
            0,
            json!({"type":"message", "role":"user", "content":"canonical window"}),
        );
        let options = lite_opts("base");
        s.sync_lite_catalog(&[function_spec("read")], &options)
            .unwrap();
        let kinds: Vec<&str> = s
            .input
            .iter()
            .map(|item| item["type"].as_str().unwrap_or("message"))
            .collect();
        assert_eq!(
            kinds,
            vec!["message", "additional_tools", "message"],
            "the canonical window stays first and untouched"
        );
    }

    #[test]
    fn web_search_joins_the_lite_catalog_as_a_builtin_declaration() {
        let mut s = lite_state();
        let mut options = lite_opts("base");
        options.web_search = true;
        s.sync_lite_catalog(&[function_spec("read")], &options)
            .unwrap();
        let defs = s
            .input
            .iter()
            .find(|item| item["type"] == "additional_tools")
            .unwrap();
        let names: Vec<&str> = defs["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .or_else(|| tool["type"].as_str())
                    .unwrap()
            })
            .collect();
        assert_eq!(names, vec!["read", "web_search"]);
    }
}
