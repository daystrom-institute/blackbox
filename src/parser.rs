// Multi-bin crate: blackboxd uses the basic parser API (ParsedEvent,
// parse_transcript_line, parse_codex_line, ...) via src/index/*;
// bro uses the rich parser API (TranscriptEvent, *_rich functions)
// via src/cli.rs. Each binary sees the other half as dead.
#![allow(dead_code)]

use serde_json::Value;

/// The role associated with a single transcript event. All provider
/// formats (Claude Code, Codex CLI, history.jsonl) normalize into this
/// fixed set; parsers that encounter a role outside this set return no
/// event rather than inventing one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumString, strum::AsRefStr, strum::Display,
)]
#[strum(serialize_all = "snake_case")]
pub enum MessageRole {
    /// Human-typed input.
    User,
    /// Model output.
    Assistant,
    /// Model reasoning / thinking blocks.
    Thinking,
    /// Tool invocation by the model.
    ToolUse,
    /// Tool output returned to the model.
    ToolResult,
    /// System / developer context (Codex-specific).
    Developer,
}

/// A single searchable unit extracted from a JSONL transcript event.
#[derive(Debug, Clone)]
pub struct ParsedEvent {
    pub role: MessageRole,
    pub content: String,
    pub session_id: String,
    pub timestamp: Option<String>,
    pub git_branch: Option<String>,
    pub is_subagent: bool,
    pub agent_slug: Option<String>,
    pub cwd: Option<String>,
    pub tool_call: Option<ToolCallInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallKind {
    Read,
    Write,
    Edit,
    Bash,
}

#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub kind: ToolCallKind,
    pub name: String,
    pub tool_use_id: Option<String>,
    pub input: Value,
}

const MAX_CONTENT_LEN: usize = 12_000;

// ═══════════════════════════════════════════════════════════════════════
// Rich transcript event model — consumed by the `bro tail` TUI.
// Indexing stays on ParsedEvent via TranscriptEvent::to_parsed().
// ═══════════════════════════════════════════════════════════════════════

/// Richer, structured transcript event. Preserves tool-call structure,
/// thinking/text separation, and out-of-band signals (compaction, hooks,
/// system-reminders, etc.) that inform *why* the agent is reasoning.
#[derive(Debug, Clone)]
pub struct TranscriptEvent {
    pub role: MessageRole,
    pub session_id: String,
    pub timestamp: Option<String>,
    pub git_branch: Option<String>,
    pub is_subagent: bool,
    pub agent_slug: Option<String>,
    pub cwd: Option<String>,
    pub parent_tool_use_id: Option<String>,
    pub detail: EventDetail,
}

