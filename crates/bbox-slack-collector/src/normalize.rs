//! Slack message JSON to [`ConversationMessageRecordV1`], and nothing else.
//!
//! Producer discipline (design 3.2): this ships RECORDS, not documents. It does
//! not concatenate messages, choose thread windows, summarize, render markdown,
//! resolve `<@U123>` mentions into names, or embed. `text` crosses the wire
//! exactly as Slack returned it, because every document-shaping decision stays
//! corpus-side so exactly one chunker version exists in the system.
//!
//! # What is dropped, and why each drop is COUNTED
//!
//! Three classes of message do not become records:
//!
//! - **Structural channel noise** ([`SKIPPED_SUBTYPES`]): joins, leaves, topic
//!   and purpose changes, pins, archive notices. They are events about a
//!   channel rather than turns in a conversation, they are numerous, and
//!   indexing them would dilute every search over the channels they clutter.
//!   Each is counted under its own subtype so an operator can see exactly what
//!   was dropped rather than reading a single opaque total.
//! - **Unattributable messages**: no `user` and no `bot_id`. The wire requires
//!   a non-empty `author_id` and inventing one would put a fabricated identity
//!   into durable corpus state.
//! - **Text over the wire's cap**: the wire refuses it and truncating would be
//!   a silent lie about what the workspace holds, so the record is skipped and
//!   counted loudly.
//!
//! An UNKNOWN subtype is kept, deliberately. Slack adds subtypes; treating
//! unknown as noise would silently drop a future message class, while treating
//! it as content at worst indexes something structural until somebody adds it
//! to the list.
//!
//! # Author kind without a `users.info` call
//!
//! [`AuthorKindV1`] is decided from message structure alone: a `bot_id` or the
//! `bot_message` subtype means an app, a bare `user` means a human. That is why
//! v1 never calls `users.list` or `users.info` even though both are
//! allowlisted, and why it needs no `users:read` scope. The record shape carries
//! no display name (identity is the opaque `author_id`, per design 4.2), so a
//! user lookup would spend rate budget on a field that has nowhere to land.

use bbox_conversation_source::{
    AttachmentRefV1, AuthorKindV1, ConversationMessageRecordV1, MAX_ATTACHMENTS_PER_RECORD,
    MAX_REACTIONS_PER_RECORD, MAX_SUBTYPE_BYTES, MAX_TEXT_BYTES, ReactionRefV1,
    validate_message_ts,
};

use crate::policy::SkipCounters;
use crate::slack::RawMessage;

/// Structural subtypes that are events about a channel, not turns in it.
pub const SKIPPED_SUBTYPES: &[&str] = &[
    "channel_join",
    "channel_leave",
    "group_join",
    "group_leave",
    "channel_topic",
    "channel_purpose",
    "channel_name",
    "channel_archive",
    "channel_unarchive",
    "group_archive",
    "group_unarchive",
    "channel_convert_to_private",
    "channel_convert_to_public",
    "pinned_item",
    "unpinned_item",
    "bot_add",
    "bot_remove",
    "tombstone",
];

pub const REASON_NO_TS: &str = "no_timestamp";
pub const REASON_INVALID_TS: &str = "invalid_timestamp";
pub const REASON_UNATTRIBUTED: &str = "unattributed";
pub const REASON_TEXT_OVER_CAP: &str = "text_over_cap";
pub const REASON_SUBTYPE_TOO_LONG: &str = "subtype_too_long";
pub const REASON_REACTIONS_TRUNCATED: &str = "reactions_truncated";
pub const REASON_ATTACHMENTS_TRUNCATED: &str = "attachments_truncated";

