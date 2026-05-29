//! The clipboard: a session-durable, snapshot register store for the harness
//! tool loop, plus the `clip_*` tools that produce and consume registers.
//!
//! A **register** is a named, server-held value cell. An agent yanks a text
//! slice into a register and pastes it elsewhere without the content ever
//! transiting the model context — the entire point. `yank`/`paste`/`list`/`set`
//! return hashes + counts + a short `preview_head`; the full content only
//! leaves via an explicit, bounded `clip_peek`.
//!
//! This is the **settled-ref layer** (Stage 1) of the broader ref ABI: the same
//! [`Registers`] cell is the substrate other tools read/write for general
//! tool→tool chaining (Stage 2 — see `file_read{into}`, `file_write{from}`,
//! `shell_run{stdout_to,stdin_from}`). A `Task` would be the *pending*-ref
//! specialization (Stage 3, not built). See
//! `design/orchestration/bro-harness-{clipboard,tool-chaining}.md`.
//!
//! The store rides the session `side` cell exactly like `todos`/`nudges`, so it
//! survives `exec → resume`. Because the session file is fully rewritten every
//! turn, registers are capped by total bytes and count, LRU-evicting at write
//! time (never silently — evictions are surfaced to the caller).

use crate::slice_core::{
    InsertSelector, SliceRangeSelector, insert_from_value, resolve_insert, resolve_slice,
    sha256_hex, slice_range_from_value,
};
use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use crate::workspace::resolve_in_root;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Total bytes the clipboard may hold across all registers. Beyond this, the
/// least-recently-touched registers are evicted at write time. Kept modest
/// because the whole store is serialized into the session file every turn.
const MAX_TOTAL_BYTES: usize = 256 * 1024;
/// Max number of live registers; LRU-evicted past this.
const MAX_REGISTERS: usize = 64;
/// Default register when the caller omits one — the "unnamed" register, like
/// vim's `"`.
pub const DEFAULT_REGISTER: &str = "@";
/// Lines included in a `preview_head` — a deliberately tiny, bounded egress so
/// `list`/`yank`/`set` can show *what* a register holds without leaking it.
const PREVIEW_LINES: usize = 3;
const PREVIEW_MAX_BYTES: usize = 240;

/// The content kind a register holds. The "typed" in "typed ref": a consumer
/// can refuse a register whose kind it can't accept. All current kinds carry
/// UTF-8 text; `Bytes`/`Json` are reserved for later non-text producers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// Literal text (`clip_set`) or a text producer's output.
    Text,
    /// A slice extracted from a file (`clip_yank`, `file_read{into}`).
    FileSlice,
    /// Captured stdout / generic tool output (`shell_run{stdout_to}`).
    ToolResult,
}

impl RefKind {
    /// Every current kind is UTF-8 text, so every register is paste-able and
    /// stdin-feedable. The method exists so non-text kinds added later refuse
    /// text consumers instead of silently stringifying.
    fn is_text(self) -> bool {
        matches!(
            self,
            RefKind::Text | RefKind::FileSlice | RefKind::ToolResult
        )
    }
}

/// Where a `FileSlice` register came from, for provenance in `clip_list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub path: String,
    /// Source file hash at capture time — lets a later paste/diff detect drift.
    /// Empty when the producer streamed the file (`file_read{into}`) and never
    /// hashed the whole thing.
    pub file_sha256: String,
    /// Human summary of the selected range, e.g. `"lines 10-20"`.
    pub range: String,
}

/// One settled ref.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Register {
    pub kind: RefKind,
    /// The snapshot. Copied in at yank/set time; paste never re-reads source.
    pub text: String,
    pub slice_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Monotonic touch stamp for LRU. Higher = more recently written.
    pub touched: u64,
}

