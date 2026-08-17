//! Slack response shapes, as narrow as the collector actually reads.
//!
//! Deliberately NOT `deny_unknown_fields`, which is the opposite of the wire
//! crate's posture and the opposite for a reason. The wire is a contract
//! between two halves of this system and an unknown field there is a version
//! skew worth refusing. This is a vendor response that gains fields on the
//! vendor's schedule, and refusing a message because Slack added a key would
//! turn a product announcement into an ingest outage.
//!
//! What IS strict here: every field the collector reads is optional in the type
//! when it is optional in practice, so a missing `user` on a bot message is a
//! normalization decision (see [`crate::normalize`]) rather than a parse error
//! that drops the whole page.

use serde::Deserialize;

/// The envelope every Slack Web API response carries.
///
/// `ok: false` is the vendor's error channel and arrives with HTTP 200, which
/// is why the client checks this before it checks anything else. A collector
/// that only looked at the status line would read `{"ok":false,
/// "error":"ratelimited"}` as a successful empty page and advance a watermark
/// over messages it never saw.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackEnvelope {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResponseMetadata {
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl ResponseMetadata {
    /// The next cursor, treating the empty string as absent.
    ///
    /// Slack signals "no more pages" with `"next_cursor": ""` rather than by
    /// omitting the key, and a client that treated the empty string as a cursor
    /// would page forever.
    pub fn cursor(&self) -> Option<&str> {
        self.next_cursor
            .as_deref()
            .filter(|cursor| !cursor.is_empty())
    }
}

/// `auth.test`: who this token is, and where.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthTestResponse {
    #[serde(flatten)]
    pub envelope: SlackEnvelope,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
}

/// One channel from `conversations.list`.
#[derive(Debug, Clone, Deserialize)]
pub struct RawChannel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub is_member: bool,
    /// Epoch seconds of the channel's creation. `conversations.list` and
    /// `users.conversations` both return it, and it is the FLOOR below which
    /// the channel's history cannot exist: a backfill that walks past it is
    /// issuing empty windows toward 1970 forever. Optional because a fixture
    /// or a future vendor shape may omit it, in which case the horizon alone
    /// still bounds the walk.
    #[serde(default)]
    pub created: Option<i64>,
    /// Present and true on a direct message. The deployed posture has none
    /// (design 3.1 ruling, and [`bbox_conversation_source::ChannelClassV1`] is
    /// closed to channel classes), so these exist here ONLY so the enrollment
    /// policy can refuse them explicitly rather than mislabel them as channels.
    #[serde(default)]
    pub is_im: bool,
    #[serde(default)]
    pub is_mpim: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConversationsListResponse {
    #[serde(flatten)]
    pub envelope: SlackEnvelope,
    #[serde(default)]
    pub channels: Vec<RawChannel>,
    #[serde(default)]
    pub response_metadata: ResponseMetadata,
}

/// The provider's edit stamp sub-object.
#[derive(Debug, Clone, Deserialize)]
pub struct RawEdited {
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawReaction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub count: u32,
}

/// A shared file reference.
///
/// The `url_private` field is deliberately ABSENT from this type. It is
/// credential-adjacent and it expires when the workspace deletes the file, so
/// persisting one would durably record a link that stops working; the wire
/// crate's `AttachmentRefV1` refuses it for the same reason. The remote id is
/// what a later blob pass needs.
#[derive(Debug, Clone, Deserialize)]
pub struct RawFile {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mimetype: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// One message as `conversations.history` or `conversations.replies` returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct RawMessage {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// Present on an app-authored message. Together with `subtype` this is how
    /// author kind is decided without spending a `users.info` call per author.
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    /// The thread this message belongs to. On a PARENT this equals `ts`, which
    /// is the distinction [`crate::normalize`] has to make: a parent is not its
    /// own reply.
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub reply_count: Option<u64>,
    /// The newest reply's `ts`. Design 5.3's cheap resweep test: an idle thread
    /// whose latest reply the producer already swept costs zero calls.
    #[serde(default)]
    pub latest_reply: Option<String>,
    #[serde(default)]
    pub edited: Option<RawEdited>,
    #[serde(default)]
    pub reactions: Vec<RawReaction>,
    #[serde(default)]
    pub files: Vec<RawFile>,
}

