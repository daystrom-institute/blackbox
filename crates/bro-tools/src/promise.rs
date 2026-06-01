//! Harness-local async promises. A promise is a same-dispatch, already-started
//! unit of work produced by selected built-in tools.

use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, watch};
use tokio::time::{Duration, Instant};

const DEFAULT_WAIT_MS: u64 = 30_000;
const DEFAULT_MULTI_WAIT_MS: u64 = 30_000;
const WAKE_MESSAGE_LIMIT: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromiseState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl PromiseState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

struct PromiseEntry {
    producer: String,
    detail: Value,
    state: PromiseState,
    result: Option<Value>,
    error: Option<String>,
    started_ms: u64,
    settled_ms: Option<u64>,
    cancel_tx: watch::Sender<bool>,
    wake_requested: bool,
    wake_delivered: bool,
    wake_message: Option<String>,
}

/// Shared promise table for one harness run. It is intentionally not persisted:
/// v1 promises own live process handles and are same-dispatch only.
pub struct PromiseStore {
    map: BTreeMap<String, PromiseEntry>,
    counter: u64,
    notify: Arc<Notify>,
}

impl Default for PromiseStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PromiseStore {
    pub fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            counter: 0,
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn notifier(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    pub fn start(&mut self, producer: &str, detail: Value) -> (String, watch::Receiver<bool>) {
        self.counter += 1;
        let id = format!("pr-{}", self.counter);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.map.insert(
            id.clone(),
            PromiseEntry {
                producer: producer.to_string(),
                detail,
                state: PromiseState::Running,
                result: None,
                error: None,
                started_ms: now_ms(),
                settled_ms: None,
                cancel_tx,
                wake_requested: false,
                wake_delivered: false,
                wake_message: None,
            },
        );
        (id, cancel_rx)
    }

    pub fn settle_completed(&mut self, id: &str, result: Value) {
        self.settle(id, PromiseState::Completed, Some(result), None);
    }

    pub fn settle_failed(&mut self, id: &str, error: String) {
        self.settle(id, PromiseState::Failed, None, Some(error));
    }

    pub fn settle_cancelled(&mut self, id: &str, result: Value) {
        self.settle(id, PromiseState::Cancelled, Some(result), None);
    }

    fn settle(
        &mut self,
        id: &str,
        state: PromiseState,
        result: Option<Value>,
        error: Option<String>,
    ) {
        if let Some(entry) = self.map.get_mut(id) {
            if entry.state.is_terminal() {
                return;
            }
            entry.state = state;
            entry.result = result;
            entry.error = error;
            entry.settled_ms = Some(now_ms());
            self.notify.notify_waiters();
        }
    }

    pub fn cancel(&mut self, id: &str) -> anyhow::Result<Value> {
        let Some(entry) = self.map.get(id) else {
            anyhow::bail!("unknown promise_id: {id}");
        };
        if entry.state.is_terminal() {
            return Ok(snapshot(id, entry));
        }
        let _ = entry.cancel_tx.send(true);
        self.notify.notify_waiters();
        Ok(snapshot(id, entry))
    }

    pub fn status(&self, id: &str) -> anyhow::Result<Value> {
        let Some(entry) = self.map.get(id) else {
            anyhow::bail!("unknown promise_id: {id}");
        };
        Ok(snapshot(id, entry))
    }

    pub fn list(&self) -> Value {
        Value::Array(
            self.map
                .iter()
                .map(|(id, entry)| snapshot(id, entry))
                .collect(),
        )
    }

    pub fn all_terminal(&self, ids: &[String]) -> anyhow::Result<Option<Vec<Value>>> {
        let mut out = Vec::new();
        for id in ids {
            let Some(entry) = self.map.get(id) else {
                anyhow::bail!("unknown promise_id: {id}");
            };
            if !entry.state.is_terminal() {
                return Ok(None);
            }
            out.push(snapshot(id, entry));
        }
        Ok(Some(out))
    }

    pub fn any_terminal(&self, ids: &[String]) -> anyhow::Result<Option<Value>> {
        for id in ids {
            let Some(entry) = self.map.get(id) else {
                anyhow::bail!("unknown promise_id: {id}");
            };
            if entry.state.is_terminal() {
                return Ok(Some(snapshot(id, entry)));
            }
        }
        Ok(None)
    }

    pub fn wake(&mut self, id: &str, message: Option<String>) -> anyhow::Result<Value> {
        let Some(entry) = self.map.get_mut(id) else {
            anyhow::bail!("unknown promise_id: {id}");
        };
        entry.wake_requested = true;
        entry.wake_delivered = false;
        entry.wake_message = message.map(|m| truncate_chars(&m, WAKE_MESSAGE_LIMIT));
        if entry.state.is_terminal() {
            self.notify.notify_waiters();
        }
        Ok(json!({
            "promise_id": id,
            "wake_registered": true,
            "state": entry.state.as_str(),
            "running": !entry.state.is_terminal(),
        }))
    }

    pub fn drain_wake_messages(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for (id, entry) in &mut self.map {
            if !entry.wake_requested || entry.wake_delivered || !entry.state.is_terminal() {
                continue;
            }
            entry.wake_delivered = true;
            out.push(render_wake(id, entry));
        }
        out
    }
}

fn snapshot(id: &str, entry: &PromiseEntry) -> Value {
    json!({
        "promise_id": id,
        "producer": entry.producer,
        "state": entry.state.as_str(),
        "running": !entry.state.is_terminal(),
        "detail": entry.detail,
        "started_ms": entry.started_ms,
        "settled_ms": entry.settled_ms,
        "wake_requested": entry.wake_requested,
        "wake_delivered": entry.wake_delivered,
        "result": entry.result,
        "error": entry.error,
    })
}

fn render_wake(id: &str, entry: &PromiseEntry) -> String {
    let mut msg = format!(
        "[HARNESS_EVENT promise_{}]\npromise_id: {id}\nproducer: {}\nstate: {}\nnext_step: call promise_status with promise_id=\"{id}\" to inspect the result.",
        entry.state.as_str(),
        entry.producer,
        entry.state.as_str(),
    );
    if let Some(note) = &entry.wake_message {
        msg.push_str("\nnote: ");
        msg.push_str(note);
    }
    msg
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }
}