impl Register {
    fn preview_head(&self) -> String {
        let mut head: String = self
            .text
            .lines()
            .take(PREVIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        if head.len() > PREVIEW_MAX_BYTES {
            // Truncate on a char boundary.
            let mut end = PREVIEW_MAX_BYTES;
            while end > 0 && !head.is_char_boundary(end) {
                end -= 1;
            }
            head.truncate(end);
            head.push('…');
        } else if self.text.lines().count() > PREVIEW_LINES {
            head.push('…');
        }
        head
    }

    fn line_count(&self) -> usize {
        if self.text.is_empty() {
            0
        } else {
            self.text.lines().count()
        }
    }

    /// Metadata-only view — hashes, counts, preview. Never the full content.
    fn meta(&self, name: &str) -> Value {
        json!({
            "register": name,
            "kind": self.kind,
            "byte_len": self.text.len(),
            "line_count": self.line_count(),
            "slice_sha256": self.slice_sha256,
            "provenance": self.provenance,
            "preview_head": self.preview_head(),
        })
    }
}

/// The register store. Keyed by register name (`"@"`, `"a"`, …).
#[derive(Debug, Clone, Default)]
pub struct Registers {
    map: BTreeMap<String, Register>,
    clock: u64,
}

impl Registers {
    /// Restore from the `side["clipboard"]` blob. Tolerant: absent/garbage →
    /// empty, so old session files and partial blobs resume cleanly.
    pub fn from_side(v: &Value) -> Self {
        let map: BTreeMap<String, Register> = v
            .get("registers")
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default();
        let clock = map.values().map(|r| r.touched).max().unwrap_or(0);
        Self { map, clock }
    }

    /// Serialize back into the `side` cell.
    pub fn to_side(&self) -> Value {
        json!({ "registers": self.map })
    }

    fn total_bytes(&self) -> usize {
        self.map.values().map(|r| r.text.len()).sum()
    }