impl RawMessage {
    /// True when this message is a thread PARENT carrying replies.
    pub fn is_thread_parent(&self) -> bool {
        match (&self.thread_ts, &self.ts) {
            (Some(thread), Some(ts)) => thread == ts && self.reply_count.unwrap_or(0) > 0,
            _ => false,
        }
    }

    /// The parent this message replies to, or `None` when it is not a reply.
    pub fn reply_parent(&self) -> Option<&str> {
        match (&self.thread_ts, &self.ts) {
            (Some(thread), Some(ts)) if thread != ts => Some(thread.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConversationsHistoryResponse {
    #[serde(flatten)]
    pub envelope: SlackEnvelope,
    #[serde(default)]
    pub messages: Vec<RawMessage>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub response_metadata: ResponseMetadata,
}

/// The workspace and bot identity one `auth.test` established, plus the scopes
/// the token actually carries.
///
/// Under the one-app posture the granted set INCLUDES write scopes and the
/// collector cannot refuse them (design 3.1). So it records them: the operator
/// reading status sees exactly what the shared credential can do, which is the
/// honest substitute for an assertion this posture cannot make. What it does
/// refuse is a MISSING read scope, because that is the failure that would
/// otherwise present as an empty, healthy-looking corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackIdentity {
    pub workspace_id: String,
    pub workspace_domain: Option<String>,
    pub workspace_name: Option<String>,
    pub bot_user_id: Option<String>,
    pub bot_id: Option<String>,
    /// Every scope the token carries, read scopes and write scopes alike,
    /// sorted and deduplicated.
    pub granted_scopes: Vec<String>,
}

impl SlackIdentity {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.granted_scopes.iter().any(|granted| granted == scope)
    }

    /// The write scopes the shared credential carries.
    ///
    /// Reported, never refused. This is the number an operator watches to know
    /// what the one-app posture actually costs them.
    pub fn write_scopes(&self) -> Vec<String> {
        self.granted_scopes
            .iter()
            .filter(|scope| is_write_scope(scope))
            .cloned()
            .collect()
    }
}

/// A conservative write-scope classifier for the status surface.
///
/// Deliberately over-inclusive rather than exact: it drives a REPORT, never a
/// refusal, so a false positive costs an operator one line of extra honesty and
/// a false negative would understate what the shared credential can do.
fn is_write_scope(scope: &str) -> bool {
    const WRITE_MARKERS: &[&str] = &[
        ":write",
        "chat:",
        "files:write",
        "views:",
        "commands",
        "im:write",
        "reactions:write",
    ];
    WRITE_MARKERS.iter().any(|marker| scope.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_next_cursor_is_not_a_cursor() {
        let metadata = ResponseMetadata {
            next_cursor: Some(String::new()),
        };
        assert_eq!(metadata.cursor(), None);
    }

    #[test]
    fn a_parent_is_not_its_own_reply() {
        let parent = RawMessage {
            ts: Some("1755000000.000100".into()),
            thread_ts: Some("1755000000.000100".into()),
            reply_count: Some(2),
            ..blank()
        };
        assert!(parent.is_thread_parent());
        assert_eq!(parent.reply_parent(), None);

        let reply = RawMessage {
            ts: Some("1755000009.000200".into()),
            thread_ts: Some("1755000000.000100".into()),
            ..blank()
        };
        assert!(!reply.is_thread_parent());
        assert_eq!(reply.reply_parent(), Some("1755000000.000100"));
    }

    #[test]
    fn write_scopes_are_reported_not_refused() {
        let identity = SlackIdentity {
            workspace_id: "T0FIXTURE".into(),
            workspace_domain: None,
            workspace_name: None,
            bot_user_id: None,
            bot_id: None,
            granted_scopes: vec![
                "channels:history".into(),
                "channels:read".into(),
                "chat:write".into(),
            ],
        };
        assert_eq!(identity.write_scopes(), vec!["chat:write".to_string()]);
        assert!(identity.has_scope("channels:history"));
    }

    fn blank() -> RawMessage {
        RawMessage {
            kind: None,
            subtype: None,
            ts: None,
            user: None,
            bot_id: None,
            username: None,
            text: None,
            thread_ts: None,
            reply_count: None,
            latest_reply: None,
            edited: None,
            reactions: Vec::new(),
            files: Vec::new(),
        }
    }
}
