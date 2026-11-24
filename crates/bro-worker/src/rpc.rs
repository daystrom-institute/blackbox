//! JSON-RPC over a native provider server's stdio: one JSON object per line.
//!
//! Responses resolve the request that is waiting on them, notifications go
//! to the adapter's driver in arrival order, and requests the server makes of
//! the client are answered here by the adapter's [`Peer`], so a stray request
//! can never stall a turn.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{Mutex, mpsc, oneshot};

type Writer = Box<dyn AsyncWrite + Unpin + Send>;
type Pending = Arc<std::sync::Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// What differs between the servers this crate drives.
#[derive(Clone, Copy)]
pub struct Peer {
    /// Names the provider in request errors.
    pub name: &'static str,
    /// Names the server process in connection errors.
    pub process: &'static str,
    /// The result for a request the server makes of this headless client, or
    /// `None` to refuse the method as unsupported.
    pub answer: fn(method: &str, params: &Value) -> Option<Value>,
}

/// A JSON-RPC error, or the server going away before it answered.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RpcError {}

/// A notification from the server.
#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[derive(Clone)]
pub struct Rpc {
    writer: Arc<Mutex<Option<Writer>>>,
    next_id: Arc<AtomicI64>,
    pending: Pending,
    /// Set once the server's stdout has ended; nothing will answer after.
    closed: Arc<AtomicBool>,
    peer: Peer,
}

/// The answer to a request sent with [`Rpc::call`].
pub struct Reply(oneshot::Receiver<Result<Value, RpcError>>, Peer);

impl Reply {
    /// Wait for the answer, however long the server takes. Resolves with an
    /// error when the server exits first.
    pub async fn wait(self) -> Result<Value, RpcError> {
        let Reply(receiver, peer) = self;
        receiver.await.unwrap_or_else(|_| Err(gone(peer)))
    }
}

impl Rpc {
    /// Start the connection: a reader task demultiplexes `reader`, and the
    /// returned receiver yields notifications until the server's stdout ends.
    pub fn start<R, W>(
        reader: R,
        writer: W,
        peer: Peer,
    ) -> (Self, mpsc::UnboundedReceiver<Notification>)
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let rpc = Self {
            writer: Arc::new(Mutex::new(Some(Box::new(writer)))),
            next_id: Arc::new(AtomicI64::new(1)),
            pending: Arc::default(),
            closed: Arc::default(),
            peer,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(rpc.clone().read_loop(reader, tx));
        (rpc, rx)
    }

    /// Send a request and wait for its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let (id, rx) = self.register()?;
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            self.send(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
                .await?;
            rx.await.unwrap_or_else(|_| Err(gone(self.peer)))
        })
        .await
        .unwrap_or_else(|_| {
            Err(RpcError {
                message: format!("{} {method} timed out after 30 seconds", self.peer.name),
            })
        });
        self.forget(id);
        outcome
    }

    /// Send a request whose answer has no deadline, such as one the server
    /// holds open for a whole turn. The write is bounded; the returned
    /// [`Reply`] resolves when the server answers or exits.
    pub async fn call(&self, method: &str, params: Value) -> Result<Reply, RpcError> {
        let (id, rx) = self.register()?;
        let sent = self
            .send(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
            .await;
        if let Err(error) = sent {
            self.forget(id);
            return Err(error);
        }
        Ok(Reply(rx, self.peer))
    }

    fn register(&self) -> Result<(i64, oneshot::Receiver<Result<Value, RpcError>>), RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, tx);
        if self.closed.load(Ordering::SeqCst) {
            self.forget(id);
            return Err(gone(self.peer));
        }
        Ok((id, rx))
    }

    fn forget(&self, id: i64) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Close the server's stdin. The server exits at end of input.
    pub async fn close(&self) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            if let Some(mut writer) = self.writer.lock().await.take() {
                let _ = writer.shutdown().await;
            }
        })
        .await;
    }

    async fn send(&self, message: &Value) -> Result<(), RpcError> {
        tokio::time::timeout(std::time::Duration::from_secs(30), self.send_inner(message))
            .await
            .unwrap_or_else(|_| {
                Err(RpcError {
                    message: format!("{} RPC write timed out after 30 seconds", self.peer.name),
                })
            })
    }

    async fn send_inner(&self, message: &Value) -> Result<(), RpcError> {
        let mut line = message.to_string();
        line.push('\n');
        let mut guard = self.writer.lock().await;
        let Some(writer) = guard.as_mut() else {
            return Err(gone(self.peer));
        };
        let process = self.peer.process;
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|error| RpcError {
                message: format!("cannot write to {process}: {error}"),
            })?;
        writer.flush().await.map_err(|error| RpcError {
            message: format!("cannot write to {process}: {error}"),
        })
    }

    async fn read_loop<R>(self, mut reader: R, notifications: mpsc::UnboundedSender<Notification>)
    where
        R: AsyncBufRead + Unpin,
    {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(message) = serde_json::from_str::<Value>(line.trim_end()) else {
                continue;
            };
            let method = message.get("method").and_then(Value::as_str);
            let id = message.get("id").filter(|id| !id.is_null());
            match (method, id) {
                (None, Some(id)) => self.resolve(id, &message),
                (Some(method), Some(id)) => {
                    let params = message.get("params").unwrap_or(&Value::Null);
                    let reply = server_request_reply(self.peer, method, params, id.clone());
                    let _ = self.send(&reply).await;
                }
                (Some(method), None) => {
                    let _ = notifications.send(Notification {
                        method: method.to_string(),
                        params: message.get("params").cloned().unwrap_or(Value::Null),
                    });
                }
                (None, None) => {}
            }
        }
        self.closed.store(true, Ordering::SeqCst);
        let waiting: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .collect();
        for (_, waiter) in waiting {
            let _ = waiter.send(Err(gone(self.peer)));
        }
    }

    fn resolve(&self, id: &Value, message: &Value) {
        let Some(id) = id.as_i64() else {
            return;
        };
        let Some(waiter) = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id)
        else {
            return;
        };
        let outcome = match message.get("error") {
            Some(error) => Err(RpcError {
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string()),
            }),
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = waiter.send(outcome);
    }
}