    fn next_clock(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Evict least-recently-touched registers until the store would fit `incoming`
    /// bytes within the caps, never evicting `keep` (the register being written).
    /// Returns the evicted names so the caller can surface them (no silent drop).
    fn evict_to_fit(&mut self, incoming: usize, keep: &str) -> Vec<String> {
        let mut evicted = Vec::new();
        // Order candidates oldest-first.
        let mut order: Vec<(u64, String)> = self
            .map
            .iter()
            .filter(|(name, _)| name.as_str() != keep)
            .map(|(name, r)| (r.touched, name.clone()))
            .collect();
        order.sort_by_key(|(touched, _)| *touched);

        let mut idx = 0;
        while idx < order.len() {
            let kept_existing = self.map.get(keep).map(|r| r.text.len()).unwrap_or(0);
            let over_bytes = self.total_bytes() - kept_existing + incoming > MAX_TOTAL_BYTES;
            // +1 for the incoming register if it does not already exist.
            let projected_count = self.map.len() + usize::from(!self.map.contains_key(keep));
            let over_count = projected_count > MAX_REGISTERS;
            if !over_bytes && !over_count {
                break;
            }
            let (_, name) = &order[idx];
            self.map.remove(name);
            evicted.push(name.clone());
            idx += 1;
        }
        evicted
    }

    /// Write a register (replacing or, with `append`, accumulating). Returns
    /// the evicted register names. Errors if the content alone exceeds the cap.
    fn put(
        &mut self,
        name: &str,
        kind: RefKind,
        text: String,
        provenance: Option<Provenance>,
        append: bool,
    ) -> anyhow::Result<Vec<String>> {
        let combined = if append {
            match self.map.get(name) {
                Some(prev) => format!("{}{}", prev.text, text),
                None => text,
            }
        } else {
            text
        };
        if combined.len() > MAX_TOTAL_BYTES {
            anyhow::bail!(
                "register content is {} bytes, over the {MAX_TOTAL_BYTES}-byte clipboard cap",
                combined.len()
            );
        }
        let evicted = self.evict_to_fit(combined.len(), name);
        let touched = self.next_clock();
        let slice_sha256 = sha256_hex(combined.as_bytes());
        self.map.insert(
            name.to_string(),
            Register {
                kind,
                text: combined,
                slice_sha256,
                provenance,
                touched,
            },
        );
        Ok(evicted)
    }

    /// Read a register and refresh its LRU stamp.
    fn touch_get(&mut self, name: &str) -> Option<&Register> {
        let stamp = self.next_clock();
        if let Some(r) = self.map.get_mut(name) {
            r.touched = stamp;
            return Some(&*r);
        }
        None
    }

    fn get(&self, name: &str) -> Option<&Register> {
        self.map.get(name)
    }

    // --- ref ABI: the chaining surface other tools (file_read{into},
    // shell_run{stdout_to}, file_write{from}, …) read/write through. ---

    /// Store a file slice into a register (file_read{into}). Replaces.
    pub fn put_slice(
        &mut self,
        name: &str,
        text: String,
        provenance: Option<Provenance>,
    ) -> anyhow::Result<Vec<String>> {
        self.put(name, RefKind::FileSlice, text, provenance, false)
    }

    /// Store captured tool output into a register (shell_run{stdout_to}).
    pub fn put_tool_result(&mut self, name: &str, text: String) -> anyhow::Result<Vec<String>> {
        self.put(name, RefKind::ToolResult, text, None, false)
    }

    /// Metadata view of a register (hashes/counts/preview), never the content.
    pub fn meta_of(&self, name: &str) -> Option<Value> {
        self.map.get(name).map(|r| r.meta(name))
    }

    /// Read a text register for a consuming tool (file_write{from},
    /// shell_run{stdin_from}). Touches LRU; errors if absent or non-text.
    pub fn consume_text(&mut self, name: &str) -> Result<String, String> {
        match self.touch_get(name) {
            Some(r) if r.kind.is_text() => Ok(r.text.clone()),
            Some(_) => Err(format!(
                "register '{name}' holds non-text content; this tool needs a text register"
            )),
            None => Err(format!("register '{name}' is empty")),
        }
    }
}

// ---------------------------------------------------------------------------
// clip_yank — extract a file slice into a register (no content returned)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ClipYankInput {
    /// File to yank from, relative to the worktree root.
    source: String,
    /// Range to extract. JSON object (or JSON string) with a `type` of
    /// `lines` | `markers` | `exact_text` | `bytes`, mirroring bbox_slice_read.
    source_range: Value,
    /// Register to store into. Default `"@"`.
    #[serde(default)]
    register: Option<String>,
    /// Append to the register instead of replacing it (gather scattered slices
    /// into one block). Default false.
    #[serde(default)]
    append: bool,
}

pub struct ClipYank;

#[async_trait]
impl Tool for ClipYank {
    fn name(&self) -> &str {
        "clip_yank"
    }
    fn description(&self) -> &str {
        "Copy a slice of a file into a named clipboard register WITHOUT returning its content to you — the bytes stay server-side so they never cost context tokens. Use this instead of file_read when the goal is to move/duplicate text rather than reason about it. source_range selects by lines/markers/exact_text/bytes (same vocabulary as bbox_slice_read). Set append=true to gather multiple slices into one register. Returns the register name, kind, hashes, byte/line counts, and a short preview_head. Paste it elsewhere with clip_paste; inspect it with clip_peek."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ClipYankInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ClipYankInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let path = match resolve_in_root(&cx.root, &args.source) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let source = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => return ToolResult::Error(format!("read {}: {e}", args.source)),
        };
        let selector: SliceRangeSelector = match slice_range_from_value(args.source_range) {
            Ok(s) => s,
            Err(e) => return ToolResult::Error(format!("bad source_range: {e}")),
        };
        let slice = match resolve_slice(&source, &selector) {
            Ok(s) => s,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let register = args
            .register
            .unwrap_or_else(|| DEFAULT_REGISTER.to_string());
        let provenance = Some(Provenance {
            path: args.source.clone(),
            file_sha256: sha256_hex(source.as_bytes()),
            range: format!("lines {}-{}", slice.line_start, slice.line_end),
        });

        let mut clip = cx.clipboard.lock().unwrap();
        let evicted = match clip.put(
            &register,
            RefKind::FileSlice,
            slice.text,
            provenance,
            args.append,
        ) {
            Ok(e) => e,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let mut out = clip.get(&register).unwrap().meta(&register);
        annotate_evicted(&mut out, evicted);
        ToolResult::Json(out)
    }
}

// ---------------------------------------------------------------------------
// clip_set — stuff literal text into a register (templating, scratch)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ClipSetInput {
    /// Register to write. Default `"@"`.
    #[serde(default)]
    register: Option<String>,
    /// Literal text to store.
    text: String,
    /// Append instead of replace. Default false.
    #[serde(default)]
    append: bool,
}

pub struct ClipSet;

#[async_trait]
impl Tool for ClipSet {
    fn name(&self) -> &str {
        "clip_set"
    }
    fn description(&self) -> &str {
        "Store literal text into a clipboard register (templating, assembling a block to paste). Returns metadata + a short preview_head, not the stored text. Set append=true to accumulate."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ClipSetInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ClipSetInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let register = args
            .register
            .unwrap_or_else(|| DEFAULT_REGISTER.to_string());
        let mut clip = cx.clipboard.lock().unwrap();
        let evicted = match clip.put(&register, RefKind::Text, args.text, None, args.append) {
            Ok(e) => e,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let mut out = clip.get(&register).unwrap().meta(&register);
        annotate_evicted(&mut out, evicted);
        ToolResult::Json(out)
    }
}

// ---------------------------------------------------------------------------
// clip_paste — insert a register's content into a file (dry-run by default)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ClipPasteInput {
    /// Target file, relative to the worktree root. Created if absent.
    target: String,
    /// Where to insert. JSON object (or string) with a `type` of
    /// `line` | `before_marker` | `after_marker` | `prepend` | `append`.
    insert: Value,
    /// Register to paste from. Default `"@"`.
    #[serde(default)]
    register: Option<String>,
    /// Paste the register `count` times (fan-out within one target). Default 1.
    #[serde(default)]
    count: Option<usize>,
    /// Must be true to write. When omitted/false, returns a dry-run plan.
    #[serde(default)]
    confirm: bool,
    /// If set, refuse the write unless the target's current content hashes to
    /// this (drift guard for confirmed writes against a concurrent edit).
    #[serde(default)]
    expected_sha256: Option<String>,
}

pub struct ClipPaste;

#[async_trait]
impl Tool for ClipPaste {
    fn name(&self) -> &str {
        "clip_paste"
    }
    fn description(&self) -> &str {
        "Insert a clipboard register's content into a target file at an insertion point, WITHOUT the content passing through your context. insert selects line/before_marker/after_marker/prepend/append. Dry-run by default (returns the resolved insertion point, byte offset, and resulting sha256); pass confirm=true to write. count pastes the register N times. expected_sha256 refuses a confirmed write if the target drifted. The target is created if absent (prepend/append only)."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ClipPasteInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ClipPasteInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let register = args
            .register
            .clone()
            .unwrap_or_else(|| DEFAULT_REGISTER.to_string());
        let count = args.count.unwrap_or(1).max(1);

        // Pull the register snapshot (touch LRU).
        let payload = {
            let mut clip = cx.clipboard.lock().unwrap();
            match clip.touch_get(&register) {
                Some(r) if r.kind.is_text() => r.text.repeat(count),
                Some(_) => {
                    return ToolResult::Error(format!(
                        "register '{register}' holds non-text content; clip_paste needs a text register"
                    ));
                }
                None => return ToolResult::Error(format!("register '{register}' is empty")),
            }
        };

        let path = match resolve_in_root(&cx.root, &args.target) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let selector: InsertSelector = match insert_from_value(args.insert) {
            Ok(s) => s,
            Err(e) => return ToolResult::Error(format!("bad insert: {e}")),
        };
        let at = match resolve_insert(&existing, &selector) {
            Ok(off) => off,
            Err(e) => return ToolResult::Error(e.to_string()),
        };

        let mut updated = String::with_capacity(existing.len() + payload.len());
        updated.push_str(&existing[..at]);
        updated.push_str(&payload);
        updated.push_str(&existing[at..]);
        let new_sha = sha256_hex(updated.as_bytes());
        let (line, _) = (
            existing[..at].bytes().filter(|b| *b == b'\n').count() + 1,
            0,
        );

        if !args.confirm {
            return ToolResult::Json(json!({
                "status": "dry_run",
                "target": args.target,
                "register": register,
                "insert_byte_offset": at,
                "insert_line": line,
                "paste_byte_len": payload.len(),
                "original_sha256": sha256_hex(existing.as_bytes()),
                "new_sha256": new_sha,
                "hint": "re-call with confirm=true to write",
            }));
        }

        if let Some(expected) = &args.expected_sha256 {
            let actual = sha256_hex(existing.as_bytes());
            if &actual != expected {
                return ToolResult::Error(format!(
                    "target sha256 {actual} != expected {expected}; file drifted, re-read before pasting"
                ));
            }
        }
        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult::Error(format!("mkdir {}: {e}", parent.display()));
        }
        match tokio::fs::write(&path, updated.as_bytes()).await {
            Ok(()) => ToolResult::Json(json!({
                "status": "written",
                "target": args.target,
                "register": register,
                "insert_byte_offset": at,
                "paste_byte_len": payload.len(),
                "new_sha256": new_sha,
            })),
            Err(e) => ToolResult::Error(format!("write {}: {e}", args.target)),
        }
    }
}

