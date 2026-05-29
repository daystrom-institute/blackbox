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
use crate::session::SessionStore;
use crate::mcp;
use crate::transport::{self, StopReason, TransportKind, TurnOpts, Usage};
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
    let model = cli
        .model
        .clone()
        .or_else(|| std::env::var("ANTHROPIC_MODEL").ok())
        .or_else(|| std::env::var("BRO_HARNESS_MODEL").ok())
        .context("no --model and no ANTHROPIC_MODEL/BRO_HARNESS_MODEL")?;
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

    let cx = ToolCx {
        root: std::env::current_dir().context("cwd")?,
        safety: Arc::new(SafetyPolicy::new()),
        http: reqwest::Client::new(),
    };
    let builtins = builtin_tools();
    let mcp_tools = mcp::load_mcp_tools(cli.mcp_config.as_deref()).await;
    let reg = Registry::new(builtins, mcp_tools, &PinPolicy::from_env());

    let base_opts = TurnOpts {
        model,
        max_tokens,
        system: None, // composed per-turn from `system` + tiered tool sections
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
            system: Some(compose_system(system.as_deref(), &reg)),
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
    store.save(tx.name(), tx.snapshot())?;
    Ok(())
}

/// Compose the effective system prompt: the caller's system text, then a
/// prominent always-available (Pinned) section, then the names-only manifest
/// of deferred tools with an instruction to load them via tool_search.
fn compose_system(base: Option<&str>, reg: &Registry) -> String {
    let mut s = base.unwrap_or("").to_string();

    let pinned = reg.pinned();
    if !pinned.is_empty() {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(
            "## Always-available tools\n\
             These are loaded and ready — prefer them for their purpose; do not search for them. \
             `tool_search` loads anything else on demand.\n",
        );
        for (name, desc) in pinned {
            s.push_str(&format!("- `{name}` — {desc}\n"));
        }
    }

    let manifest = reg.manifest();
    if !manifest.is_empty() {
        s.push_str(&format!(
            "\n## Additional tools ({} available, not yet loaded)\n\
             Call `tool_search(\"<keywords>\")` (or `tool_search(\"select:name1,name2\")`) to load \
             any of these before using them:\n",
            manifest.len()
        ));
        for (name, desc) in manifest {
            s.push_str(&format!("- {name}: {desc}\n"));
        }
    }

    s
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