#[derive(Debug, Clone)]
pub enum EventDetail {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        name: String,
        target: String,
        tool_use_id: Option<String>,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        is_error: bool,
        exit_code: Option<i32>,
        size: usize,
        preview: String,
    },
    SystemSignal {
        kind: SystemSignalKind,
        summary: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSignalKind {
    SessionInit,
    SessionResumed,
    Compaction,
    HookFired,
    SystemReminder,
    PermissionDenied,
    RateLimitHit,
    UserCommand,
    SubagentLaunched,
    Other,
}

impl TranscriptEvent {
    /// Project onto the flat ParsedEvent shape used by the tantivy indexer.
    /// SystemSignal events are not indexed by default.
    pub fn to_parsed(&self) -> Option<ParsedEvent> {
        let content = match &self.detail {
            EventDetail::Text { text } => truncate(text),
            EventDetail::Thinking { text } => truncate(text),
            EventDetail::ToolUse { name, input, .. } => {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                format!("tool:{} {}", name, truncate(&input_str))
            }
            EventDetail::ToolResult {
                tool_use_id,
                preview,
                ..
            } => {
                format!("result:{} {}", tool_use_id, truncate(preview))
            }
            EventDetail::SystemSignal { .. } => return None,
        };
        Some(ParsedEvent {
            role: self.role,
            content,
            session_id: self.session_id.clone(),
            timestamp: self.timestamp.clone(),
            git_branch: self.git_branch.clone(),
            is_subagent: self.is_subagent,
            agent_slug: self.agent_slug.clone(),
            cwd: self.cwd.clone(),
            tool_call: match &self.detail {
                EventDetail::ToolUse {
                    name,
                    tool_use_id,
                    input,
                    ..
                } => tool_call_info(name, tool_use_id.clone(), input.clone()),
                _ => None,
            },
        })
    }
}

pub fn tool_call_info(
    name: &str,
    tool_use_id: Option<String>,
    input: Value,
) -> Option<ToolCallInfo> {
    let kind = match name {
        "Read" | "read" => ToolCallKind::Read,
        "Write" | "write" => ToolCallKind::Write,
        "Edit" | "edit" => ToolCallKind::Edit,
        "Bash" | "bash" | "shell" => ToolCallKind::Bash,
        _ => return None,
    };
    Some(ToolCallInfo {
        kind,
        name: name.to_string(),
        tool_use_id,
        input,
    })
}

pub fn tool_call_file_path(info: &ToolCallInfo) -> Option<&str> {
    match info.kind {
        ToolCallKind::Read | ToolCallKind::Write | ToolCallKind::Edit => info
            .input
            .get("file_path")
            .or_else(|| info.input.get("path"))
            .and_then(|value| value.as_str()),
        ToolCallKind::Bash => None,
    }
}

pub fn tool_call_command(info: &ToolCallInfo) -> Option<&str> {
    if info.kind != ToolCallKind::Bash {
        return None;
    }
    info.input.get("command").and_then(|value| value.as_str())
}

/// Best-effort extraction of a human-readable "what is this tool doing"
/// string from a tool-use input payload. Per-tool rules fall back to the
/// first non-empty string-valued field.
pub fn extract_tool_target(tool_name: &str, input: &Value) -> String {
    let explicit: Option<&str> = match tool_name {
        "Bash" | "bash" | "shell" => input.get("command").and_then(|v| v.as_str()),
        "Read" | "Write" | "Edit" | "read" | "write" | "edit" => {
            input.get("file_path").and_then(|v| v.as_str())
        }
        "NotebookEdit" => input.get("notebook_path").and_then(|v| v.as_str()),
        "Grep" | "grep" => input.get("pattern").and_then(|v| v.as_str()),
        "Glob" | "glob" => input.get("pattern").and_then(|v| v.as_str()),
        "WebFetch" => input.get("url").and_then(|v| v.as_str()),
        "WebSearch" => input.get("query").and_then(|v| v.as_str()),
        "Task" | "TaskCreate" => input
            .get("description")
            .or_else(|| input.get("subject"))
            .and_then(|v| v.as_str()),
        "TaskUpdate" => input.get("taskId").and_then(|v| v.as_str()),
        "ScheduleWakeup" => input.get("reason").and_then(|v| v.as_str()),
        "Skill" => input.get("skill").and_then(|v| v.as_str()),
        "ToolSearch" => input.get("query").and_then(|v| v.as_str()),
        _ => None,
    };
    if let Some(s) = explicit {
        return oneline_snippet(s, 160);
    }
    if let Some(obj) = input.as_object() {
        for (_k, v) in obj {
            if let Some(s) = v.as_str()
                && !s.is_empty() {
                    return oneline_snippet(s, 160);
                }
        }
    }
    String::new()
}

/// Split Gemini assistant content on inlined `[Thought: true]` / `[Thought:true]`
/// reasoning markers. Each segment before a marker is a thought; the final
/// segment (after the last marker, or the whole string if no markers) is the
/// answer. Returns at least one segment.
fn split_gemini_thought_segments(content: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("[Thought:") {
        if let Some(close_rel) = rest[start..].find(']') {
            parts.push(&rest[..start]);
            rest = &rest[start + close_rel + 1..];
        } else {
            break;
        }
    }
    parts.push(rest);
    parts
}

fn oneline_snippet(s: &str, max_chars: usize) -> String {
    let one = s.replace('\n', " ");
    let count = one.chars().count();
    if count <= max_chars {
        one
    } else {
        let mut out: String = one.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// Best-effort exit code extraction from a Bash-style tool result.
/// Returns None if not present or not parseable.
fn extract_exit_code(text: &str) -> Option<i32> {
    // Match "exit code <N>", "exit_code=<N>", "(exit code <N>)"
    let lower = text.to_ascii_lowercase();
    let marker = lower
        .find("exit code")
        .or_else(|| lower.find("exit_code"))?;
    let rest = &lower[marker..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '-')
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

struct RichBase {
    session_id: String,
    timestamp: Option<String>,
    git_branch: Option<String>,
    is_subagent: bool,
    agent_slug: Option<String>,
    cwd: Option<String>,
    parent_tool_use_id: Option<String>,
}

fn make_rich(role: MessageRole, detail: EventDetail, base: &RichBase) -> TranscriptEvent {
    TranscriptEvent {
        role,
        session_id: base.session_id.clone(),
        timestamp: base.timestamp.clone(),
        git_branch: base.git_branch.clone(),
        is_subagent: base.is_subagent,
        agent_slug: base.agent_slug.clone(),
        cwd: base.cwd.clone(),
        parent_tool_use_id: base.parent_tool_use_id.clone(),
        detail,
    }
}

/// Detect steering signals buried inside a "user" text message:
/// `<system-reminder>` injections and `<command-name>` slash commands.
fn detect_user_signal(text: &str) -> Option<SystemSignalKind> {
    if text.contains("<system-reminder>") {
        return Some(SystemSignalKind::SystemReminder);
    }
    if text.contains("<command-name>") || text.contains("<local-command-stdout>") {
        return Some(SystemSignalKind::UserCommand);
    }
    None
}

// ── Rich parser: Claude Code JSONL ──────────────────────────────────

pub fn parse_transcript_line_rich(line: &str) -> Vec<TranscriptEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let event_type = match v["type"].as_str() {
        Some(t) => t,
        None => return vec![],
    };
    let base = RichBase {
        session_id: v["sessionId"].as_str().unwrap_or("").to_string(),
        timestamp: v["timestamp"].as_str().map(String::from),
        git_branch: v["gitBranch"].as_str().map(String::from),
        is_subagent: v
            .get("isSidechain")
            .and_then(|x| x.as_bool())
            .unwrap_or(false)
            || v.get("agentId").is_some(),
        agent_slug: v["slug"].as_str().map(String::from),
        cwd: v["cwd"].as_str().map(String::from),
        parent_tool_use_id: v["parentUuid"].as_str().map(String::from),
    };
    match event_type {
        "user" => parse_user_message_rich(&v["message"], &base),
        "assistant" => parse_assistant_message_rich(&v["message"], &base),
        "system" => parse_system_event_rich(&v, &base),
        "summary" => vec![make_rich(
            MessageRole::Developer,
            EventDetail::SystemSignal {
                kind: SystemSignalKind::Compaction,
                summary: v["summary"]
                    .as_str()
                    .unwrap_or("context compacted")
                    .to_string(),
            },
            &base,
        )],
        _ => vec![],
    }
}

fn parse_user_message_rich(message: &Value, base: &RichBase) -> Vec<TranscriptEvent> {
    match &message["content"] {
        Value::String(s) if !s.is_empty() => {
            if let Some(kind) = detect_user_signal(s) {
                return vec![make_rich(
                    MessageRole::User,
                    EventDetail::SystemSignal {
                        kind,
                        summary: s.clone(),
                    },
                    base,
                )];
            }
            vec![make_rich(
                MessageRole::User,
                EventDetail::Text { text: s.clone() },
                base,
            )]
        }
        Value::Array(blocks) => {
            let mut out = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = block["text"].as_str()
                            && !text.is_empty() {
                                if let Some(kind) = detect_user_signal(text) {
                                    out.push(make_rich(
                                        MessageRole::User,
                                        EventDetail::SystemSignal {
                                            kind,
                                            summary: text.into(),
                                        },
                                        base,
                                    ));
                                } else {
                                    out.push(make_rich(
                                        MessageRole::User,
                                        EventDetail::Text { text: text.into() },
                                        base,
                                    ));
                                }
                            }
                    }
                    Some("tool_result") => {
                        let tool_use_id = block["tool_use_id"].as_str().unwrap_or("?").to_string();
                        let is_error = block["is_error"].as_bool().unwrap_or(false);
                        let text = extract_tool_result_text(block).unwrap_or_default();
                        let size = text.len();
                        let preview = oneline_snippet(&text, 200);
                        let exit_code = extract_exit_code(&text);
                        out.push(make_rich(
                            MessageRole::ToolResult,
                            EventDetail::ToolResult {
                                tool_use_id,
                                is_error,
                                exit_code,
                                size,
                                preview,
                            },
                            base,
                        ));
                    }
                    _ => {}
                }
            }
            out
        }
        _ => vec![],
    }
}

fn parse_assistant_message_rich(message: &Value, base: &RichBase) -> Vec<TranscriptEvent> {
    let blocks = match message["content"].as_array() {
        Some(b) => b,
        None => return vec![],
    };
    let mut out = Vec::new();
    for block in blocks {
        let block_type = match block["type"].as_str() {
            Some(t) => t,
            None => continue,
        };
        match block_type {
            "text" => {
                if let Some(text) = block["text"].as_str()
                    && !text.is_empty() {
                        out.push(make_rich(
                            MessageRole::Assistant,
                            EventDetail::Text { text: text.into() },
                            base,
                        ));
                    }
            }
            "thinking" => {
                if let Some(t) = block["thinking"].as_str()
                    && !t.is_empty() {
                        out.push(make_rich(
                            MessageRole::Thinking,
                            EventDetail::Thinking { text: t.into() },
                            base,
                        ));
                    }
            }
            "tool_use" => {
                let name = block["name"].as_str().unwrap_or("unknown").to_string();
                let tool_use_id = block["id"].as_str().map(String::from);
                let input = block["input"].clone();
                let target = extract_tool_target(&name, &input);
                out.push(make_rich(
                    MessageRole::ToolUse,
                    EventDetail::ToolUse {
                        name,
                        target,
                        tool_use_id,
                        input,
                    },
                    base,
                ));
            }
            _ => {}
        }
    }
    out
}

fn parse_system_event_rich(v: &Value, base: &RichBase) -> Vec<TranscriptEvent> {
    let subtype = v["subtype"].as_str().unwrap_or("");
    match subtype {
        "init" => {
            let model = v["model"].as_str().unwrap_or("?");
            let cwd = v["cwd"].as_str().unwrap_or("?");
            let tools = v["tools"].as_array().map(|a| a.len()).unwrap_or(0);
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal {
                    kind: SystemSignalKind::SessionInit,
                    summary: format!("session init: {model} in {cwd} ({tools} tools)"),
                },
                base,
            )]
        }
        "hook_started" | "hook_response" => {
            let hook = v["hook_name"].as_str().unwrap_or("?");
            let outcome = v["outcome"].as_str().unwrap_or("");
            let summary = if outcome.is_empty() {
                format!("hook {subtype}: {hook}")
            } else {
                format!("hook {hook}: {outcome}")
            };
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal {
                    kind: SystemSignalKind::HookFired,
                    summary,
                },
                base,
            )]
        }
        _ => vec![],
    }
}