async fn wait_until<F>(cx: &ToolCx, timeout_ms: u64, mut done: F) -> anyhow::Result<Value>
where
    F: FnMut(&PromiseStore) -> anyhow::Result<Option<Value>>,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let notify = cx.promises.lock().unwrap().notifier();
    loop {
        if let Some(value) = done(&cx.promises.lock().unwrap())? {
            return Ok(value);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(json!({"timed_out": true}));
        }
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);
        tokio::select! {
            _ = notify.notified() => {}
            _ = &mut sleep => return Ok(json!({"timed_out": true})),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct PromiseIdInput {
    promise_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct PromiseWaitInput {
    promise_id: String,
    /// Max milliseconds to wait. Defaults to 30000.
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct PromiseManyInput {
    promise_ids: Vec<String>,
    /// Max milliseconds to wait. Defaults to 30000.
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct PromiseWakeInput {
    promise_id: String,
    /// Optional bounded note echoed in the hidden completion event.
    message: Option<String>,
}

pub struct PromiseStatus;
pub struct PromiseWait;
pub struct PromiseWhenAll;
pub struct PromiseWhenAny;
pub struct PromiseCancel;
pub struct PromiseList;
pub struct PromiseWake;

#[async_trait]
impl Tool for PromiseStatus {
    fn name(&self) -> &str {
        "promise_status"
    }
    fn description(&self) -> &str {
        "Inspect a harness-local promise by id. Returns state, producer, result/error when terminal, and wake metadata."
    }
    fn input_schema(&self) -> Value {
        schema_for::<PromiseIdInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: PromiseIdInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        ToolResult::from_result(cx.promises.lock().unwrap().status(&args.promise_id))
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
}

#[async_trait]
impl Tool for PromiseWait {
    fn name(&self) -> &str {
        "promise_wait"
    }
    fn description(&self) -> &str {
        "Wait for one promise to settle, up to timeout_ms. Returns the terminal promise snapshot, or {timed_out:true}."
    }
    fn input_schema(&self) -> Value {
        schema_for::<PromiseWaitInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: PromiseWaitInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let id = args.promise_id;
        ToolResult::from_result(
            wait_until(cx, args.timeout_ms.unwrap_or(DEFAULT_WAIT_MS), |store| {
                let status = store.status(&id)?;
                if status["running"].as_bool() == Some(false) {
                    Ok(Some(status))
                } else {
                    Ok(None)
                }
            })
            .await,
        )
    }
}

#[async_trait]
impl Tool for PromiseWhenAll {
    fn name(&self) -> &str {
        "promise_when_all"
    }
    fn description(&self) -> &str {
        "Wait until every listed promise settles, up to timeout_ms. Returns {promises:[...]} or {timed_out:true}."
    }
    fn input_schema(&self) -> Value {
        schema_for::<PromiseManyInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: PromiseManyInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let ids = args.promise_ids;
        ToolResult::from_result(
            wait_until(
                cx,
                args.timeout_ms.unwrap_or(DEFAULT_MULTI_WAIT_MS),
                |store| match store.all_terminal(&ids)? {
                    Some(promises) => Ok(Some(json!({"promises": promises}))),
                    None => Ok(None),
                },
            )
            .await,
        )
    }
}

#[async_trait]
impl Tool for PromiseWhenAny {
    fn name(&self) -> &str {
        "promise_when_any"
    }
    fn description(&self) -> &str {
        "Wait until any listed promise settles, up to timeout_ms. Returns {promise:...} or {timed_out:true}."
    }
    fn input_schema(&self) -> Value {
        schema_for::<PromiseManyInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: PromiseManyInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let ids = args.promise_ids;
        ToolResult::from_result(
            wait_until(
                cx,
                args.timeout_ms.unwrap_or(DEFAULT_MULTI_WAIT_MS),
                |store| match store.any_terminal(&ids)? {
                    Some(promise) => Ok(Some(json!({"promise": promise}))),
                    None => Ok(None),
                },
            )
            .await,
        )
    }
}

#[async_trait]
impl Tool for PromiseCancel {
    fn name(&self) -> &str {
        "promise_cancel"
    }
    fn description(&self) -> &str {
        "Request cancellation of a running promise. The producer settles it as cancelled once teardown completes."
    }
    fn input_schema(&self) -> Value {
        schema_for::<PromiseIdInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: PromiseIdInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        ToolResult::from_result(cx.promises.lock().unwrap().cancel(&args.promise_id))
    }
}

#[async_trait]
impl Tool for PromiseList {
    fn name(&self) -> &str {
        "promise_list"
    }
    fn description(&self) -> &str {
        "List harness-local promises for this dispatch, including running and terminal entries."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn call(&self, _input: Value, cx: &ToolCx) -> ToolResult {
        ToolResult::Json(cx.promises.lock().unwrap().list())
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
}

#[async_trait]
impl Tool for PromiseWake {
    fn name(&self) -> &str {
        "promise_wake"
    }
    fn description(&self) -> &str {
        "Register a hidden HARNESS_EVENT user turn to be injected when a promise settles. Use when you want to keep working and be nudged to inspect completion later."
    }
    fn input_schema(&self) -> Value {
        schema_for::<PromiseWakeInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: PromiseWakeInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        ToolResult::from_result(
            cx.promises
                .lock()
                .unwrap()
                .wake(&args.promise_id, args.message),
        )
    }
}

pub fn promise_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(PromiseStatus),
        Arc::new(PromiseWait),
        Arc::new(PromiseWhenAll),
        Arc::new(PromiseWhenAny),
        Arc::new(PromiseCancel),
        Arc::new(PromiseList),
        Arc::new(PromiseWake),
    ]
}
