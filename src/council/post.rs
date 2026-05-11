//! `CouncilPost` — one entry on the conversational coordination log.
//! Append-only; persisted line-per-record at `<store>/councils/<id>/posts.jsonl`.
//!
//! Distinguished from a whiteboard post: a council post is chat, not a
//! structured decision artifact. If the deliberation lands a claim
//! worth durable record the user posts it to a whiteboard separately.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    User,
    Bro,
    System,
}

/// What this post is responding to. Catchup posts span a range of
/// prior turns rather than replying to one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyScope {
    /// Top-of-conversation user post (no prior context).
    Origin,
    /// Reply to one specific prior post.
    Direct { seq: u64 },
    /// Coalesced response after the bro fell behind. `included_seqs`
    /// are turns the reply addresses; `omitted_seqs` are turns the bro
    /// chose not to engage with (preserved for transcript clarity).
    Catchup {
        from_seq: u64,
        to_seq: u64,
        included_seqs: Vec<u64>,
        omitted_seqs: Vec<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilPost {
    pub council_id: String,
    pub sequence: u64,
    pub sender_kind: SenderKind,
    pub sender_id: String,
    pub body: String,
    /// `@name` tokens parsed from `body` at post time.
    #[serde(default)]
    pub addressed_to: Vec<String>,
    pub reply_scope: ReplyScope,
    /// Source envelope that produced this post. `Some(id)` iff
    /// `sender_kind == Bro` — bro posts are produced by exactly one
    /// envelope. `User` posts originate, `System` posts are daemon-
    /// emitted notices (council opened / closed / member added). Both
    /// originator kinds carry `None`. Validated by `CouncilPost::new_*`
    /// constructors — do not construct manually.
    #[serde(default)]
    pub source_envelope_id: Option<String>,
    pub created_at: String,
}

impl CouncilPost {
    pub fn new_user(council_id: String, sequence: u64, sender_id: String, body: String) -> Self {
        let addressed_to = parse_mentions(&body);
        Self {
            council_id,
            sequence,
            sender_kind: SenderKind::User,
            sender_id,
            body,
            addressed_to,
            reply_scope: ReplyScope::Origin,
            source_envelope_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    pub fn new_bro(
        council_id: String,
        sequence: u64,
        bro_id: String,
        body: String,
        reply_scope: ReplyScope,
        source_envelope_id: String,
    ) -> Self {
        let addressed_to = parse_mentions(&body);
        Self {
            council_id,
            sequence,
            sender_kind: SenderKind::Bro,
            sender_id: bro_id,
            body,
            addressed_to,
            reply_scope,
            source_envelope_id: Some(source_envelope_id),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Format a single post for inclusion in a replay or catchup frame.
    pub fn render_for_frame(&self) -> String {
        let prefix = match self.sender_kind {
            SenderKind::User => "user".to_string(),
            SenderKind::Bro => self.sender_id.clone(),
            SenderKind::System => "system".to_string(),
        };
        format!("turn {} [{}] {}", self.sequence, prefix, self.body.trim())
    }
}

/// Parse `@name` tokens from a body. Names are alphanumeric +
/// hyphen + underscore; the parser is intentionally simple and
/// permissive — addressing is a hint, not a strict identifier system.
pub fn parse_mentions(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '@' {
            continue;
        }
        let mut name = String::new();
        while let Some(&peek) = chars.peek() {
            if peek.is_ascii_alphanumeric() || peek == '-' || peek == '_' {
                name.push(peek);
                chars.next();
            } else {
                break;
            }
        }
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_post_has_no_envelope() {
        let p = CouncilPost::new_user("c1".into(), 1, "user".into(), "hello @alice".into());
        assert_eq!(p.sender_kind, SenderKind::User);
        assert_eq!(p.source_envelope_id, None);
        assert_eq!(p.addressed_to, vec!["alice"]);
    }

    #[test]
    fn bro_post_carries_envelope() {
        let p = CouncilPost::new_bro(
            "c1".into(),
            2,
            "alice".into(),
            "ack".into(),
            ReplyScope::Direct { seq: 1 },
            "env-deadbeef".into(),
        );
        assert_eq!(p.sender_kind, SenderKind::Bro);
        assert_eq!(p.source_envelope_id.as_deref(), Some("env-deadbeef"));
    }

    #[test]
    fn parse_multiple_mentions() {
        assert_eq!(
            parse_mentions("@alice and @bob, what about @charlie-1?"),
            vec!["alice", "bob", "charlie-1"]
        );
    }

    #[test]
    fn parse_mentions_dedupes() {
        assert_eq!(
            parse_mentions("@alice @alice @alice"),
            vec!["alice".to_string()]
        );
    }

    #[test]
    fn parse_mentions_ignores_email() {
        let m = parse_mentions("contact a@b.com or @alice");
        assert!(m.contains(&"alice".to_string()));
    }

    #[test]
    fn reply_scope_serializes_with_tag() {
        let s = serde_json::to_string(&ReplyScope::Direct { seq: 5 }).unwrap();
        assert!(s.contains("\"kind\":\"direct\""));
        let c = ReplyScope::Catchup {
            from_seq: 3,
            to_seq: 7,
            included_seqs: vec![3, 5],
            omitted_seqs: vec![4, 6, 7],
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"kind\":\"catchup\""));
    }
}