// ── Rich parser: Codex CLI JSONL ────────────────────────────────────

pub fn parse_codex_line_rich(line: &str, session_id: &str) -> Vec<TranscriptEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let event_type = match v["type"].as_str() {
        Some(t) => t,
        None => return vec![],
    };
    let timestamp = v["timestamp"].as_str().map(String::from);

    match event_type {
        "session_meta" => {
            let cwd = v["payload"]["cwd"].as_str().map(String::from);
            let model = v["payload"]["model"].as_str().unwrap_or("?");
            let base = RichBase {
                session_id: session_id.into(),
                timestamp,
                git_branch: None,
                is_subagent: false,
                agent_slug: None,
                cwd,
                parent_tool_use_id: None,
            };
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal {
                    kind: SystemSignalKind::SessionInit,
                    summary: format!("codex session: {model}"),
                },
                &base,
            )]
        }
        "response_item" => {
            let payload = &v["payload"];
            let base = RichBase {
                session_id: session_id.into(),
                timestamp,
                git_branch: None,
                is_subagent: false,
                agent_slug: None,
                cwd: None,
                parent_tool_use_id: None,
            };
            // Codex response items dispatch on payload.type: `message` carries
            // role+content, while `function_call`, `function_call_output`, and
            // `reasoning` are top-level items with no role field.
            match payload["type"].as_str().unwrap_or("") {
                "message" => {
                    let role = match payload["role"].as_str() {
                        Some("user") => MessageRole::User,
                        Some("assistant") => MessageRole::Assistant,
                        Some("developer") => MessageRole::Developer,
                        _ => return vec![],
                    };
                    parse_codex_content_rich(payload, role, &base)
                }
                "function_call" => {
                    let name = payload["name"].as_str().unwrap_or("unknown").to_string();
                    let tool_use_id = payload["call_id"].as_str().map(String::from);
                    let args_str = payload["arguments"].as_str().unwrap_or("{}");
                    let input: Value = serde_json::from_str(args_str).unwrap_or(Value::Null);
                    let target = extract_tool_target(&name, &input);
                    vec![make_rich(
                        MessageRole::ToolUse,
                        EventDetail::ToolUse {
                            name,
                            target,
                            tool_use_id,
                            input,
                        },
                        &base,
                    )]
                }
                "function_call_output" => {
                    let tool_use_id = payload["call_id"].as_str().unwrap_or("?").to_string();
                    let output = payload["output"].as_str().unwrap_or("");
                    let size = output.len();
                    let preview = oneline_snippet(output, 200);
                    let exit_code = extract_exit_code(output);
                    vec![make_rich(
                        MessageRole::ToolResult,
                        EventDetail::ToolResult {
                            tool_use_id,
                            is_error: false,
                            exit_code,
                            size,
                            preview,
                        },
                        &base,
                    )]
                }
                "reasoning" => {
                    // Codex reasoning content is usually encrypted; surface
                    // readable text when summary/content carries it.
                    let text = payload["summary"][0]["text"]
                        .as_str()
                        .or_else(|| payload["content"][0]["text"].as_str())
                        .or_else(|| payload["text"].as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        vec![]
                    } else {
                        vec![make_rich(
                            MessageRole::Thinking,
                            EventDetail::Thinking { text: text.into() },
                            &base,
                        )]
                    }
                }
                _ => vec![],
            }
        }
        _ => vec![],
    }
}

fn parse_codex_content_rich(
    payload: &Value,
    role: MessageRole,
    base: &RichBase,
) -> Vec<TranscriptEvent> {
    let content = match payload["content"].as_array() {
        Some(c) => c,
        None => {
            if let Some(s) = payload["content"].as_str()
                && !s.is_empty() {
                    return vec![make_rich(role, EventDetail::Text { text: s.into() }, base)];
                }
            return vec![];
        }
    };
    let mut out = Vec::new();
    for block in content {
        let block_type = block["type"].as_str().unwrap_or("");
        match block_type {
            "input_text" | "output_text" => {
                if let Some(text) = block["text"].as_str()
                    && !text.is_empty() {
                        if let Some(kind) = detect_user_signal(text) {
                            out.push(make_rich(
                                role,
                                EventDetail::SystemSignal {
                                    kind,
                                    summary: text.into(),
                                },
                                base,
                            ));
                        } else {
                            out.push(make_rich(
                                role,
                                EventDetail::Text { text: text.into() },
                                base,
                            ));
                        }
                    }
            }
            "function_call" => {
                let name = block["name"].as_str().unwrap_or("unknown").to_string();
                let tool_use_id = block["call_id"].as_str().map(String::from);
                let args_str = block["arguments"].as_str().unwrap_or("{}");
                let input: Value = serde_json::from_str(args_str).unwrap_or(Value::Null);
                let target = extract_tool_target(&name, &input);
                out.push(make_rich(
                    MessageRole::ToolUse,
                    EventDetail::ToolUse {
                        name,
                        target,
                        tool_use_id,
                        input,
                    },
                    base,
                ));
            }
            "function_call_output" => {
                let tool_use_id = block["call_id"].as_str().unwrap_or("?").to_string();
                let output = block["output"].as_str().unwrap_or("");
                let size = output.len();
                let preview = oneline_snippet(output, 200);
                let exit_code = extract_exit_code(output);
                out.push(make_rich(
                    MessageRole::ToolResult,
                    EventDetail::ToolResult {
                        tool_use_id,
                        is_error: false,
                        exit_code,
                        size,
                        preview,
                    },
                    base,
                ));
            }
            "reasoning" => {
                if let Some(text) = block["text"].as_str()
                    && !text.is_empty() {
                        out.push(make_rich(
                            MessageRole::Thinking,
                            EventDetail::Thinking { text: text.into() },
                            base,
                        ));
                    }
            }
            _ => {}
        }
    }
    out
}

// ── Rich parser: Copilot events JSONL ───────────────────────────────