// ---------------------------------------------------------------------------
// clip_list / clip_peek / clip_clear
// ---------------------------------------------------------------------------

pub struct ClipList;

#[async_trait]
impl Tool for ClipList {
    fn name(&self) -> &str {
        "clip_list"
    }
    fn description(&self) -> &str {
        "List clipboard registers with kind, byte/line counts, provenance, sha256, and a short preview_head. Does not return full content — use clip_peek for that."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, _input: Value, cx: &ToolCx) -> ToolResult {
        let clip = cx.clipboard.lock().unwrap();
        let registers: Vec<Value> = clip.map.iter().map(|(name, r)| r.meta(name)).collect();
        ToolResult::Json(json!({
            "registers": registers,
            "total_bytes": clip.total_bytes(),
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
struct ClipPeekInput {
    /// Register to read. Default `"@"`.
    #[serde(default)]
    register: Option<String>,
    /// Cap on lines returned. Omit for the whole register.
    #[serde(default)]
    max_lines: Option<usize>,
}

pub struct ClipPeek;

#[async_trait]
impl Tool for ClipPeek {
    fn name(&self) -> &str {
        "clip_peek"
    }
    fn description(&self) -> &str {
        "Return a clipboard register's actual content (bounded by max_lines). This is the ONLY way content leaves the clipboard into your context, so call it deliberately — usually you want to clip_paste, not peek."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ClipPeekInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ClipPeekInput = serde_json::from_value(input).unwrap_or(ClipPeekInput {
            register: None,
            max_lines: None,
        });
        let register = args
            .register
            .unwrap_or_else(|| DEFAULT_REGISTER.to_string());
        let clip = cx.clipboard.lock().unwrap();
        let Some(r) = clip.get(&register) else {
            return ToolResult::Error(format!("register '{register}' is empty"));
        };
        let text = match args.max_lines {
            Some(n) => {
                let kept: Vec<&str> = r.text.lines().take(n).collect();
                let mut s = kept.join("\n");
                if r.text.lines().count() > n {
                    s.push_str(&format!("\n[truncated at max_lines={n}]"));
                }
                s
            }
            None => r.text.clone(),
        };
        ToolResult::Text(text)
    }
}

#[derive(Deserialize, JsonSchema)]
struct ClipClearInput {
    /// Register to clear. Omit to clear ALL registers.
    #[serde(default)]
    register: Option<String>,
}

pub struct ClipClear;

#[async_trait]
impl Tool for ClipClear {
    fn name(&self) -> &str {
        "clip_clear"
    }
    fn description(&self) -> &str {
        "Drop one clipboard register, or all of them when register is omitted. Returns the count cleared."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ClipClearInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ClipClearInput =
            serde_json::from_value(input).unwrap_or(ClipClearInput { register: None });
        let mut clip = cx.clipboard.lock().unwrap();
        let cleared = match args.register {
            Some(name) => clip.map.remove(&name).map(|_| 1).unwrap_or(0),
            None => {
                let n = clip.map.len();
                clip.map.clear();
                n
            }
        };
        ToolResult::Json(json!({ "cleared": cleared }))
    }
}

/// Surface evicted register names on a producer's response — never drop them
/// silently (the design's "no silent truncation" rule).
pub(crate) fn annotate_evicted(out: &mut Value, evicted: Vec<String>) {
    if !evicted.is_empty()
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert("evicted".into(), json!(evicted));
        obj.insert(
            "evicted_note".into(),
            json!("clipboard cap reached; least-recently-used registers were dropped"),
        );
    }
}

/// The clip_* built-ins, in registration order.
pub fn clip_tools() -> Vec<std::sync::Arc<dyn Tool>> {
    use std::sync::Arc;
    vec![
        Arc::new(ClipYank),
        Arc::new(ClipPaste),
        Arc::new(ClipSet),
        Arc::new(ClipList),
        Arc::new(ClipPeek),
        Arc::new(ClipClear),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_text(regs: &mut Registers, name: &str, text: &str) -> Vec<String> {
        regs.put(name, RefKind::Text, text.to_string(), None, false)
            .unwrap()
    }

    #[test]
    fn side_round_trip_preserves_registers() {
        let mut regs = Registers::default();
        put_text(&mut regs, "@", "hello");
        put_text(&mut regs, "a", "world");
        let blob = regs.to_side();

        let restored = Registers::from_side(&blob);
        assert_eq!(restored.get("@").unwrap().text, "hello");
        assert_eq!(restored.get("a").unwrap().text, "world");
        // Clock resumes past the max touched stamp so new writes stay ordered.
        assert!(restored.clock >= 2);
    }

    #[test]
    fn legacy_or_empty_side_restores_empty() {
        assert!(Registers::from_side(&Value::Null).map.is_empty());
        assert!(Registers::from_side(&json!({"todos": []})).map.is_empty());
    }

    #[test]
    fn append_accumulates() {
        let mut regs = Registers::default();
        regs.put("g", RefKind::Text, "a\n".into(), None, false)
            .unwrap();
        regs.put("g", RefKind::Text, "b\n".into(), None, true)
            .unwrap();
        assert_eq!(regs.get("g").unwrap().text, "a\nb\n");
    }

    #[test]
    fn over_cap_single_register_is_rejected() {
        let mut regs = Registers::default();
        let huge = "x".repeat(MAX_TOTAL_BYTES + 1);
        let err = regs.put("@", RefKind::Text, huge, None, false).unwrap_err();
        assert!(err.to_string().contains("over the"));
    }

    #[test]
    fn lru_evicts_oldest_and_reports() {
        let mut regs = Registers::default();
        // Three ~100KB registers; cap is 256KB, so the third write evicts the
        // oldest (which is "@", written first and never touched since).
        let big = "y".repeat(100 * 1024);
        put_text(&mut regs, "@", &big);
        put_text(&mut regs, "a", &big);
        let evicted = put_text(&mut regs, "b", &big);
        assert_eq!(evicted, vec!["@".to_string()]);
        assert!(regs.get("@").is_none());
        assert!(regs.get("a").is_some());
        assert!(regs.get("b").is_some());
    }

    #[test]
    fn touch_protects_from_eviction() {
        let mut regs = Registers::default();
        let big = "z".repeat(100 * 1024);
        put_text(&mut regs, "@", &big);
        put_text(&mut regs, "a", &big);
        // Touch "@" so "a" becomes the LRU victim.
        regs.touch_get("@");
        let evicted = put_text(&mut regs, "b", &big);
        assert_eq!(evicted, vec!["a".to_string()]);
        assert!(regs.get("@").is_some());
    }

    #[test]
    fn preview_head_is_bounded() {
        let mut regs = Registers::default();
        put_text(&mut regs, "@", "l1\nl2\nl3\nl4\nl5");
        let head = regs.get("@").unwrap().preview_head();
        assert_eq!(head, "l1\nl2\nl3…");
    }

    #[test]
    fn consume_text_touches_and_errors_on_empty() {
        let mut regs = Registers::default();
        put_text(&mut regs, "@", "payload");
        assert_eq!(regs.consume_text("@").unwrap(), "payload");
        assert!(regs.consume_text("missing").unwrap_err().contains("empty"));
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;
    use crate::tool::ToolCx;
    use std::sync::{Arc, Mutex};

    fn cx_at(root: &std::path::Path) -> ToolCx {
        ToolCx {
            root: root.to_path_buf(),
            safety: Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(crate::todo::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(crate::shell::ShellSessions::default())),
            clipboard: Arc::new(Mutex::new(Registers::default())),
        }
    }

    /// clip_yank → clip_paste moves bytes without the content ever appearing in
    /// a tool result; clip_peek is the only egress.
    #[tokio::test]
    async fn yank_paste_round_trip_keeps_content_server_side() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let cx = cx_at(dir.path());

        let yank = ClipYank
            .call(
                json!({"source": "src.txt", "source_range": {"type": "lines", "start_line": 2, "end_line": 2}, "register": "a"}),
                &cx,
            )
            .await;
        let v = match yank {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        // The producer returns metadata, NOT the slice text.
        assert_eq!(v["register"], "a");
        assert_eq!(v["byte_len"], "beta\n".len());
        assert!(v.get("text").is_none());
        assert_eq!(v["preview_head"], "beta");

        // Paste appends into a new file (dry-run first, then confirm).
        let dry = ClipPaste
            .call(
                json!({"target": "out.txt", "insert": {"type": "append"}, "register": "a"}),
                &cx,
            )
            .await;
        assert_eq!(json_of(dry)["status"], "dry_run");
        let done = ClipPaste
            .call(
                json!({"target": "out.txt", "insert": {"type": "append"}, "register": "a", "confirm": true}),
                &cx,
            )
            .await;
        assert_eq!(json_of(done)["status"], "written");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "beta\n"
        );

        // clip_peek is the explicit egress.
        let peek = ClipPeek.call(json!({"register": "a"}), &cx).await;
        match peek {
            ToolResult::Text(t) => assert_eq!(t, "beta\n"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// Stage 2: file_read{into} produces a register; file_write{from} consumes
    /// it — a file→file copy with zero content in context.
    #[tokio::test]
    async fn file_read_into_then_file_write_from() {
        use crate::workspace::{FileRead, FileWrite};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let cx = cx_at(dir.path());

        let read = FileRead
            .call(
                json!({"file_path": "a.txt", "start_line": 1, "end_line": 2, "into": "r"}),
                &cx,
            )
            .await;
        let v = json_of(read);
        assert_eq!(v["register"], "r");
        assert!(v.get("text").is_none(), "into must not return content");

        let write = FileWrite
            .call(json!({"file_path": "b.txt", "from": "r"}), &cx)
            .await;
        assert_eq!(json_of(write)["ok"], true);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "one\ntwo"
        );
    }

    #[tokio::test]
    async fn file_write_rejects_both_content_and_from() {
        use crate::workspace::FileWrite;
        let dir = tempfile::tempdir().unwrap();
        let cx = cx_at(dir.path());
        let r = FileWrite
            .call(
                json!({"file_path": "x.txt", "content": "hi", "from": "r"}),
                &cx,
            )
            .await;
        assert!(matches!(r, ToolResult::Error(e) if e.contains("not both")));
    }

    /// Stage 2: shell_run{stdout_to} captures stdout into a register and keeps
    /// it out of the response.
    #[tokio::test]
    async fn shell_stdout_to_register() {
        use crate::shell::ShellRun;
        let dir = tempfile::tempdir().unwrap();
        let cx = cx_at(dir.path());
        let r = ShellRun
            .call(
                json!({"command": "printf 'captured-output'", "stdout_to": "s"}),
                &cx,
            )
            .await;
        let v = json_of(r);
        assert_eq!(v["stdout_register"], "s");
        assert_eq!(v["stdout_bytes"], "captured-output".len());
        assert!(
            v.get("stdout").is_none(),
            "stdout must be routed to the register"
        );
        assert_eq!(
            cx.clipboard.lock().unwrap().get("s").unwrap().text,
            "captured-output"
        );
    }

    fn json_of(r: ToolResult) -> Value {
        match r {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }
}
