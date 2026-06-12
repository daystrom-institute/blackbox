use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

pub const DEFAULT_RESUME_QUEUE_CAP: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTurn {
    pub turn_id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueStatus {
    pub depth: usize,
    pub active: bool,
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    Full { cap: usize },
    Closed,
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full { cap } => write!(f, "consultant resume queue full (cap {cap})"),
            Self::Closed => f.write_str("consultant resume queue closed"),
        }
    }
}

impl std::error::Error for QueueError {}

#[derive(Debug)]
pub struct ResumeQueue {
    cap: usize,
    inner: Mutex<ResumeQueueInner>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct ResumeQueueInner {
    pending: VecDeque<PendingTurn>,
    active: bool,
    closed: bool,
}

impl ResumeQueue {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(ResumeQueueInner::default()),
            notify: Notify::new(),
        }
    }

    pub fn enqueue(&self, turn: PendingTurn) -> Result<usize, QueueError> {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(QueueError::Closed);
        }
        if inner.pending.len() >= self.cap {
            return Err(QueueError::Full { cap: self.cap });
        }
        inner.pending.push_back(turn);
        let depth = inner.pending.len();
        drop(inner);
        self.notify.notify_waiters();
        Ok(depth)
    }

    pub fn enqueue_priority(&self, turn: PendingTurn) -> Result<usize, QueueError> {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(QueueError::Closed);
        }
        if inner.pending.len() >= self.cap {
            inner.pending.pop_back();
        }
        inner.pending.push_front(turn);
        let depth = inner.pending.len();
        drop(inner);
        self.notify.notify_waiters();
        Ok(depth)
    }

    pub fn pop_next(&self) -> Option<PendingTurn> {
        self.inner.lock().pending.pop_front()
    }

    pub async fn wait_until_turn(
        self: &Arc<Self>,
        turn_id: &str,
    ) -> Result<QueuePermit, QueueError> {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.lock();
                if inner.closed {
                    return Err(QueueError::Closed);
                }
                let ready = !inner.active
                    && inner
                        .pending
                        .front()
                        .is_some_and(|turn| turn.turn_id == turn_id);
                if ready {
                    inner.active = true;
                    let turn = inner.pending.pop_front().expect("front checked above");
                    return Ok(QueuePermit {
                        queue: self.clone(),
                        turn,
                    });
                }
            }
            notified.await;
        }
    }

    pub fn status(&self) -> QueueStatus {
        let inner = self.inner.lock();
        QueueStatus {
            depth: inner.pending.len(),
            active: inner.active,
            closed: inner.closed,
        }
    }

    pub fn close_and_clear(&self) -> Vec<PendingTurn> {
        let mut inner = self.inner.lock();
        inner.closed = true;
        inner.active = false;
        self.notify.notify_waiters();
        inner.pending.drain(..).collect()
    }

    fn finish_current(&self) {
        {
            let mut inner = self.inner.lock();
            inner.active = false;
        }
        self.notify.notify_waiters();
    }
}

impl Default for ResumeQueue {
    fn default() -> Self {
        Self::new(DEFAULT_RESUME_QUEUE_CAP)
    }
}

#[derive(Debug)]
pub struct QueuePermit {
    queue: Arc<ResumeQueue>,
    pub turn: PendingTurn,
}

impl Drop for QueuePermit {
    fn drop(&mut self) {
        self.queue.finish_current();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str) -> PendingTurn {
        PendingTurn {
            turn_id: id.to_string(),
            prompt: format!("prompt {id}"),
        }
    }

    #[test]
    fn queue_caps_regular_resumes() {
        let queue = ResumeQueue::new(2);
        assert_eq!(queue.enqueue(turn("1")).unwrap(), 1);
        assert_eq!(queue.enqueue(turn("2")).unwrap(), 2);
        assert_eq!(queue.enqueue(turn("3")), Err(QueueError::Full { cap: 2 }));
        assert_eq!(queue.status().depth, 2);
    }

    #[test]
    fn priority_enqueue_takes_head_and_drops_tail_when_full() {
        let queue = ResumeQueue::new(2);
        queue.enqueue(turn("1")).unwrap();
        queue.enqueue(turn("2")).unwrap();
        queue.enqueue_priority(turn("dismiss")).unwrap();
        assert_eq!(queue.pop_next().unwrap().turn_id, "dismiss");
        assert_eq!(queue.pop_next().unwrap().turn_id, "1");
        assert!(queue.pop_next().is_none());
    }

    #[test]
    fn close_clears_and_rejects_new_turns() {
        let queue = ResumeQueue::new(3);
        queue.enqueue(turn("1")).unwrap();
        let drained = queue.close_and_clear();
        assert_eq!(drained.len(), 1);
        assert_eq!(queue.enqueue(turn("2")), Err(QueueError::Closed));
        assert!(queue.status().closed);
    }

    #[tokio::test]
    async fn wait_until_turn_serializes_active_work() {
        let queue = Arc::new(ResumeQueue::new(3));
        queue.enqueue(turn("1")).unwrap();
        queue.enqueue(turn("2")).unwrap();

        let first = queue.clone().wait_until_turn("1").await.unwrap();
        assert_eq!(first.turn.turn_id, "1");
        assert!(queue.status().active);

        let second_queue = queue.clone();
        let second = tokio::spawn(async move {
            let permit = second_queue.wait_until_turn("2").await.unwrap();
            permit.turn.turn_id.clone()
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());

        drop(first);
        assert_eq!(second.await.unwrap(), "2");
        assert!(!queue.status().active);
    }
}
