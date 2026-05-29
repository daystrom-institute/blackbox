//! The transport-agnostic tool-calling loop.
//!
//! ```text
//! build transport (anthropic | openai-chat | openai-responses)
//! restore on --resume; push user turn
//! loop:
//!   out = transport.run_turn(tool_specs, opts)   // wire encode/decode inside
//!   emit assistant text (Claude envelope)
//!   if out.stop != ToolCalls: break
//!   dispatch client tool_calls; push results
//! emit result; persist snapshot
//! ```
//!
//! The transport handles all wire differences; the loop and the stdout
//! envelope are identical across providers.

use crate::cli::Cli;
use crate::emit::Emitter;
use crate::registry::{PinPolicy, Registry};
use crate::session::{SaveState, SessionStore};
use crate::mcp;
use crate::transport::{self, StopReason, SystemPrompt, TransportKind, TurnOpts, Usage};
use anyhow::{Context, Result};
use bro_tools::{SafetyPolicy, ToolCx, builtin_tools};
use std::io::Read;
use std::sync::Arc;

const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Hard backstop on loop iterations; the daemon's supervision is the outer
/// guard. Override with `BRO_HARNESS_MAX_TURNS`.
const DEFAULT_MAX_TURNS: u64 = 50;

pub async fn run(cli: Cli) -> Result<()> {
    if let Some(fmt) = cli.output_format.as_deref()
        && fmt != "stream-json"
    {
        anyhow::bail!("unsupported --output-format {fmt}; only stream-json");
    }

    let prompt = resolve_prompt(&cli)?;
    let max_tokens = env_u32("BRO_HARNESS_MAX_TOKENS").unwrap_or(DEFAULT_MAX_TOKENS);
    let max_turns = env_u64("BRO_HARNESS_MAX_TURNS").unwrap_or(DEFAULT_MAX_TURNS);
    let web_search = std::env::var("BRO_HARNESS_WEB_SEARCH")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);

    // Empty --system-prompt means "suppress" (provider-defaults mode).
    let system = match cli.system_prompt.as_deref() {
        Some("") | None => None,
        Some(s) => Some(s.to_string()),
    };

    let kind = TransportKind::from_env();
    let mut tx = transport::build_transport(kind).await?;

    let store = SessionStore::open(cli.session_id.as_deref(), cli.resume.as_deref())?;
    let restored_model = store.restored.as_ref().and_then(|r| r.model.clone());
    // Loop-level side cells restored from a prior turn. Each cell deserializes
    // its own slot tolerantly (absent/garbage → empty), so old session files
    // and partial blobs resume cleanly.
    let prior_side = store
        .restored
        .as_ref()
        .map(|r| r.side.clone())
        .unwrap_or(serde_json::Value::Null);
    let todos = Arc::new(std::sync::Mutex::new(bro_tools::TodoList::from_side(
        prior_side.get("todos").unwrap_or(&serde_json::Value::Null),
    )));
    if let Some(r) = &store.restored {
        if r.transport != tx.name() {
            anyhow::bail!(
                "resume transport mismatch: session is '{}', harness is '{}'",
                r.transport,
                tx.name()
            );
        }
        tx.restore(r.snapshot.clone());
    }
    tx.push_user_text(&prompt);

    // On resume the daemon doesn't re-pass --model (it's implied by the
    // session), so fall back to the model persisted with the session.
    let model = cli
        .model
        .clone()
        .or(restored_model)
        .or_else(|| std::env::var("ANTHROPIC_MODEL").ok())
        .or_else(|| std::env::var("BRO_HARNESS_MODEL").ok())
        .context("no --model, no resumed session model, and no ANTHROPIC_MODEL/BRO_HARNESS_MODEL")?;

    let cx = ToolCx {
        root: std::env::current_dir().context("cwd")?,
        safety: Arc::new(SafetyPolicy::new()),
        http: reqwest::Client::new(),
        todos: todos.clone(),
        // Live shell sessions: in-memory, this dispatch only (not persisted).
        shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
    };
    let builtins = builtin_tools();
    // Client-side allow/deny (recursion guard + brofile + per-dispatch),
    // distinct from server-side surface. Denied MCP tools are dropped at load.
    let tool_filter = mcp::ToolFilter::from_csv(cli.deny_tools.as_deref(), cli.allow_tools.as_deref());
    let mcp_tools = mcp::load_mcp_tools(cli.mcp_config.as_deref(), &tool_filter).await;
    let reg = Registry::new(builtins, mcp_tools, &PinPolicy::from_env());

    let base_opts = TurnOpts {
        model,
        max_tokens,
        // Composed per-turn from `system` + tiered tool sections, split into a
        // cache-stable prefix and a volatile tail (see compose_system).
        system: SystemPrompt::default(),
        effort: cli.effort.clone(),
        web_search,
    };

    let emitter = Emitter::new(store.id.clone());
    emitter.system_init();

    let mut total_usage = Usage::default();
    let mut final_text = String::new();
    let mut turns: u64 = 0;

    loop {
        if turns >= max_turns {
            tracing::warn!(max_turns, "hit max turns; stopping");
            break;
        }
        // Rebuild each turn: the wire tool set grows as tool_search activates
        // deferred tools, and the manifest shrinks correspondingly.
        let tool_specs = reg.wire_specs();
        let opts = TurnOpts {
            system: compose_system(system.as_deref(), &reg),
            ..base_opts.clone()
        };
        let out = tx.run_turn(&tool_specs, &opts).await?;
        turns += 1;
        total_usage.add(&out.usage);

        if !out.text.is_empty() {
            emitter.assistant_text(&out.text);
            final_text = out.text;
        }

        if out.stop != StopReason::ToolCalls || out.tool_calls.is_empty() {
            break;
        }

        let mut results = Vec::with_capacity(out.tool_calls.len());
        for tc in out.tool_calls {
            tracing::info!(tool = %tc.name, "dispatch");
            let (content, is_error) = reg.dispatch(&tc.name, tc.args, &cx).await.into_content();
            results.push(transport::ToolResult {
                id: tc.id,
                content,
                is_error,
            });
        }
        tx.push_tool_results(results);
    }

    emitter.result(&final_text, &total_usage, turns, None);
    // Flush loop-level cells back into `side`. Each cell owns its slot; start
    // from the prior blob so cells not yet wired here are preserved.
    let mut side = match prior_side {
        serde_json::Value::Object(m) => serde_json::Value::Object(m),
        _ => serde_json::json!({}),
    };
    side["todos"] = todos.lock().map(|t| t.to_side()).unwrap_or(serde_json::Value::Null);
    store.save(&SaveState {
        transport: tx.name(),
        model: &base_opts.model,
        snapshot: tx.snapshot(),
        side,
    })?;
    Ok(())
}