/// Parse one line of `~/.copilot/session-state/<id>/events.jsonl`.
/// Event shape: `{type, data, id, timestamp, parentId}` with types like
/// `session.start`, `user.message`, `assistant.message`,
/// `tool.execution_start`, `tool.execution_complete`, etc.
pub fn parse_copilot_line_rich(line: &str, session_id: &str) -> Vec<TranscriptEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let event_type = match v["type"].as_str() {
        Some(t) => t,
        None => return vec![],
    };
    let timestamp = v["timestamp"].as_str().map(String::from);
    let data = &v["data"];
    let base = RichBase {
        session_id: session_id.into(),
        timestamp,
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: None,
        parent_tool_use_id: v["parentId"].as_str().map(String::from),
    };
    match event_type {
        "session.start" => {
            let model = data["model"].as_str().unwrap_or("?");
            let cwd = data["cwd"].as_str().unwrap_or("?");
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal {
                    kind: SystemSignalKind::SessionInit,
                    summary: format!("copilot session: {model} in {cwd}"),
                },
                &base,
            )]
        }
        "session.model_change" => {
            let from = data["fromModel"].as_str().unwrap_or("?");
            let to = data["toModel"].as_str().unwrap_or("?");
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal {
                    kind: SystemSignalKind::Other,
                    summary: format!("model: {from} → {to}"),
                },
                &base,
            )]
        }
        "user.message" => {
            let content = data["content"].as_str().unwrap_or("");
            if content.is_empty() {
                return vec![];
            }
            if let Some(kind) = detect_user_signal(content) {
                return vec![make_rich(
                    MessageRole::User,
                    EventDetail::SystemSignal {
                        kind,
                        summary: content.into(),
                    },
                    &base,
                )];
            }
            vec![make_rich(
                MessageRole::User,
                EventDetail::Text {
                    text: content.into(),
                },
                &base,
            )]
        }
        "assistant.message" => {
            let mut out = Vec::new();
            if let Some(r) = data["reasoningText"].as_str()
                && !r.is_empty() {
                    out.push(make_rich(
                        MessageRole::Thinking,
                        EventDetail::Thinking { text: r.into() },
                        &base,
                    ));
                }
            let content = data["content"].as_str().unwrap_or("");
            if !content.is_empty() {
                out.push(make_rich(
                    MessageRole::Assistant,
                    EventDetail::Text {
                        text: content.into(),
                    },
                    &base,
                ));
            }
            out
        }
        "tool.execution_start" => {
            let name = data["toolName"].as_str().unwrap_or("unknown").to_string();
            let tool_use_id = data["toolCallId"].as_str().map(String::from);
            let input = data["arguments"].clone();
            let target = extract_tool_target(&name, &input);
            vec![make_rich(
                MessageRole::ToolUse,
                EventDetail::ToolUse {
                    name,
                    target,
                    tool_use_id,
                    input,
                },
                &base,
            )]
        }
        "tool.execution_complete" => {
            let tool_use_id = data["toolCallId"].as_str().unwrap_or("?").to_string();
            let success = data["success"].as_bool().unwrap_or(true);
            // result can be string OR { content, detailedContent }
            let (preview, size) = match &data["result"] {
                Value::String(s) => (oneline_snippet(s, 200), s.len()),
                Value::Object(_) => {
                    let s = data["result"]["content"].as_str().unwrap_or("");
                    (oneline_snippet(s, 200), s.len())
                }
                _ => (String::new(), 0),
            };
            vec![make_rich(
                MessageRole::ToolResult,
                EventDetail::ToolResult {
                    tool_use_id,
                    is_error: !success,
                    exit_code: None,
                    size,
                    preview,
                },
                &base,
            )]
        }
        "session.warning" => {
            let msg = data["message"].as_str().unwrap_or("warning");
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal {
                    kind: SystemSignalKind::Other,
                    summary: msg.into(),
                },
                &base,
            )]
        }
        _ => vec![], // turn_start / turn_end / shutdown — no display value
    }
}

// ── Rich parser: Vibe messages JSONL (OpenAI-style chat shape) ──────

/// Parse one line of `~/.vibe/logs/session/.../messages.jsonl`.
/// Shape follows OpenAI chat-completion: `{role, content, tool_calls?,
/// name?, tool_call_id?, injected?, message_id}`.
pub fn parse_vibe_line_rich(line: &str, session_id: &str) -> Vec<TranscriptEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let role = v["role"].as_str().unwrap_or("");
    let message_id = v["message_id"].as_str().map(String::from);
    let injected = v["injected"].as_bool().unwrap_or(false);
    let base = RichBase {
        session_id: session_id.into(),
        timestamp: None,
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: None,
        parent_tool_use_id: message_id,
    };

    match role {
        "user" => {
            let content = v["content"].as_str().unwrap_or("");
            if content.is_empty() {
                return vec![];
            }
            if injected {
                return vec![make_rich(
                    MessageRole::User,
                    EventDetail::SystemSignal {
                        kind: SystemSignalKind::SystemReminder,
                        summary: content.into(),
                    },
                    &base,
                )];
            }
            if let Some(kind) = detect_user_signal(content) {
                return vec![make_rich(
                    MessageRole::User,
                    EventDetail::SystemSignal {
                        kind,
                        summary: content.into(),
                    },
                    &base,
                )];
            }
            vec![make_rich(
                MessageRole::User,
                EventDetail::Text {
                    text: content.into(),
                },
                &base,
            )]
        }
        "assistant" => {
            let mut out = Vec::new();
            let content = v["content"].as_str().unwrap_or("");
            if !content.is_empty() {
                out.push(make_rich(
                    MessageRole::Assistant,
                    EventDetail::Text {
                        text: content.into(),
                    },
                    &base,
                ));
            }
            if let Some(calls) = v["tool_calls"].as_array() {
                for call in calls {
                    let name = call["function"]["name"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();
                    let tool_use_id = call["id"].as_str().map(String::from);
                    let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let input: Value = serde_json::from_str(args_str).unwrap_or(Value::Null);
                    let target = extract_tool_target(&name, &input);
                    out.push(make_rich(
                        MessageRole::ToolUse,
                        EventDetail::ToolUse {
                            name,
                            target,
                            tool_use_id,
                            input,
                        },
                        &base,
                    ));
                }
            }
            out
        }
        "tool" => {
            let tool_use_id = v["tool_call_id"].as_str().unwrap_or("?").to_string();
            let content = v["content"].as_str().unwrap_or("");
            let size = content.len();
            let preview = oneline_snippet(content, 200);
            let exit_code = extract_exit_code(content);
            // Vibe doesn't carry an explicit is_error flag — infer from common
            // failure words; conservative default is success.
            let lower = content.to_ascii_lowercase();
            let is_error = lower.starts_with("error")
                || lower.contains("traceback")
                || lower.starts_with("failed");
            vec![make_rich(
                MessageRole::ToolResult,
                EventDetail::ToolResult {
                    tool_use_id,
                    is_error,
                    exit_code,
                    size,
                    preview,
                },
                &base,
            )]
        }
        _ => vec![],
    }
}

// ── Rich parser: Gemini chat JSON (single-object, not JSONL) ────────