fn gone(peer: Peer) -> RpcError {
    RpcError {
        message: format!("{} exited", peer.process),
    }
}

/// The answer to a request the server makes of a headless client.
fn server_request_reply(peer: Peer, method: &str, params: &Value, id: Value) -> Value {
    match (peer.answer)(method, params) {
        Some(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("bro-worker does not handle {method}")},
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    const PEER: Peer = Peer {
        name: "Test",
        process: "the test server",
        answer: |method, params| (method == "ask").then(|| json!({"echo": params["question"]})),
    };

    #[tokio::test(start_paused = true)]
    async fn unanswered_requests_and_blocked_writes_have_a_deadline() {
        for capacity in [1, 4096] {
            let (writer, _unread) = tokio::io::duplex(capacity);
            let (_silent, reader) = tokio::io::duplex(4096);
            let (rpc, _) = Rpc::start(BufReader::new(reader), writer, PEER);
            let error = rpc.request("initialize", json!({})).await.unwrap_err();
            assert!(error.message.contains("timed out"));
            assert!(rpc.pending.lock().unwrap().is_empty());
            rpc.close().await;
        }
    }

    #[tokio::test]
    async fn requests_resolve_notifications_flow_and_server_requests_are_answered() {
        let (client_out, server_in) = tokio::io::duplex(4096);
        let (server_out, client_in) = tokio::io::duplex(4096);
        let (rpc, mut notes) = Rpc::start(BufReader::new(client_in), client_out, PEER);
        let server = tokio::spawn(async move {
            let mut lines = BufReader::new(server_in).lines();
            let mut out = server_out;
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "ping");
            let id = request["id"].clone();
            out.write_all(b"{\"method\":\"note\",\"params\":{\"n\":1}}\n")
                .await
                .unwrap();
            out.write_all(b"{\"id\":99,\"method\":\"ask\",\"params\":{\"question\":7}}\n")
                .await
                .unwrap();
            out.write_all(b"{\"id\":\"100\",\"method\":\"other\",\"params\":{}}\n")
                .await
                .unwrap();
            out.write_all(format!("{}\n", json!({"id": id, "result": {"pong": true}})).as_bytes())
                .await
                .unwrap();
            let reply: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(reply["id"], 99);
            assert_eq!(reply["result"]["echo"], 7);
            let refusal: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(refusal["id"], "100");
            assert_eq!(refusal["error"]["code"], -32601);
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            out.write_all(
                format!(
                    "{}\n",
                    json!({"id": request["id"], "error": {"code": 1, "message": "nope"}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            drop(out);
        });
        assert_eq!(
            rpc.request("ping", json!({})).await.unwrap(),
            json!({"pong": true})
        );
        let note = notes.recv().await.unwrap();
        assert_eq!(
            (note.method.as_str(), &note.params),
            ("note", &json!({"n": 1}))
        );
        assert_eq!(
            rpc.request("fail", json!({})).await.unwrap_err().message,
            "nope"
        );
        server.await.unwrap();
        assert!(notes.recv().await.is_none());
        assert!(rpc.request("late", json!({})).await.is_err());
    }
}