/// Compose the effective system prompt as a cache-stable prefix plus a
/// volatile tail.
///
/// - **stable** = the caller's system text + the prominent always-available
///   (Pinned) section. Neither changes across a session, so the transport puts
///   the prompt-cache breakpoint here.
/// - **volatile** = the names-only manifest of deferred tools (it shrinks as
///   `tool_search` activates tools) with the instruction to load them. Kept
///   after the breakpoint so a changing tail never invalidates the cached
///   prefix. This is also where ambient nudges will land
///   (design/orchestration/bro-harness-hooks.md §1).
fn compose_system(base: Option<&str>, reg: &Registry) -> SystemPrompt {
    let mut stable = base.unwrap_or("").to_string();

    let pinned = reg.pinned();
    if !pinned.is_empty() {
        if !stable.is_empty() {
            stable.push_str("\n\n");
        }
        stable.push_str(
            "## Always-available tools\n\
             These are loaded and ready — prefer them for their purpose; do not search for them. \
             `tool_search` loads anything else on demand.\n",
        );
        for (name, desc) in pinned {
            stable.push_str(&format!("- `{name}` — {desc}\n"));
        }
    }

    let mut volatile = String::new();
    let manifest = reg.manifest();
    if !manifest.is_empty() {
        volatile.push_str(&format!(
            "## Additional tools ({} available, not yet loaded)\n\
             Call `tool_search(\"<keywords>\")` (or `tool_search(\"select:name1,name2\")`) to load \
             any of these before using them:\n",
            manifest.len()
        ));
        for (name, desc) in manifest {
            volatile.push_str(&format!("- {name}: {desc}\n"));
        }
    }

    SystemPrompt {
        stable: (!stable.is_empty()).then_some(stable),
        volatile: (!volatile.is_empty()).then_some(volatile),
    }
}

fn resolve_prompt(cli: &Cli) -> Result<String> {
    if let Some(p) = &cli.prompt {
        return Ok(p.clone());
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read prompt from stdin")?;
    if buf.trim().is_empty() {
        anyhow::bail!("no prompt: neither -p nor stdin provided one");
    }
    Ok(buf)
}

fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}