/// Normalize one message, counting every drop.
///
/// `observed_at` is supplied rather than read from the clock inside so a test
/// can pin it; on the wire it is opaque, bounded, diagnostic text.
pub fn normalize(
    channel_id: &str,
    message: &RawMessage,
    observed_at: &str,
    counters: &mut SkipCounters,
) -> Option<ConversationMessageRecordV1> {
    if let Some(subtype) = message.subtype.as_deref()
        && SKIPPED_SUBTYPES.contains(&subtype)
    {
        // Bounded cardinality: only listed subtypes produce a counter key, so
        // a workspace inventing subtypes cannot inflate the counter map.
        counters.record(&format!("subtype_{subtype}"));
        return None;
    }
    let Some(ts) = message.ts.as_deref() else {
        counters.record(REASON_NO_TS);
        return None;
    };
    if validate_message_ts(ts).is_err() {
        counters.record(REASON_INVALID_TS);
        return None;
    }
    let Some(author_id) = author_id(message) else {
        counters.record(REASON_UNATTRIBUTED);
        return None;
    };
    let text = message.text.clone().unwrap_or_default();
    if text.len() > MAX_TEXT_BYTES {
        // Truncating would be a silent lie about what the workspace holds, and
        // the wire would refuse the record anyway.
        counters.record(REASON_TEXT_OVER_CAP);
        return None;
    }

    let thread_parent_ts = message
        .reply_parent()
        .filter(|parent| validate_message_ts(parent).is_ok())
        .map(str::to_string);

    let subtype = match message.subtype.as_deref() {
        Some(subtype) if subtype.len() > MAX_SUBTYPE_BYTES => {
            counters.record(REASON_SUBTYPE_TOO_LONG);
            None
        }
        Some(subtype) => Some(subtype.to_string()),
        None => None,
    };

    let mut reactions: Vec<ReactionRefV1> = message
        .reactions
        .iter()
        .filter_map(|reaction| {
            let name = reaction.name.clone()?;
            (!name.is_empty()).then_some(ReactionRefV1 {
                name,
                count: reaction.count,
            })
        })
        .collect();
    if reactions.len() > MAX_REACTIONS_PER_RECORD {
        reactions.truncate(MAX_REACTIONS_PER_RECORD);
        counters.record(REASON_REACTIONS_TRUNCATED);
    }

    let mut attachments: Vec<AttachmentRefV1> = message
        .files
        .iter()
        .filter_map(|file| {
            // A file with no id is not a reference to anything a later blob
            // pass could fetch, so it is not carried.
            let remote_id = file.id.clone()?;
            Some(AttachmentRefV1 {
                remote_id,
                name: file.name.clone(),
                mime_type: file.mimetype.clone(),
                size: file.size,
            })
        })
        .collect();
    if attachments.len() > MAX_ATTACHMENTS_PER_RECORD {
        attachments.truncate(MAX_ATTACHMENTS_PER_RECORD);
        counters.record(REASON_ATTACHMENTS_TRUNCATED);
    }

    Some(ConversationMessageRecordV1 {
        channel_id: channel_id.to_string(),
        message_ts: ts.to_string(),
        // Always zero here. A revision is an EDIT observed against an
        // already-landed record and it travels on the revisions verb; a sweep
        // never mints one, because a sweep cannot tell whether the corpus
        // already holds this message.
        revision: 0,
        author_id,
        author_kind: author_kind(message),
        thread_parent_ts,
        subtype,
        text,
        edited_ts: message
            .edited
            .as_ref()
            .and_then(|edited| edited.ts.clone()),
        reactions,
        attachments,
        observed_at: observed_at.to_string(),
    })
}

/// The opaque author id: a user, or the app that spoke.
fn author_id(message: &RawMessage) -> Option<String> {
    message
        .user
        .clone()
        .filter(|user| !user.is_empty())
        .or_else(|| message.bot_id.clone().filter(|bot| !bot.is_empty()))
}

fn author_kind(message: &RawMessage) -> AuthorKindV1 {
    if message.bot_id.is_some() || message.subtype.as_deref() == Some("bot_message") {
        return AuthorKindV1::App;
    }
    match &message.user {
        Some(user) if !user.is_empty() => AuthorKindV1::Human,
        _ => AuthorKindV1::Unknown,
    }
}

