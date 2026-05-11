//! `InboxEnvelope` — one queued, draining, completed, or failed unit
//! of work for a (council, bro) pair. Persisted as the full vector
//! per council at `<store>/councils/<id>/envelopes.json`, atomic
//! tmp+rename on each transition.
//!
//! Envelopes carry the lease + retry shape needed to recover from
//! daemon restarts mid-drain: the worker stamps `lease_owner` /
//! `lease_expires_at` when it transitions queued → draining; the
//! restart reconciler walks expired draining envelopes and decides
//! between (a) marking done if a CouncilPost references the envelope,
//! (b) requeueing with `attempt_count += 1`, or (c) failing if
//! `attempt_count >= config.max_attempts`.

use serde::{Deserialize, Serialize};

use super::post::ReplyScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeStatus {
    Queued,
    Draining,
    Done,
    /// Filtered before posting (low-signal reply, empty body, etc.).
    Dropped,
    /// Coalesced into a sibling envelope; rendered frame is on the
    /// envelope referenced by `superseded_by`.
    Superseded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplyMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    pub latency_ms: u64,
    pub queue_depth_at_drain: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEnvelope {
    pub id: String,
    pub council_id: String,
    pub bro_id: String,
    pub status: EnvelopeStatus,
    pub reply_scope: ReplyScope,
    /// SHA-256 of the rendered prompt frame (replay or catchup),
    /// when one was rendered. The frame body itself is stored
    /// separately under `frames/<envelope_id>.txt` so the envelope
    /// vector stays small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered_frame_hash: Option<String>,

    /// True if the user `@mentioned` the bro in the originating post.
    /// The drain worker emits "you must respond" framing in the
    /// council ambient when set.
    pub addressed_by_user: bool,
    /// True if the envelope was created by mention-forwarding from
    /// another bro's reply. Optional response, may pass.
    pub mentioned_by_bro: bool,

    /// The post sequence that produced this envelope — used as the
    /// dedupe key `(source_post_seq, bro_id)` to prevent the same
    /// turn cascading multiple envelopes onto the same bro.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_post_seq: Option<u64>,

    pub relay_depth: u32,

    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<String>,
    /// Set on coalesced envelopes; points at the envelope that
    /// absorbed this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_post_seq: Option<u64>,
    /// Audit trail: when the drain produced no post, why
    /// (`low_signal`, `empty`, `tool_failure`, `passed`, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_meta: Option<ReplyMeta>,

    pub enqueued_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

impl InboxEnvelope {
    pub fn new_queued(
        id: String,
        council_id: String,
        bro_id: String,
        reply_scope: ReplyScope,
        addressed_by_user: bool,
        mentioned_by_bro: bool,
        source_post_seq: Option<u64>,
        relay_depth: u32,
    ) -> Self {
        Self {
            id,
            council_id,
            bro_id,
            status: EnvelopeStatus::Queued,
            reply_scope,
            rendered_frame_hash: None,
            addressed_by_user,
            mentioned_by_bro,
            source_post_seq,
            relay_depth,
            attempt_count: 0,
            lease_owner: None,
            lease_expires_at: None,
            last_error: None,
            next_retry_at: None,
            superseded_by: None,
            reply_post_seq: None,
            drop_reason: None,
            reply_meta: None,
            enqueued_at: chrono::Utc::now().to_rfc3339(),
            started_at: None,
            finished_at: None,
        }
    }

    #[allow(dead_code)] // used by tests in same file
    pub fn dedupe_key(&self) -> Option<(u64, &str)> {
        self.source_post_seq.map(|s| (s, self.bro_id.as_str()))
    }
}

/// Compute the SHA-256 hash (hex) of a rendered prompt frame so
/// the envelope can audit which body produced its reply without
/// embedding the full text.
pub fn frame_hash(body: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_queued_starts_fresh() {
        let e = InboxEnvelope::new_queued(
            "env-1".into(),
            "c1".into(),
            "alice".into(),
            ReplyScope::Direct { seq: 5 },
            true,
            false,
            Some(5),
            0,
        );
        assert_eq!(e.status, EnvelopeStatus::Queued);
        assert_eq!(e.attempt_count, 0);
        assert!(e.lease_owner.is_none());
        assert_eq!(e.dedupe_key(), Some((5, "alice")));
    }

    #[test]
    fn frame_hash_is_stable() {
        let a = frame_hash("hello world");
        let b = frame_hash("hello world");
        assert_eq!(a, b);
        let c = frame_hash("hello world ");
        assert_ne!(a, c);
    }
}