/// Parse a full Gemini chat-session JSON file. Gemini stores sessions as
/// pretty-printed JSON objects under `~/.gemini/tmp/<project>/chats/`
/// rather than JSONL; callers re-invoke this on file mtime change and
/// filter out already-rendered messages by `id`.
pub fn parse_gemini_file_rich(raw: &str) -> Vec<TranscriptEvent> {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let session_id = v["sessionId"].as_str().unwrap_or("").to_string();
    let messages = match v["messages"].as_array() {
        Some(m) => m,
        None => return vec![],
    };
    let mut out = Vec::new();
    for msg in messages {
        let timestamp = msg["timestamp"].as_str().map(String::from);
        let msg_id = msg["id"].as_str().unwrap_or("").to_string();
        let base = RichBase {
            session_id: session_id.clone(),
            timestamp,
            git_branch: None,
            is_subagent: false,
            agent_slug: None,
            cwd: None,
            parent_tool_use_id: Some(msg_id),
        };
        let msg_type = msg["type"].as_str().unwrap_or("");
        match msg_type {
            "user" => {
                // content is either a string or an array of { text }
                if let Some(s) = msg["content"].as_str() {
                    if !s.is_empty() {
                        out.push(make_rich(
                            MessageRole::User,
                            EventDetail::Text { text: s.into() },
                            &base,
                        ));
                    }
                } else if let Some(arr) = msg["content"].as_array() {
                    for block in arr {
                        if let Some(text) = block["text"].as_str()
                            && !text.is_empty() {
                                out.push(make_rich(
                                    MessageRole::User,
                                    EventDetail::Text { text: text.into() },
                                    &base,
                                ));
                            }
                    }
                }
            }
            "gemini" => {
                // Thoughts (reasoning) first, then content, then any tool calls.
                if let Some(thoughts) = msg["thoughts"].as_array() {
                    for t in thoughts {
                        let subject = t["subject"].as_str().unwrap_or("");
                        let description = t["description"].as_str().unwrap_or("");
                        let text = if subject.is_empty() {
                            description.to_string()
                        } else {
                            format!("{subject}\n{description}")
                        };
                        if !text.is_empty() {
                            out.push(make_rich(
                                MessageRole::Thinking,
                                EventDetail::Thinking { text },
                                &base,
                            ));
                        }
                    }
                }
                if let Some(s) = msg["content"].as_str()
                    && !s.is_empty() {
                        // Gemini sometimes inlines reasoning into `content`
                        // delimited by `[Thought: true]` markers: each
                        // segment before a marker is a thought, and anything
                        // after the final marker is the assistant's answer.
                        let segments = split_gemini_thought_segments(s);
                        let last = segments.len().saturating_sub(1);
                        for (i, seg) in segments.iter().enumerate() {
                            let trimmed = seg.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if i == last {
                                out.push(make_rich(
                                    MessageRole::Assistant,
                                    EventDetail::Text {
                                        text: trimmed.into(),
                                    },
                                    &base,
                                ));
                            } else {
                                out.push(make_rich(
                                    MessageRole::Thinking,
                                    EventDetail::Thinking {
                                        text: trimmed.into(),
                                    },
                                    &base,
                                ));
                            }
                        }
                    }
                // Gemini packs call + result together: each toolCalls entry has
                // id/name/args plus a result[].functionResponse.response blob
                // and a status (success|error|cancelled). Emit a ToolUse/
                // ToolResult pair per call so counters and tail rendering match
                // the other providers.
                if let Some(calls) = msg["toolCalls"].as_array() {
                    for call in calls {
                        let name = call["name"].as_str().unwrap_or("unknown").to_string();
                        let tool_use_id = call["id"].as_str().map(String::from);
                        let input = call["args"].clone();
                        let target = extract_tool_target(&name, &input);
                        out.push(make_rich(
                            MessageRole::ToolUse,
                            EventDetail::ToolUse {
                                name: name.clone(),
                                target,
                                tool_use_id: tool_use_id.clone(),
                                input,
                            },
                            &base,
                        ));
                        let status = call["status"].as_str().unwrap_or("success");
                        let is_error = status != "success";
                        let response = call["result"][0]["functionResponse"]["response"].clone();
                        let output = if let Some(s) = response["output"].as_str() {
                            s.to_string()
                        } else if let Some(s) = response["error"].as_str() {
                            s.to_string()
                        } else if response.is_null() {
                            String::new()
                        } else {
                            response.to_string()
                        };
                        let size = output.len();
                        let preview = oneline_snippet(&output, 200);
                        let exit_code = extract_exit_code(&output);
                        out.push(make_rich(
                            MessageRole::ToolResult,
                            EventDetail::ToolResult {
                                tool_use_id: tool_use_id.unwrap_or_else(|| "?".into()),
                                is_error,
                                exit_code,
                                size,
                                preview,
                            },
                            &base,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Parse a single JSONL line into zero or more searchable events.
pub fn parse_transcript_line(line: &str) -> Vec<ParsedEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let event_type = match v["type"].as_str() {
        Some(t) => t,
        None => return vec![],
    };

    let session_id = v["sessionId"].as_str().unwrap_or("").to_string();
    let timestamp = v["timestamp"].as_str().map(String::from);
    let git_branch = v["gitBranch"].as_str().map(String::from);
    let is_subagent = v
        .get("isSidechain")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || v.get("agentId").is_some();
    let agent_slug = v["slug"].as_str().map(String::from);
    let cwd = v["cwd"].as_str().map(String::from);

    let base = EventBase {
        session_id,
        timestamp,
        git_branch,
        is_subagent,
        agent_slug,
        cwd,
    };

    match event_type {
        "user" => parse_user_message(&v["message"], &base),
        "assistant" => parse_assistant_message(&v["message"], &base),
        _ => vec![],
    }
}

/// Parse a history.jsonl line (different schema: {display, project, sessionId, timestamp}).
pub fn parse_history_line(line: &str) -> Vec<ParsedEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let display = match v["display"].as_str() {
        Some(d) if !d.is_empty() => d,
        _ => return vec![],
    };

    let session_id = v["sessionId"].as_str().unwrap_or("").to_string();
    let project = v["project"].as_str().map(String::from);

    // Timestamp is epoch millis (number), convert to ISO-ish string
    let timestamp = v["timestamp"].as_u64().map(|ms| {
        let secs = ms / 1000;
        format!("{}", secs)
    });

    vec![ParsedEvent {
        role: MessageRole::User,
        content: truncate(display),
        session_id,
        timestamp,
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: project,
        tool_call: None,
    }]
}

struct EventBase {
    session_id: String,
    timestamp: Option<String>,
    git_branch: Option<String>,
    is_subagent: bool,
    agent_slug: Option<String>,
    cwd: Option<String>,
}

fn parse_user_message(message: &Value, base: &EventBase) -> Vec<ParsedEvent> {
    match &message["content"] {
        // String content = human-typed message
        Value::String(s) if !s.is_empty() => {
            vec![make_event(MessageRole::User, &truncate(s), base)]
        }
        // Array content = tool results (sent as user role in Claude API)
        Value::Array(blocks) => {
            let mut events = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = block["text"].as_str()
                            && !text.is_empty() {
                                events.push(make_event(MessageRole::User, &truncate(text), base));
                            }
                    }
                    Some("tool_result") => {
                        if let Some(text) = extract_tool_result_text(block)
                            && !text.is_empty() {
                                let tool_id = block["tool_use_id"].as_str().unwrap_or("?");
                                let content = format!("result:{} {}", tool_id, truncate(&text));
                                events.push(make_event(MessageRole::ToolResult, &content, base));
                            }
                    }
                    _ => {}
                }
            }
            events
        }
        _ => vec![],
    }
}

fn parse_assistant_message(message: &Value, base: &EventBase) -> Vec<ParsedEvent> {
    let blocks = match message["content"].as_array() {
        Some(b) => b,
        None => return vec![],
    };

    let mut events = Vec::new();
    for block in blocks {
        let block_type = match block["type"].as_str() {
            Some(t) => t,
            None => continue,
        };

        match block_type {
            "text" => {
                if let Some(text) = block["text"].as_str()
                    && !text.is_empty() {
                        events.push(make_event(MessageRole::Assistant, &truncate(text), base));
                    }
            }
            "thinking" => {
                if let Some(thinking) = block["thinking"].as_str()
                    && !thinking.is_empty() {
                        events.push(make_event(MessageRole::Thinking, &truncate(thinking), base));
                    }
            }
            "tool_use" => {
                let tool_name = block["name"].as_str().unwrap_or("unknown");
                let input_str = serde_json::to_string(&block["input"]).unwrap_or_default();
                let content = format!("tool:{} {}", tool_name, truncate(&input_str));
                events.push(make_tool_event(
                    MessageRole::ToolUse,
                    &content,
                    base,
                    tool_call_info(
                        tool_name,
                        block["id"].as_str().map(String::from),
                        block["input"].clone(),
                    ),
                ));
            }
            _ => {}
        }
    }
    events
}

pub fn extract_tool_result_text(block: &Value) -> Option<String> {
    match &block["content"] {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

// ── Codex CLI parser ────────────────────────────────────────────────

/// Parse a Codex CLI JSONL line: {timestamp, type, payload}.
/// Session ID is NOT per-line in Codex — caller must supply it from session_meta or filename.
pub fn parse_codex_line(line: &str, session_id: &str) -> Vec<ParsedEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let event_type = match v["type"].as_str() {
        Some(t) => t,
        None => return vec![],
    };

    let timestamp = v["timestamp"].as_str().map(String::from);

    match event_type {
        "session_meta" => {
            // Extract cwd from session metadata — useful for project association
            let cwd = v["payload"]["cwd"].as_str().map(String::from);
            let base_instructions = v["payload"]["base_instructions"].as_str().unwrap_or("");
            if base_instructions.is_empty() {
                return vec![];
            }
            let base = EventBase {
                session_id: session_id.to_string(),
                timestamp,
                git_branch: None,
                is_subagent: false,
                agent_slug: None,
                cwd,
            };
            vec![make_event(
                MessageRole::Developer,
                &truncate(base_instructions),
                &base,
            )]
        }
        "response_item" => {
            let payload = &v["payload"];
            let base = EventBase {
                session_id: session_id.to_string(),
                timestamp,
                git_branch: None,
                is_subagent: false,
                agent_slug: None,
                cwd: None,
            };
            // Codex response items dispatch on payload.type: `message` carries
            // role+content, while `function_call`, `function_call_output`, and
            // `reasoning` are top-level items with no role field.
            match payload["type"].as_str().unwrap_or("") {
                "message" => {
                    let role = match payload["role"].as_str() {
                        Some("user") => MessageRole::User,
                        Some("assistant") => MessageRole::Assistant,
                        Some("developer") => MessageRole::Developer,
                        _ => return vec![],
                    };
                    parse_codex_content_blocks(payload, role, &base)
                }
                "function_call" => {
                    let name = payload["name"].as_str().unwrap_or("unknown");
                    let args = payload["arguments"].as_str().unwrap_or("{}");
                    vec![make_tool_event(
                        MessageRole::ToolUse,
                        &truncate(&format!("tool:{} {}", name, args)),
                        &base,
                        tool_call_info(
                            name,
                            payload["call_id"].as_str().map(String::from),
                            serde_json::from_str(args).unwrap_or(Value::Null),
                        ),
                    )]
                }
                "function_call_output" => {
                    let output = payload["output"].as_str().unwrap_or("");
                    if output.is_empty() {
                        vec![]
                    } else {
                        vec![make_event(
                            MessageRole::ToolResult,
                            &truncate(&format!("result: {}", output)),
                            &base,
                        )]
                    }
                }
                "reasoning" => {
                    let text = payload["summary"][0]["text"]
                        .as_str()
                        .or_else(|| payload["content"][0]["text"].as_str())
                        .or_else(|| payload["text"].as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        vec![]
                    } else {
                        vec![make_event(MessageRole::Thinking, &truncate(text), &base)]
                    }
                }
                _ => vec![],
            }
        }
        _ => vec![], // Skip event_msg, turn_context, etc.
    }
}

/// Parse Codex history.jsonl: {session_id, ts, text}
pub fn parse_codex_history_line(line: &str) -> Vec<ParsedEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let text = match v["text"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return vec![],
    };

    let session_id = v["session_id"].as_str().unwrap_or("").to_string();
    let timestamp = v["ts"].as_u64().map(|s| format!("{}", s));

    vec![ParsedEvent {
        role: MessageRole::User,
        content: truncate(text),
        session_id,
        timestamp,
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: None,
        tool_call: None,
    }]
}

fn parse_codex_content_blocks(
    payload: &Value,
    role: MessageRole,
    base: &EventBase,
) -> Vec<ParsedEvent> {
    let content = match payload["content"].as_array() {
        Some(c) => c,
        None => {
            // Sometimes content is a string directly
            if let Some(s) = payload["content"].as_str()
                && !s.is_empty() {
                    return vec![make_event(role, &truncate(s), base)];
                }
            return vec![];
        }
    };

    let mut events = Vec::new();
    for block in content {
        let block_type = block["type"].as_str().unwrap_or("");
        match block_type {
            "input_text" | "output_text" => {
                if let Some(text) = block["text"].as_str()
                    && !text.is_empty() {
                        events.push(make_event(role, &truncate(text), base));
                    }
            }
            "function_call" => {
                let name = block["name"].as_str().unwrap_or("unknown");
                let args = block["arguments"].as_str().unwrap_or("{}");
                let content = format!("tool:{} {}", name, truncate(args));
                events.push(make_event(MessageRole::ToolUse, &content, base));
            }
            "function_call_output" => {
                let output = block["output"].as_str().unwrap_or("");
                if !output.is_empty() {
                    let content = format!("result: {}", truncate(output));
                    events.push(make_event(MessageRole::ToolResult, &content, base));
                }
            }
            "reasoning" => {
                // Codex reasoning/thinking — map to "thinking"
                if let Some(text) = block["text"].as_str()
                    && !text.is_empty() {
                        events.push(make_event(MessageRole::Thinking, &truncate(text), base));
                    }
            }
            _ => {}
        }
    }
    events
}

fn make_event(role: MessageRole, content: &str, base: &EventBase) -> ParsedEvent {
    make_tool_event(role, content, base, None)
}

fn make_tool_event(
    role: MessageRole,
    content: &str,
    base: &EventBase,
    tool_call: Option<ToolCallInfo>,
) -> ParsedEvent {
    ParsedEvent {
        role,
        content: content.to_string(),
        session_id: base.session_id.clone(),
        timestamp: base.timestamp.clone(),
        git_branch: base.git_branch.clone(),
        is_subagent: base.is_subagent,
        agent_slug: base.agent_slug.clone(),
        cwd: base.cwd.clone(),
        tool_call,
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_CONTENT_LEN {
        s.to_string()
    } else {
        let mut end = MAX_CONTENT_LEN;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...[truncated]", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Rich parser sanity ──────────────────────────────────────────

    #[test]
    fn test_rich_claude_tool_use_has_target() {
        let line = json!({
            "type": "assistant",
            "sessionId": "s1",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t1", "name": "Bash",
                      "input": { "command": "git status --short" } }
                ]
            }
        })
        .to_string();
        let events = parse_transcript_line_rich(&line);
        assert_eq!(events.len(), 1);
        match &events[0].detail {
            EventDetail::ToolUse { name, target, .. } => {
                assert_eq!(name, "Bash");
                assert_eq!(target, "git status --short");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_rich_claude_system_reminder_detected() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "message": { "role": "user",
                "content": "<system-reminder>reset</system-reminder>" }
        })
        .to_string();
        let events = parse_transcript_line_rich(&line);
        assert!(matches!(
            events[0].detail,
            EventDetail::SystemSignal {
                kind: SystemSignalKind::SystemReminder,
                ..
            }
        ));
    }

    #[test]
    fn test_rich_copilot_tool_execution() {
        let line = json!({
            "type": "tool.execution_start",
            "timestamp": "2026-04-16T20:00:00Z",
            "data": {
                "toolCallId": "call_1",
                "toolName": "Bash",
                "arguments": { "command": "ls" }
            }
        })
        .to_string();
        let events = parse_copilot_line_rich(&line, "sess-cop-1");
        assert_eq!(events.len(), 1);
        match &events[0].detail {
            EventDetail::ToolUse {
                name,
                target,
                tool_use_id,
                ..
            } => {
                assert_eq!(name, "Bash");
                assert_eq!(target, "ls");
                assert_eq!(tool_use_id.as_deref(), Some("call_1"));
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_rich_copilot_tool_complete_failure() {
        let line = json!({
            "type": "tool.execution_complete",
            "data": {
                "toolCallId": "call_1",
                "success": false,
                "result": { "content": "permission denied", "detailedContent": "x" }
            }
        })
        .to_string();
        let events = parse_copilot_line_rich(&line, "s");
        match &events[0].detail {
            EventDetail::ToolResult {
                is_error, preview, ..
            } => {
                assert!(*is_error);
                assert_eq!(preview, "permission denied");
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn test_rich_vibe_assistant_with_tool_calls() {
        let line = json!({
            "role": "assistant",
            "content": "Calling shell",
            "message_id": "m1",
            "tool_calls": [{
                "id": "call_1",
                "function": {
                    "name": "shell",
                    "arguments": "{\"command\": \"echo hi\"}"
                }
            }]
        })
        .to_string();
        let events = parse_vibe_line_rich(&line, "sess-vibe-1");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].detail, EventDetail::Text { .. }));
        match &events[1].detail {
            EventDetail::ToolUse { name, target, .. } => {
                assert_eq!(name, "shell");
                assert_eq!(target, "echo hi");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_rich_vibe_tool_result_error_inference() {
        let line = json!({
            "role": "tool",
            "content": "Error: something broke",
            "tool_call_id": "call_1",
            "name": "shell"
        })
        .to_string();
        let events = parse_vibe_line_rich(&line, "s");
        match &events[0].detail {
            EventDetail::ToolResult { is_error, .. } => assert!(*is_error),
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn test_rich_gemini_thoughts_to_thinking() {
        let raw = json!({
            "sessionId": "g1",
            "messages": [
                {
                    "id": "m1",
                    "timestamp": "t",
                    "type": "gemini",
                    "content": "final answer",
                    "thoughts": [
                        {"subject": "Step 1", "description": "Considering options"}
                    ]
                }
            ]
        })
        .to_string();
        let events = parse_gemini_file_rich(&raw);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].detail, EventDetail::Thinking { .. }));
        assert!(matches!(events[1].detail, EventDetail::Text { .. }));
    }

    #[test]
    fn test_rich_gemini_inline_thoughts() {
        let raw = json!({
            "sessionId": "g1",
            "messages": [{
                "id": "m1",
                "timestamp": "t",
                "type": "gemini",
                "content": "**Step 1** thinking about problem[Thought: true]**Step 2** more thinking[Thought:true]Here is the final answer."
            }]
        }).to_string();
        let events = parse_gemini_file_rich(&raw);
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0].detail, EventDetail::Thinking { .. }));
        assert!(matches!(events[1].detail, EventDetail::Thinking { .. }));
        match &events[2].detail {
            EventDetail::Text { text } => assert_eq!(text, "Here is the final answer."),
            _ => panic!("expected trailing Text"),
        }
    }

    #[test]
    fn test_rich_gemini_no_inline_thoughts() {
        // Plain content with no markers should still produce a single Text event.
        let raw = json!({
            "sessionId": "g1",
            "messages": [{
                "id": "m1", "timestamp": "t", "type": "gemini",
                "content": "just the answer"
            }]
        })
        .to_string();
        let events = parse_gemini_file_rich(&raw);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].detail, EventDetail::Text { .. }));
    }

    #[test]
    fn claude_tool_use_preserves_tool_call_payload() {
        let line = serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "Edit",
                    "input": {
                        "file_path": "/tmp/demo.rs",
                        "old_string": "before",
                        "new_string": "after"
                    }
                }]
            }
        })
        .to_string();

        let events = parse_transcript_line(&line);

        assert_eq!(events.len(), 1);
        let tool_call = events[0].tool_call.as_ref().unwrap();
        assert_eq!(tool_call.kind, ToolCallKind::Edit);
        assert_eq!(tool_call.tool_use_id.as_deref(), Some("toolu_1"));
        assert_eq!(tool_call_file_path(tool_call), Some("/tmp/demo.rs"));
    }

    #[test]
    fn codex_function_call_preserves_tool_call_payload() {
        let line = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Bash",
                "arguments": "{\"command\":\"cargo test\"}"
            }
        })
        .to_string();

        let events = parse_codex_line(&line, "sess-2");

        assert_eq!(events.len(), 1);
        let tool_call = events[0].tool_call.as_ref().unwrap();
        assert_eq!(tool_call.kind, ToolCallKind::Bash);
        assert_eq!(tool_call.tool_use_id.as_deref(), Some("call_1"));
        assert_eq!(tool_call_command(tool_call), Some("cargo test"));
    }

    #[test]
    fn test_rich_gemini_tool_calls() {
        let raw = json!({
            "sessionId": "g1",
            "messages": [{
                "id": "m1",
                "timestamp": "t",
                "type": "gemini",
                "content": "",
                "toolCalls": [
                    {
                        "id": "c1",
                        "name": "read_file",
                        "args": {"path": "foo.rs"},
                        "status": "success",
                        "result": [{"functionResponse": {"response": {"output": "file contents"}}}]
                    },
                    {
                        "id": "c2",
                        "name": "read_file",
                        "args": {"path": "nope.rs"},
                        "status": "error",
                        "result": [{"functionResponse": {"response": {"error": "File not found"}}}]
                    }
                ]
            }]
        })
        .to_string();
        let events = parse_gemini_file_rich(&raw);
        assert_eq!(events.len(), 4);
        match &events[0].detail {
            EventDetail::ToolUse {
                name,
                target,
                tool_use_id,
                ..
            } => {
                assert_eq!(name, "read_file");
                assert_eq!(target, "foo.rs");
                assert_eq!(tool_use_id.as_deref(), Some("c1"));
            }
            _ => panic!("expected ToolUse"),
        }
        match &events[1].detail {
            EventDetail::ToolResult {
                is_error,
                preview,
                tool_use_id,
                ..
            } => {
                assert!(!is_error);
                assert_eq!(preview, "file contents");
                assert_eq!(tool_use_id, "c1");
            }
            _ => panic!("expected ToolResult"),
        }
        match &events[3].detail {
            EventDetail::ToolResult {
                is_error, preview, ..
            } => {
                assert!(is_error);
                assert_eq!(preview, "File not found");
            }
            _ => panic!("expected error ToolResult"),
        }
    }

    #[test]
    fn test_extract_tool_target_fallback() {
        let v = json!({ "some_random_field": "meaningful value" });
        assert_eq!(extract_tool_target("UnknownTool", &v), "meaningful value");
    }

    // ── Legacy ParsedEvent parser tests ─────────────────────────────

    #[test]
    fn test_parse_user_string_message() {
        let line = json!({
            "type": "user",
            "sessionId": "session-123",
            "timestamp": "2026-04-15T12:00:00Z",
            "message": {
                "role": "user",
                "content": "Hello world"
            }
        })
        .to_string();

        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role, MessageRole::User);
        assert_eq!(events[0].content, "Hello world");
        assert_eq!(events[0].session_id, "session-123");
    }

    #[test]
    fn test_parse_user_tool_result() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "Analyzing..." },
                    { "type": "tool_result", "tool_use_id": "call_1", "content": "Success" }
                ]
            }
        })
        .to_string();

        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].role, MessageRole::User);
        assert_eq!(events[0].content, "Analyzing...");
        assert_eq!(events[1].role, MessageRole::ToolResult);
        assert!(events[1].content.contains("result:call_1 Success"));
    }

    #[test]
    fn test_parse_assistant_message() {
        let line = json!({
            "type": "assistant",
            "sessionId": "s1",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "I should say hello" },
                    { "type": "text", "text": "Hello!" },
                    { "type": "tool_use", "name": "read_file", "input": {"path": "foo.rs"}, "id": "t1" }
                ]
            }
        }).to_string();

        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].role, MessageRole::Thinking);
        assert_eq!(events[0].content, "I should say hello");
        assert_eq!(events[1].role, MessageRole::Assistant);
        assert_eq!(events[1].content, "Hello!");
        assert_eq!(events[2].role, MessageRole::ToolUse);
        assert!(events[2].content.contains("tool:read_file"));
        assert!(events[2].content.contains("foo.rs"));
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_transcript_line("not json").is_empty());
        assert!(parse_transcript_line("{}").is_empty());
    }

    #[test]
    fn test_parse_metadata() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "timestamp": "ts",
            "gitBranch": "main",
            "cwd": "/repo",
            "message": { "content": "hi" }
        })
        .to_string();

        let events = parse_transcript_line(&line);
        assert_eq!(events[0].session_id, "s1");
        assert_eq!(events[0].timestamp, Some("ts".to_string()));
        assert_eq!(events[0].git_branch, Some("main".to_string()));
        assert_eq!(events[0].cwd, Some("/repo".to_string()));
        assert!(!events[0].is_subagent);
    }

    #[test]
    fn test_parse_subagent() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "isSidechain": true,
            "slug": "researcher",
            "message": { "content": "hi" }
        })
        .to_string();

        let events = parse_transcript_line(&line);
        assert!(events[0].is_subagent);
        assert_eq!(events[0].agent_slug, Some("researcher".to_string()));
    }

    #[test]
    fn test_parse_history_line() {
        let line = json!({
            "display": "ls command",
            "sessionId": "s1",
            "project": "/p",
            "timestamp": 1600000000000u64
        })
        .to_string();

        let events = parse_history_line(&line);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "ls command");
        assert_eq!(events[0].role, MessageRole::User);
        assert_eq!(events[0].cwd, Some("/p".to_string()));
    }

    #[test]
    fn test_parse_codex_line() {
        let meta = json!({
            "type": "session_meta",
            "payload": { "cwd": "/repo", "base_instructions": "Be helpful" }
        })
        .to_string();
        let ev1 = parse_codex_line(&meta, "s1");
        assert_eq!(ev1.len(), 1);
        assert_eq!(ev1[0].role, MessageRole::Developer);
        assert_eq!(ev1[0].content, "Be helpful");

        // assistant text message
        let msg = json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "Done" }]
            }
        })
        .to_string();
        let ev_msg = parse_codex_line(&msg, "s1");
        assert_eq!(ev_msg.len(), 1);
        assert_eq!(ev_msg[0].role, MessageRole::Assistant);

        // function_call is a top-level response_item with no role
        let call = json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "ls",
                "arguments": "{\"path\": \".\"}",
                "call_id": "c1"
            }
        })
        .to_string();
        let ev_call = parse_codex_line(&call, "s1");
        assert_eq!(ev_call.len(), 1);
        assert_eq!(ev_call[0].role, MessageRole::ToolUse);
        assert!(ev_call[0].content.contains("tool:ls"));
    }

    #[test]
    fn test_extract_tool_result_text() {
        let b1 = json!({ "content": "direct string" });
        assert_eq!(
            extract_tool_result_text(&b1),
            Some("direct string".to_string())
        );

        let b2 = json!({ "content": [ { "text": "part1" }, { "text": "part2" } ] });
        assert_eq!(
            extract_tool_result_text(&b2),
            Some("part1\npart2".to_string())
        );

        let b3 = json!({ "content": [] });
        assert_eq!(extract_tool_result_text(&b3), None);
    }

    #[test]
    fn test_truncate() {
        let short = "abc";
        assert_eq!(truncate(short), "abc");

        let long = "a".repeat(13000);
        let tr = truncate(&long);
        assert!(tr.ends_with("...[truncated]"));
        assert!(tr.len() < 13000);

        // UTF-8 boundary test (Emoji is 4 bytes: 🦀 = \u{1F980})
        let emoji_repeated = "🦀".repeat(4000);
        let tr_emoji = truncate(&emoji_repeated);
        assert!(tr_emoji.ends_with("...[truncated]"));
        // Ensure it doesn't panic and result is valid string
        assert!(!tr_emoji.is_empty());
    }

    #[test]
    fn test_parse_codex_history_line() {
        let line = json!({
            "session_id": "s1",
            "ts": 1600000000,
            "text": "Hello Codex"
        })
        .to_string();
        let events = parse_codex_history_line(&line);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "Hello Codex");
        assert_eq!(events[0].timestamp, Some("1600000000".to_string()));
    }

    #[test]
    fn test_parse_codex_direct_string_content() {
        let line = json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": "direct text"
            }
        })
        .to_string();
        let events = parse_codex_line(&line, "s1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "direct text");
    }

    #[test]
    fn test_parse_codex_reasoning() {
        // Codex reasoning is a top-level response_item. Live sessions usually
        // carry encrypted content with empty summary/content; we surface
        // readable text only when it exists.
        let with_text = json!({
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": "Thinking hard" }]
            }
        })
        .to_string();
        let events = parse_codex_line(&with_text, "s1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role, MessageRole::Thinking);
        assert_eq!(events[0].content, "Thinking hard");

        let encrypted_only = json!({
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "summary": [],
                "content": null,
                "encrypted_content": "gAAAAA..."
            }
        })
        .to_string();
        assert!(parse_codex_line(&encrypted_only, "s1").is_empty());
    }

    #[test]
    fn test_parse_codex_function_call_output() {
        let line = json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "c1",
                "output": "Tool result"
            }
        })
        .to_string();
        let events = parse_codex_line(&line, "s1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role, MessageRole::ToolResult);
        assert!(events[0].content.contains("result: Tool result"));
    }

    #[test]
    fn test_parse_assistant_no_content() {
        let line = json!({
            "type": "assistant",
            "sessionId": "s1",
            "message": { "role": "assistant" }
        })
        .to_string();
        assert!(parse_transcript_line(&line).is_empty());
    }

    #[test]
    fn test_parse_user_array_text() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "Part 1" },
                    { "type": "text", "text": "Part 2" }
                ]
            }
        })
        .to_string();
        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content, "Part 1");
        assert_eq!(events[1].content, "Part 2");
    }

    #[test]
    fn test_parse_assistant_tool_use_truncation() {
        let long_input = "a".repeat(15000);
        let line = json!({
            "type": "assistant",
            "sessionId": "s1",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "name": "foo", "input": {"data": long_input} }
                ]
            }
        })
        .to_string();
        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 1);
        assert!(events[0].content.contains("...[truncated]"));
    }
}