/// The provider's edit stamp, when the message carries one.
///
/// This is the value the reconciliation baseline compares against: a message
/// returns from a resweep with its ORIGINAL `ts` and a MOVED edit stamp, which
/// is the only signal Slack gives that a landed record is now wrong (design
/// 5.4).
pub fn edit_stamp(message: &RawMessage) -> Option<&str> {
    message.edited.as_ref()?.ts.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::{RawEdited, RawFile, RawReaction};

    const CHANNEL: &str = "C0FIXTURE01";
    const OBSERVED_AT: &str = "2026-08-13T00:00:00Z";

    fn message(ts: &str, text: &str) -> RawMessage {
        RawMessage {
            kind: Some("message".into()),
            subtype: None,
            ts: Some(ts.into()),
            user: Some("U0HUMAN".into()),
            bot_id: None,
            username: None,
            text: Some(text.into()),
            thread_ts: None,
            reply_count: None,
            latest_reply: None,
            edited: None,
            reactions: Vec::new(),
            files: Vec::new(),
        }
    }

    fn normalize_one(message: &RawMessage) -> Option<ConversationMessageRecordV1> {
        let mut counters = SkipCounters::default();
        normalize(CHANNEL, message, OBSERVED_AT, &mut counters)
    }

    #[test]
    fn a_plain_message_normalizes_and_validates_against_the_wire() {
        let record = normalize_one(&message("1755000000.000100", "hello")).unwrap();
        record.validate().unwrap();
        assert_eq!(record.channel_id, CHANNEL);
        assert_eq!(record.author_id, "U0HUMAN");
        assert_eq!(record.author_kind, AuthorKindV1::Human);
        assert_eq!(record.revision, 0);
        assert_eq!(record.text, "hello");
        assert_eq!(record.thread_parent_ts, None);
    }

    #[test]
    fn raw_text_is_never_rendered_or_resolved() {
        // Mention syntax, links, and formatting cross the wire untouched. The
        // corpus owns rendering; a producer that "helpfully" resolved a mention
        // here would put a display name into an identity-opaque record.
        let raw = "<@U0HUMAN> see <https://example.com|the doc> *now*";
        let record = normalize_one(&message("1755000000.000100", raw)).unwrap();
        assert_eq!(record.text, raw);
    }

    #[test]
    fn a_reply_carries_its_parent_and_a_parent_does_not() {
        let mut reply = message("1755000009.000200", "in thread");
        reply.thread_ts = Some("1755000000.000100".into());
        assert_eq!(
            normalize_one(&reply).unwrap().thread_parent_ts,
            Some("1755000000.000100".to_string())
        );

        let mut parent = message("1755000000.000100", "thread starter");
        parent.thread_ts = Some("1755000000.000100".into());
        parent.reply_count = Some(3);
        assert_eq!(normalize_one(&parent).unwrap().thread_parent_ts, None);
    }

    #[test]
    fn a_bot_message_is_an_app_author() {
        let mut bot = message("1755000000.000100", "deploy finished");
        bot.user = None;
        bot.bot_id = Some("B0APP".into());
        bot.subtype = Some("bot_message".into());
        let record = normalize_one(&bot).unwrap();
        assert_eq!(record.author_kind, AuthorKindV1::App);
        assert_eq!(record.author_id, "B0APP");
        assert_eq!(record.subtype.as_deref(), Some("bot_message"));
    }

    #[test]
    fn structural_subtypes_are_skipped_and_counted_by_name() {
        let mut counters = SkipCounters::default();
        for subtype in ["channel_join", "channel_topic", "pinned_item"] {
            let mut noise = message("1755000000.000100", "");
            noise.subtype = Some(subtype.into());
            assert!(normalize(CHANNEL, &noise, OBSERVED_AT, &mut counters).is_none());
            assert_eq!(counters.get(&format!("subtype_{subtype}")), 1);
        }
    }

    #[test]
    fn an_unknown_subtype_is_kept_rather_than_guessed_at() {
        let mut future = message("1755000000.000100", "something new");
        future.subtype = Some("some_future_subtype".into());
        let record = normalize_one(&future).unwrap();
        assert_eq!(record.subtype.as_deref(), Some("some_future_subtype"));
    }

    #[test]
    fn an_unattributable_message_is_skipped_rather_than_given_an_invented_author() {
        let mut orphan = message("1755000000.000100", "who said this");
        orphan.user = None;
        orphan.bot_id = None;
        let mut counters = SkipCounters::default();
        assert!(normalize(CHANNEL, &orphan, OBSERVED_AT, &mut counters).is_none());
        assert_eq!(counters.get(REASON_UNATTRIBUTED), 1);
    }

    #[test]
    fn oversize_text_is_skipped_rather_than_truncated() {
        let huge = "x".repeat(MAX_TEXT_BYTES + 1);
        let mut counters = SkipCounters::default();
        assert!(
            normalize(
                CHANNEL,
                &message("1755000000.000100", &huge),
                OBSERVED_AT,
                &mut counters
            )
            .is_none()
        );
        assert_eq!(counters.get(REASON_TEXT_OVER_CAP), 1);
    }

    #[test]
    fn a_malformed_timestamp_never_reaches_the_wire() {
        let mut counters = SkipCounters::default();
        let mut broken = message("not-a-timestamp", "hello");
        assert!(normalize(CHANNEL, &broken, OBSERVED_AT, &mut counters).is_none());
        assert_eq!(counters.get(REASON_INVALID_TS), 1);

        broken.ts = None;
        assert!(normalize(CHANNEL, &broken, OBSERVED_AT, &mut counters).is_none());
        assert_eq!(counters.get(REASON_NO_TS), 1);
    }

    #[test]
    fn reactions_and_files_cross_as_references_only() {
        let mut rich = message("1755000000.000100", "here it is");
        rich.reactions = vec![RawReaction {
            name: Some("eyes".into()),
            count: 3,
        }];
        rich.files = vec![
            RawFile {
                id: Some("F0FIXTURE".into()),
                name: Some("plan.md".into()),
                mimetype: Some("text/markdown".into()),
                size: Some(12),
            },
            // No id: not a reference to anything, so not carried.
            RawFile {
                id: None,
                name: Some("ghost".into()),
                mimetype: None,
                size: None,
            },
        ];
        let record = normalize_one(&rich).unwrap();
        record.validate().unwrap();
        assert_eq!(record.reactions.len(), 1);
        assert_eq!(record.reactions[0].count, 3);
        assert_eq!(record.attachments.len(), 1);
        assert_eq!(record.attachments[0].remote_id, "F0FIXTURE");
    }

    #[test]
    fn an_edit_stamp_is_carried_but_is_not_freshness() {
        let mut edited = message("1755000000.000100", "fixed typo");
        edited.edited = Some(RawEdited {
            user: Some("U0HUMAN".into()),
            ts: Some("1755000100.000000".into()),
        });
        let record = normalize_one(&edited).unwrap();
        assert_eq!(record.edited_ts.as_deref(), Some("1755000100.000000"));
        // Revision remains zero: a sweep observes, it does not adjudicate
        // whether the corpus already holds an older body.
        assert_eq!(record.revision, 0);
        assert_eq!(edit_stamp(&edited), Some("1755000100.000000"));
    }
}
