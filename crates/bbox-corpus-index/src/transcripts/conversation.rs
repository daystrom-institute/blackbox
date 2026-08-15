//! Transcript adapter for connector-landed conversations (the Slack lane).
//!
//! Design: `design/connectors/slack-ingestion-connector.md`, sections 4.3
//! (corpus-side landing) and 7 (retrieval).
//!
//! # Why this is an adapter and not a pipeline
//!
//! Landed records feed the EXISTING transcript projection, so exactly one code
//! path builds conversation documents. This module owns the read half of that:
//! it serves [`TranscriptReadAdapter`] out of
//! [`ConversationSourceStore`]'s per-channel journal, and every document is
//! then built by `projection::normalized_to_doc` like any other transcript
//! event. The rejected alternative (drive the normalized-event projection
//! directly, outside the registry) buys nothing but drift.
//!
//! # What a location is here
//!
//! One location per `(workspace_id, channel_id)`. Its `path` is the channel's
//! journal, because change detection needs bytes to stat; its IDENTITY is
//! `slack:<workspace_id>/<channel_id>`, carried in
//! [`TranscriptLocation::logical_key`] and read back through
//! [`TranscriptLocation::locator`]. That split is the non-path location
//! fingerprint design 4.3 calls for: the journal path contains two one-way
//! hashes and moves with the daemon state dir, so identifying a channel by it
//! would purge and re-index the whole corpus the first time an operator
//! relocated their state.
//!
//! # Bucketing: per channel, per day, thread-anchored
//!
//! Each message projects to its OWN document, exactly as one transcript event
//! does; the bucketing question is which `session_id` groups them, since that
//! is the transcript corpus's unit of "one readable run of turns".
//!
//! A message buckets under `<channel_id>/<UTC date of its anchor>`, where the
//! anchor is the thread parent's timestamp for a reply and the message's own
//! timestamp otherwise. Two reasons for that shape over thread-rooted
//! bucketing:
//!
//! 1. Most channel messages are not in a thread. Thread-rooted bucketing would
//!    put the majority of a workspace into singleton buckets, which is not a
//!    conversation, while a day of a channel reads like a session: bounded,
//!    chronological, and pageable.
//! 2. A bucket identity must not depend on facts that arrive later. Under
//!    thread-rooted bucketing a message's bucket would change the day someone
//!    replied to it; here it is a pure function of two immutable fields on the
//!    record, so a reprojection over the same journal produces the same
//!    documents (the S2 determinism gate).
//!
//! The thread anchor is what keeps a reply searchable under its parent's
//! context: a reply lands in the parent's bucket even when it arrives days
//! later, and carries `thread_parent_ts` as an indexed field besides.
//!
//! # Enrollment and purge
//!
//! A channel is projected when an enrolled conversation-lane scope covers it:
//! the scope is in the daemon's connector grants, and the channel's latest
//! roster observation says the observing identity is still a member. Drop
//! either and the channel stops being emitted by [`scan_locations`], its
//! locator leaves the reindex scan set, and the purge phase deletes its
//! documents by that term. See [`ConversationTranscriptAdapter`] for the one
//! enrollment signal this lane does NOT yet expose.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use bbox_conversation_source::{AuthorKindV1, message_ts_order_key};
use bbox_conversation_source_store::{ConversationSourceStore, LandedMessageV1};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bro_transcript::MessageRole;

use super::adapters::{TranscriptReadAdapter, TranscriptScanTarget};
use super::types::{
    ConversationAuthorKind, ConversationProvenance, NormalizedTranscriptEvent, RawTranscriptRef,
    TranscriptBatch, TranscriptCursor, TranscriptEventKind, TranscriptLocation,
    TranscriptReadError, TranscriptSnapshot, TranscriptSource, TranscriptStorage,
};

/// One conversation-lane connector source the corpus is allowed to project.
///
/// The operator's config is the authority for both halves: `scope` is the
/// grant, and `remote_authority` is the vendor tenant that grant declares.
/// Nothing here is producer-supplied, which is why an unenrolled source
/// disappears from the projection by the operator editing config rather than
/// by a producer deciding to stop reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSourceEnrollmentV1 {
    pub scope: ConnectorScope,
    /// The vendor tenant (`acme.slack.com`, or bare `acme`). The only input
    /// permalink derivation needs beyond the record itself.
    pub remote_authority: String,
}

/// Adapter over every enrolled conversation source under one landing store.
///
/// One instance covers all enrolled scopes rather than one instance per scope,
/// because a channel visible to two observers must produce ONE document set,
/// not two: design 4.2 keys message identity on the workspace precisely so a
/// search hit count does not vary with how many bots happen to be in a
/// channel. Scopes are visited in `connector_source_id` order and the first
/// one covering a channel owns its projection, which makes the choice
/// deterministic and keeps the coverage invariant intact: unenrolling one
/// observer hands the channel to the next covering scope rather than removing
/// documents another observer still covers.
///
/// **The enrollment signal.** Coverage ends when the roster reports
/// `is_member: false` or when the operator removes the grant; this adapter
/// reads only that signal and does not care how it got there.
///
/// `record_roster` writes the latest observation per channel and never
/// deletes one on its own, so a producer that just stops listing a channel
/// (an ordinary partial roster) leaves that channel's last observation on
/// disk unchanged, and it keeps projecting. That is deliberate: treating
/// every roster POST as a complete replacement set would let one partial
/// roster from a mid-restart producer purge a workspace.
///
/// A producer that CAN prove a roster is its entire current member set for
/// the workspace says so on the wire (`ChannelRosterRequestV1::complete`),
/// and `ConversationSourceStore::record_roster` then records `is_member:
/// false` for every channel it previously held that the sweep omitted (see
/// that method's doc for the full contract, including idempotency under
/// replay). This adapter needs no separate handling for that case: it already
/// treats an `is_member: false` observation, however it was produced, as
/// coverage ended. Re-enrollment (the channel reappearing in a later roster)
/// is symmetric for the same reason: the next `is_member: true` observation
/// makes the channel covered again with no adapter-side state to reset.
///
/// The gap this closes is `gap-e84231d3`. It is still possible for a channel
/// to keep projecting after real-world coverage ended: a producer that never
/// posts a complete roster (an old, unredeployed satellite, or one whose
/// enumeration this cycle was provably partial) still leaves a stale
/// observation in place until an explicit `is_member: false` arrives or the
/// operator removes the grant, exactly as before.
pub struct ConversationTranscriptAdapter {
    store: ConversationSourceStore,
    sources: Vec<ConversationSourceEnrollmentV1>,
}

/// A channel one enrolled scope covers.
struct CoveredChannel {
    workspace_id: String,
    channel_id: String,
    channel_name: Option<String>,
    remote_authority: String,
    journal: PathBuf,
}

impl ConversationTranscriptAdapter {
    /// Open the adapter over `root` (the daemon's `conversation-sources` state
    /// dir). Returns `None` when nothing is enrolled, so a daemon with no
    /// conversation grants registers no adapter at all rather than one that
    /// scans an empty store on every pass.
    pub fn open(root: &Path, sources: Vec<ConversationSourceEnrollmentV1>) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        let store = ConversationSourceStore::open(root).ok()?;
        let mut sources = sources;
        // Deterministic covering order. Every dedupe decision below depends on
        // it, so it is established once here rather than trusted from the
        // caller's config file ordering.
        sources.sort_by(|a, b| {
            a.scope
                .connector_source_id()
                .as_str()
                .cmp(b.scope.connector_source_id().as_str())
        });
        Some(Self { store, sources })
    }

    /// Every channel an enrolled scope currently covers, first covering scope
    /// wins, in stable `(workspace, channel)` order.
    fn covered_channels(&self) -> Result<Vec<CoveredChannel>, TranscriptReadError> {
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        let mut out: BTreeMap<(String, String), CoveredChannel> = BTreeMap::new();
        for source in &self.sources {
            let Some(binding) = self
                .store
                .workspace_binding(&source.scope)
                .map_err(|error| store_error("workspace_binding", error))?
            else {
                // Granted but never onboarded: nothing is bound, so nothing
                // can be projected. Not an error.
                continue;
            };
            let channels = self
                .store
                .channels(&source.scope)
                .map_err(|error| store_error("channels", error))?;
            for channel_id in channels {
                let key = (binding.workspace_id.clone(), channel_id.clone());
                if seen.contains(&key) {
                    continue;
                }
                let observation = self
                    .store
                    .roster(&source.scope, &binding.workspace_id, &channel_id)
                    .map_err(|error| store_error("roster", error))?;
                // Coverage ended: the observing identity is no longer a member,
                // so this scope's claim on the channel is gone. Skipping it
                // here is what makes the reindex purge its documents.
                if observation
                    .as_ref()
                    .is_some_and(|observation| !observation.is_member)
                {
                    continue;
                }
                let journal = self.store.channel_journal_path(
                    &source.scope,
                    &binding.workspace_id,
                    &channel_id,
                );
                if !journal.is_file() {
                    // Rostered but nothing has landed yet. Emitting a location
                    // with no bytes would make the freshness row unstattable
                    // and the channel purge-exposed on alternating passes.
                    continue;
                }
                seen.insert(key.clone());
                out.insert(
                    key,
                    CoveredChannel {
                        workspace_id: binding.workspace_id.clone(),
                        channel_id,
                        channel_name: observation.and_then(|observation| observation.observed_name),
                        remote_authority: source.remote_authority.clone(),
                        journal,
                    },
                );
            }
        }
        Ok(out.into_values().collect())
    }

    /// The covering channel for a location, re-derived by the same rule
    /// `covered_channels` used. Re-derived rather than remembered so a
    /// location that outlived an enrollment change resolves against what is
    /// enrolled NOW.
    fn covering(
        &self,
        location: &TranscriptLocation,
    ) -> Result<Option<(ConnectorScope, CoveredChannel)>, TranscriptReadError> {
        let (workspace_id, channel_id) = match (&location.account, &location.session_id) {
            (Some(workspace_id), Some(channel_id)) => (workspace_id, channel_id),
            _ => {
                return Err(TranscriptReadError::InvalidLocation {
                    source: TranscriptSource::Slack,
                    path: location.path.clone(),
                    reason: "a conversation location must carry its workspace and channel",
                });
            }
        };
        for source in &self.sources {
            let Some(binding) = self
                .store
                .workspace_binding(&source.scope)
                .map_err(|error| store_error("workspace_binding", error))?
            else {
                continue;
            };
            if &binding.workspace_id != workspace_id {
                continue;
            }
            let journal =
                self.store
                    .channel_journal_path(&source.scope, &binding.workspace_id, channel_id);
            if !journal.is_file() {
                continue;
            }
            let observation = self
                .store
                .roster(&source.scope, &binding.workspace_id, channel_id)
                .map_err(|error| store_error("roster", error))?;
            if observation
                .as_ref()
                .is_some_and(|observation| !observation.is_member)
            {
                continue;
            }
            return Ok(Some((
                source.scope.clone(),
                CoveredChannel {
                    workspace_id: binding.workspace_id.clone(),
                    channel_id: channel_id.clone(),
                    channel_name: observation.and_then(|observation| observation.observed_name),
                    remote_authority: source.remote_authority.clone(),
                    journal,
                },
            )));
        }
        Ok(None)
    }

    fn location_for(&self, channel: &CoveredChannel) -> TranscriptLocation {
        TranscriptLocation {
            source: TranscriptSource::Slack,
            storage: TranscriptStorage::LandedRecords,
            // Bytes to stat, never identity. See the module doc.
            path: channel.journal.clone(),
            account: Some(channel.workspace_id.clone()),
            session_id: Some(channel.channel_id.clone()),
            // A conversation has no checkout, so it deliberately matches no
            // project filter rather than pretending a channel is a project.
            project: None,
            cwd: None,
            is_subagent: false,
            logical_key: Some(channel_locator(&channel.workspace_id, &channel.channel_id)),
        }
    }
}

/// Channel ids whose CURRENT roster observation carries `name`, across every
/// enrolled scope under `root`.
///
/// The search channel filter resolves names through here so a renamed channel
/// still matches its WHOLE history by the stable channel id, even though its
/// older documents were stamped with the name observed when they were indexed.
/// Best-effort by design: an unreadable store or an unknown name returns
/// empty, and the caller's name-stamp lane still matches whatever documents
/// literally carry the queried name.
pub fn rostered_channel_ids_named(
    root: &Path,
    sources: &[ConversationSourceEnrollmentV1],
    name: &str,
) -> Vec<String> {
    let Some(adapter) = ConversationTranscriptAdapter::open(root, sources.to_vec()) else {
        return Vec::new();
    };
    let Ok(channels) = adapter.covered_channels() else {
        return Vec::new();
    };
    channels
        .into_iter()
        .filter(|channel| {
            channel
                .channel_name
                .as_deref()
                .is_some_and(|observed| observed.eq_ignore_ascii_case(name))
        })
        .map(|channel| channel.channel_id)
        .collect()
}

impl TranscriptReadAdapter for ConversationTranscriptAdapter {
    fn source(&self) -> TranscriptSource {
        TranscriptSource::Slack
    }

    /// No runtime handle exists: the daemon never dispatches to a workspace,
    /// so there is no session id a caller could hold.
    fn locate(&self, _session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        Ok(None)
    }

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError> {
        if target == TranscriptScanTarget::History {
            return Ok(Vec::new());
        }
        Ok(self
            .covered_channels()?
            .iter()
            .map(|channel| self.location_for(channel))
            .collect())
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        // A byte offset into a journal is not a position in a conversation:
        // the fold that produces landed state replays the whole journal, and a
        // revision that supersedes an early message is appended late. Refusing
        // is right; silently ignoring the cursor would report a partial read
        // as a complete one.
        if let Some(cursor) = cursor {
            return Err(TranscriptReadError::UnsupportedCursor {
                source: TranscriptSource::Slack,
                cursor: cursor.clone(),
            });
        }
        let Some((scope, channel)) = self.covering(location)? else {
            // Enrollment ended between the scan and the read. An empty batch
            // is the honest answer: the caller has already deleted this
            // location's documents by term and will not re-add any.
            return Ok(TranscriptBatch {
                location: location.clone(),
                events: Vec::new(),
                cursor: None,
                reached_end: true,
            });
        };
        let landed = self
            .store
            .landed_messages(&scope, &channel.workspace_id, &channel.channel_id)
            .map_err(|error| store_error("landed_messages", error))?;
        let events = landed
            .iter()
            .filter_map(|message| project_message(&channel, message))
            .collect();
        Ok(TranscriptBatch {
            location: location.clone(),
            events,
            cursor: None,
            reached_end: true,
        })
    }

    fn load_snapshot(
        &self,
        location: &TranscriptLocation,
    ) -> Result<TranscriptSnapshot, TranscriptReadError> {
        let batch = self.read_since(location, None)?;
        Ok(TranscriptSnapshot {
            location: location.clone(),
            events: batch.events,
            cursor: batch.cursor,
        })
    }
}

/// The stable identity of one channel's projection.
///
/// Workspace-level, exactly like the record identity it groups: two observers
/// of one channel converge on one locator, so they converge on one document
/// set and one purge term.
pub fn channel_locator(workspace_id: &str, channel_id: &str) -> String {
    format!("slack:{workspace_id}/{channel_id}")
}

/// Project one landed message into a normalized transcript event.
///
/// Returns `None` for a tombstoned record: a confirmed deletion must leave the
/// projected document, which for a per-message document means not emitting it.
/// The journal still holds the tombstone, so the corpus can still tell a
/// redaction from a message it never saw.
fn project_message(
    channel: &CoveredChannel,
    landed: &LandedMessageV1,
) -> Option<NormalizedTranscriptEvent> {
    if landed.tombstoned {
        return None;
    }
    let record = &landed.record;
    let anchor = record
        .thread_parent_ts
        .as_deref()
        .unwrap_or(&record.message_ts);
    let author_kind = match record.author_kind {
        AuthorKindV1::Human => ConversationAuthorKind::Human,
        AuthorKindV1::App => ConversationAuthorKind::App,
        AuthorKindV1::Unknown => ConversationAuthorKind::Unknown,
    };
    let entity_id = message_entity_id(
        &channel.workspace_id,
        &channel.channel_id,
        &record.message_ts,
    );
    Some(NormalizedTranscriptEvent {
        source: TranscriptSource::Slack,
        role: author_kind.role(),
        kind: TranscriptEventKind::Message,
        // RAW text, as landed. Rendering belongs to no one on this path:
        // the producer does not do it (design 3.2) and neither does this.
        content: record.text.clone(),
        session_id: bucket_session_id(&channel.channel_id, anchor),
        timestamp: rfc3339_from_message_ts(&record.message_ts),
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: None,
        tool_call: None,
        raw: RawTranscriptRef::provider_event(
            TranscriptSource::Slack,
            TranscriptStorage::LandedRecords,
            &channel.journal,
            record.message_ts.clone(),
            entity_id,
        ),
        conversation: Some(ConversationProvenance {
            workspace_id: channel.workspace_id.clone(),
            channel_id: channel.channel_id.clone(),
            channel_name: channel.channel_name.clone(),
            message_ts: record.message_ts.clone(),
            thread_parent_ts: record.thread_parent_ts.clone(),
            author_id: record.author_id.clone(),
            author_kind,
            permalink: permalink(
                &channel.remote_authority,
                &channel.channel_id,
                &record.message_ts,
                record.thread_parent_ts.as_deref(),
            ),
        }),
    })
}

/// Identity of one projected message: the durable record identity, revision
/// excluded. A revision supersedes IN PLACE precisely because the entity id
/// does not move when it lands.
pub fn message_entity_id(workspace_id: &str, channel_id: &str, message_ts: &str) -> String {
    format!("slack:{workspace_id}:{channel_id}:{message_ts}")
}

/// `<channel_id>/<UTC date>`; see the module doc for why the day, and why the
/// anchor is the thread parent for a reply.
pub fn bucket_session_id(channel_id: &str, anchor_ts: &str) -> String {
    match epoch_seconds(anchor_ts) {
        Some(seconds) => {
            let (year, month, day) = civil_from_epoch_seconds(seconds);
            format!("{channel_id}/{year:04}-{month:02}-{day:02}")
        }
        // An unparseable anchor gets its own bucket rather than being dropped
        // or silently folded into a real day: it is a contract violation
        // upstream, and hiding it inside a legitimate day's conversation is
        // how it would stay hidden.
        None => format!("{channel_id}/undated"),
    }
}

/// The Slack archives permalink, derived rather than fetched.
///
/// `chat.getPermalink` is one API call per message, which is unaffordable at
/// corpus scale (design section 7), and the archive form is deterministic from
/// fields already held. `None` when the tenant or the timestamp is not a shape
/// this derivation covers: emitting a link that does not resolve would be
/// worse than emitting none, and section 7 keeps the API call as the fallback
/// for shapes the derivation does not handle.
pub fn permalink(
    remote_authority: &str,
    channel_id: &str,
    message_ts: &str,
    thread_parent_ts: Option<&str>,
) -> Option<String> {
    let authority = remote_authority.trim();
    if authority.is_empty()
        || authority.contains('/')
        || authority.contains(char::is_whitespace)
        || authority.contains(':')
    {
        return None;
    }
    // An operator may declare the tenant either way round. A bare label is a
    // workspace subdomain; anything already carrying a dot is taken as the
    // host it is.
    let host = if authority.contains('.') {
        authority.to_string()
    } else {
        format!("{authority}.slack.com")
    };
    let digits: String = message_ts.chars().filter(|c| *c != '.').collect();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if channel_id.is_empty() || !channel_id.chars().all(is_id_char) {
        return None;
    }
    let mut url = format!("https://{host}/archives/{channel_id}/p{digits}");
    if let Some(parent) = thread_parent_ts {
        // A reply's archive URL needs the thread it belongs to, or it opens
        // the channel at the parent instead of at the reply.
        url.push_str(&format!("?thread_ts={parent}&cid={channel_id}"));
    }
    Some(url)
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Whole seconds of a provider message timestamp (`1712345678.000200`).
fn epoch_seconds(message_ts: &str) -> Option<i64> {
    // Ordering already validated this shape upstream; reusing it here means
    // the projection and the store agree about what an orderable timestamp is.
    message_ts_order_key(message_ts)?;
    let seconds = message_ts.split('.').next()?;
    seconds.parse::<i64>().ok()
}

fn rfc3339_from_message_ts(message_ts: &str) -> Option<String> {
    let seconds = epoch_seconds(message_ts)?;
    let (year, month, day) = civil_from_epoch_seconds(seconds);
    let day_seconds = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60,
    );
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Civil UTC date from epoch seconds, no calendar dependency.
///
/// Hinnant's `civil_from_days`. This crate is the corpus index engine and is
/// deliberately thin on dependencies; a date conversion is not a reason to
/// pull a calendar crate into it, and the algorithm is exact for every date
/// this corpus can hold.
fn civil_from_epoch_seconds(seconds: i64) -> (i64, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn store_error(op: &'static str, error: anyhow::Error) -> TranscriptReadError {
    TranscriptReadError::Io {
        op,
        // The store composes its own paths from one-way hashes, so the useful
        // coordinate here is the operation, not a path the reader cannot act
        // on. The error's own context carries the file when there is one.
        path: PathBuf::new(),
        kind: std::io::ErrorKind::Other,
        message: error.to_string(),
    }
}

// ── The read plane (bbox_context / bbox_messages) ───────────────────────
//
// `bbox_context` and `bbox_messages` read session-transcript FILES by
// default: a byte offset into a JSONL file, or a session id resolved to
// one. A `slack:<workspace_id>/<channel_id>` locator (this module's own
// [`channel_locator`], the `file_path` a slack search hit carries) and a
// `<channel_id>/<date>` bucket (`bucket_session_id`, the `session_id` a
// slack hit carries) name no file at all — handing either to the
// filesystem reader is exactly the ENOENT / "Session not found" dead end
// gap-2d4d17da reports.
//
// The functions below are the seam `TranscriptIndex::context` and
// `::messages` call into instead of the filesystem when a locator or
// session id parses to one of those two shapes: they resolve straight
// against the landing store, render surrounding messages in channel-time
// order, and refuse with a message that NAMES a working lane
// (`bbox_search(channel=...)`) when the channel does not resolve, rather
// than letting a filesystem error render inline or bubble as an opaque
// `Err`.

/// Parse a `slack:<workspace_id>/<channel_id>` locator — the `file_path`
/// form both a slack search hit and this module's own [`channel_locator`]
/// produce. `None` for anything else, including a bare `slack:` with no
/// channel segment.
pub fn parse_channel_locator(locator: &str) -> Option<(&str, &str)> {
    let body = locator.strip_prefix("slack:")?;
    let (workspace_id, channel_id) = body.split_once('/')?;
    if workspace_id.is_empty() || channel_id.is_empty() {
        return None;
    }
    Some((workspace_id, channel_id))
}

/// Parse a `<channel_id>/<date>` session id — the per-channel-per-day
/// bucket [`bucket_session_id`] stamps. Distinguished from an arbitrary
/// slash-bearing session id by requiring the date half to look like
/// `YYYY-MM-DD` or the literal `undated` fallback bucket, which is narrow
/// enough that no other session id shape in this corpus collides with it.
pub fn parse_session_bucket(session_id: &str) -> Option<(&str, &str)> {
    let (channel_id, date) = session_id.split_once('/')?;
    if channel_id.is_empty() || !looks_like_date_bucket(date) {
        return None;
    }
    Some((channel_id, date))
}

fn looks_like_date_bucket(date: &str) -> bool {
    if date == "undated" {
        return true;
    }
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// The `byte_offset` `bbox_context` borrows to name one message: the same
/// digit-concatenation the archive permalink derives (see [`permalink`]
/// above), so two different messages never collide as long as their
/// timestamps differ. `None` for a `message_ts` shape `message_ts_order_key`
/// would refuse.
pub fn message_ts_digits(message_ts: &str) -> Option<u64> {
    message_ts_order_key(message_ts)?;
    let digits: String = message_ts.chars().filter(|c| *c != '.').collect();
    digits.parse::<u64>().ok()
}

fn not_configured_message() -> String {
    "This server has no Slack conversation source configured, so \
     bbox_context/bbox_messages cannot resolve a slack: locator here. \
     bbox_search still works regardless (source=\"slack\" or channel=\"...\" \
     filters)."
        .to_string()
}

fn unknown_channel_message(channel_id: &str) -> String {
    format!(
        "Channel {channel_id} is not indexed on this server right now (not \
         enrolled, never onboarded, or the observing identity is no longer a \
         member). Try bbox_search(channel=\"{channel_id}\") or \
         bbox_search(source=\"slack\") to find indexed channels."
    )
}

/// What resolving a channel against the landing store produced: either a
/// covering adapter and location ready to read, or a refusal message that
/// already names a working lane.
enum ChannelResolution {
    Ready(ConversationTranscriptAdapter, TranscriptLocation),
    Refused(String),
}

/// Resolve a channel to a covering adapter + read location, or a refusal.
/// Shared by every read-plane entry point below: all of them need the same
/// covering-scope answer before they can read anything.
fn open_covered_channel(
    conversation_source_root: Option<&Path>,
    sources: &[ConversationSourceEnrollmentV1],
    workspace_id: &str,
    channel_id: &str,
) -> Result<ChannelResolution> {
    let Some(root) = conversation_source_root else {
        return Ok(ChannelResolution::Refused(not_configured_message()));
    };
    let Some(adapter) = ConversationTranscriptAdapter::open(root, sources.to_vec()) else {
        return Ok(ChannelResolution::Refused(not_configured_message()));
    };
    let location = TranscriptLocation {
        source: TranscriptSource::Slack,
        storage: TranscriptStorage::LandedRecords,
        path: PathBuf::new(),
        account: Some(workspace_id.to_string()),
        session_id: Some(channel_id.to_string()),
        project: None,
        cwd: None,
        is_subagent: false,
        logical_key: Some(channel_locator(workspace_id, channel_id)),
    };
    if adapter.covering(&location)?.is_none() {
        return Ok(ChannelResolution::Refused(unknown_channel_message(
            channel_id,
        )));
    }
    Ok(ChannelResolution::Ready(adapter, location))
}

/// Resolve a bare channel id to the workspace an enrolled source currently
/// covers it under. Used by the `bbox_messages(session_id=...)` form, whose
/// session id carries only the channel id: the daemon has to find which
/// workspace binding that id belongs to before it can read anything.
pub fn workspace_for_channel(
    conversation_source_root: Option<&Path>,
    sources: &[ConversationSourceEnrollmentV1],
    channel_id: &str,
) -> Option<String> {
    let adapter = ConversationTranscriptAdapter::open(conversation_source_root?, sources.to_vec())?;
    let channels = adapter.covered_channels().ok()?;
    channels
        .into_iter()
        .find(|channel| channel.channel_id == channel_id)
        .map(|channel| channel.workspace_id)
}

/// Every live message (tombstones dropped) a covered channel currently
/// holds, in ascending channel-time order — the actual send time, not the
/// thread-anchor bucket search groups them under, since a context window
/// can legitimately span midnight or a thread reply landing days later.
fn ordered_messages(
    adapter: &ConversationTranscriptAdapter,
    location: &TranscriptLocation,
) -> Result<Vec<NormalizedTranscriptEvent>> {
    let mut events = adapter.load_snapshot(location)?.events;
    events.sort_by_cached_key(|event| {
        event
            .conversation
            .as_ref()
            .and_then(|provenance| message_ts_order_key(&provenance.message_ts))
            .unwrap_or_default()
    });
    Ok(events)
}

/// Shared preview truncation: `max_length == 0` means unbounded
/// (`bbox_messages`'s contract); a positive cap always applies
/// (`bbox_context`'s fixed 400-char preview, matching the file-based
/// reader).
fn truncate_preview(content: &str, max_length: usize) -> String {
    if max_length == 0 || content.len() <= max_length {
        return content.to_string();
    }
    let mut end = max_length;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &content[..end])
}

/// `bbox_context`'s per-line rendering: `>>>` marks the target message, no
/// timestamp — matching the file-based reader's own format exactly.
fn render_context_line(event: &NormalizedTranscriptEvent, is_target: bool) -> String {
    let marker = if is_target { ">>>" } else { "   " };
    let role: MessageRole = event.role.into();
    format!(
        "{marker} [{}] {}",
        role.as_ref(),
        truncate_preview(&event.content, 400)
    )
}

/// `bbox_messages`'s per-line rendering: timestamped, no marker — matching
/// the file-based reader's own format exactly.
fn render_message_line(event: &NormalizedTranscriptEvent, max_length: usize) -> String {
    let role: MessageRole = event.role.into();
    let ts = event.timestamp.as_deref().unwrap_or("");
    format!(
        "[{ts}] [{}] {}",
        role.as_ref(),
        truncate_preview(&event.content, max_length)
    )
}

/// `bbox_context` for a `slack:<workspace_id>/<channel_id>` locator: a
/// window of `context_lines` messages either side of the message whose
/// [`message_ts_digits`] equals `target`, in channel-time order. A `target`
/// that matches no live message falls back to the channel's earliest one,
/// mirroring the file-based reader's own fallback when a byte offset lands
/// past every line.
pub fn context_for_channel(
    conversation_source_root: Option<&Path>,
    sources: &[ConversationSourceEnrollmentV1],
    workspace_id: &str,
    channel_id: &str,
    target: u64,
    context_lines: usize,
) -> Result<String> {
    let (adapter, location) =
        match open_covered_channel(conversation_source_root, sources, workspace_id, channel_id)? {
            ChannelResolution::Ready(adapter, location) => (adapter, location),
            ChannelResolution::Refused(message) => return Ok(message),
        };
    let events = ordered_messages(&adapter, &location)?;
    if events.is_empty() {
        return Ok(format!("Channel {channel_id} has no landed messages yet."));
    }
    let target_idx = events
        .iter()
        .position(|event| {
            event
                .conversation
                .as_ref()
                .and_then(|provenance| message_ts_digits(&provenance.message_ts))
                == Some(target)
        })
        .unwrap_or(0);
    let start = target_idx.saturating_sub(context_lines);
    let end = (target_idx + context_lines + 1).min(events.len());

    let output: Vec<String> = events[start..end]
        .iter()
        .enumerate()
        .map(|(i, event)| render_context_line(event, start + i == target_idx))
        .collect();
    Ok(output.join("\n\n"))
}

/// `bbox_messages` for a whole channel (`file_path` form, `date_bucket =
/// None`) or one per-channel-per-day bucket (`session_id` form,
/// `date_bucket = Some(date)`). Pagination mirrors the file-based reader:
/// `offset`/`limit` page forward, `from_end` reverses so offset 0 means the
/// newest page.
#[allow(clippy::too_many_arguments)]
pub fn messages_for_channel(
    conversation_source_root: Option<&Path>,
    sources: &[ConversationSourceEnrollmentV1],
    workspace_id: &str,
    channel_id: &str,
    date_bucket: Option<&str>,
    role_filter: Option<&str>,
    max_length: usize,
    from_end: bool,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let (adapter, location) =
        match open_covered_channel(conversation_source_root, sources, workspace_id, channel_id)? {
            ChannelResolution::Ready(adapter, location) => (adapter, location),
            ChannelResolution::Refused(message) => return Ok(message),
        };
    let mut events = ordered_messages(&adapter, &location)?;
    if let Some(date) = date_bucket {
        let wanted = format!("{channel_id}/{date}");
        events.retain(|event| event.session_id == wanted);
    }
    if let Some(role) = role_filter {
        events.retain(|event| {
            let event_role: MessageRole = event.role.into();
            event_role.as_ref() == role
        });
    }

    if events.is_empty() {
        return Ok(match date_bucket {
            Some(date) => format!("No messages found for {channel_id}/{date}."),
            None => "No messages found matching filters.".to_string(),
        });
    }

    let all_messages: Vec<String> = events
        .iter()
        .map(|event| render_message_line(event, max_length))
        .collect();
    let total = all_messages.len();

    let (page, showing_start, showing_end): (Vec<&String>, usize, usize) = if from_end {
        let tail_start = total.saturating_sub(offset + limit);
        let tail_end = total.saturating_sub(offset);
        (
            all_messages[tail_start..tail_end].iter().collect(),
            tail_start,
            tail_end,
        )
    } else {
        let end = (offset + limit).min(total);
        (
            all_messages.iter().skip(offset).take(limit).collect(),
            offset,
            end,
        )
    };

    let mut header = format!(
        "Messages {}-{} of {} total",
        showing_start + 1,
        showing_end,
        total
    );
    if !from_end && showing_end < total {
        header.push_str(&format!(" (next page: offset={showing_end})"));
    }
    if from_end && showing_start > 0 {
        header.push_str(&format!(
            " (earlier: from_end=true, offset={})",
            offset + limit
        ));
    }

    const MAX_RESPONSE_BYTES: usize = 80_000;
    let mut body = String::new();
    for (included, msg) in page.iter().enumerate() {
        let entry = format!("{msg}\n\n");
        if body.len() + entry.len() > MAX_RESPONSE_BYTES {
            body.push_str(&format!(
                "[Response truncated at {included} messages — narrow with role \
                 filter, smaller limit, or a day-bucket session_id]\n"
            ));
            break;
        }
        body.push_str(&entry);
    }
    Ok(format!("{header}\n\n{}", body.trim_end()))
}

/// `bbox_messages(session_id="<channel_id>/<date>")`: resolve the channel's
/// workspace, then defer to [`messages_for_channel`]. The bucket shape
/// [`parse_session_bucket`] requires is narrow enough that an id which
/// matches it but resolves to no covered channel is confidently a slack
/// refusal rather than a coincidental non-slack session id, so this refuses
/// by name even when nothing is enrolled at all.
#[allow(clippy::too_many_arguments)]
pub fn messages_for_session_bucket(
    conversation_source_root: Option<&Path>,
    sources: &[ConversationSourceEnrollmentV1],
    channel_id: &str,
    date: &str,
    role_filter: Option<&str>,
    max_length: usize,
    from_end: bool,
    offset: usize,
    limit: usize,
) -> Result<String> {
    if conversation_source_root.is_none() {
        return Ok(not_configured_message());
    }
    let Some(workspace_id) = workspace_for_channel(conversation_source_root, sources, channel_id)
    else {
        return Ok(unknown_channel_message(channel_id));
    };
    messages_for_channel(
        conversation_source_root,
        sources,
        &workspace_id,
        channel_id,
        Some(date),
        role_filter,
        max_length,
        from_end,
        offset,
        limit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_permalink_is_derived_in_the_archives_form() {
        assert_eq!(
            permalink("acme.slack.com", "C0ABCD", "1712345678.000200", None).unwrap(),
            "https://acme.slack.com/archives/C0ABCD/p1712345678000200"
        );
        // A bare tenant label is a workspace subdomain.
        assert_eq!(
            permalink("acme", "C0ABCD", "1712345678.000200", None).unwrap(),
            "https://acme.slack.com/archives/C0ABCD/p1712345678000200"
        );
    }

    #[test]
    fn a_thread_reply_permalink_carries_the_thread_it_belongs_to() {
        let url = permalink(
            "acme.slack.com",
            "C0ABCD",
            "1712345900.000400",
            Some("1712345678.000200"),
        )
        .unwrap();
        assert_eq!(
            url,
            "https://acme.slack.com/archives/C0ABCD/p1712345900000400\
             ?thread_ts=1712345678.000200&cid=C0ABCD"
        );
    }

    #[test]
    fn a_shape_the_derivation_does_not_cover_yields_no_permalink() {
        // Better no link than one that does not resolve: section 7 keeps the
        // API call as the fallback for exactly these.
        assert_eq!(permalink("", "C0ABCD", "1712345678.000200", None), None);
        assert_eq!(
            permalink("acme.slack.com/x", "C0ABCD", "1712345678.000200", None),
            None
        );
        assert_eq!(
            permalink("acme.slack.com", "C0ABCD", "not-a-ts", None),
            None
        );
        assert_eq!(
            permalink("acme.slack.com", "C0 ABCD", "1712345678.000200", None),
            None
        );
    }

    #[test]
    fn messages_bucket_per_channel_per_utc_day() {
        // 2024-04-05T19:34:38Z and one 40 hours later.
        assert_eq!(
            bucket_session_id("C0ABCD", "1712345678.000200"),
            "C0ABCD/2024-04-05"
        );
        assert_eq!(
            bucket_session_id("C0ABCD", "1712489678.000100"),
            "C0ABCD/2024-04-07"
        );
        // One second before a UTC midnight, with the largest fraction a
        // provider can send. A fraction that leaked into the seconds would
        // roll this into the next day, which is the whole failure mode the
        // subsecond part invites.
        assert_eq!(
            bucket_session_id("C0ABCD", "1712447999.999999"),
            "C0ABCD/2024-04-06"
        );
        assert_eq!(
            bucket_session_id("C0ABCD", "garbage"),
            "C0ABCD/undated",
            "an unorderable timestamp gets its own bucket rather than hiding \
             inside a real day"
        );
    }

    #[test]
    fn a_message_timestamp_projects_to_a_utc_rfc3339_stamp() {
        assert_eq!(
            rfc3339_from_message_ts("1712345678.000200").unwrap(),
            "2024-04-05T19:34:38Z"
        );
        // THE SUBSECOND PART NEVER MOVES THE CLOCK. Same second, same
        // rendering, whatever the fraction: a provider timestamp is
        // `<seconds>.<microseconds>`, and reading the fraction as anything
        // else shifts the wall clock by exactly its numeric value. The 4800
        // here is chosen so a leak would read 20:54:38 (an 80 minute jump)
        // rather than something a reader might mistake for a time zone.
        assert_eq!(
            rfc3339_from_message_ts("1712345678.004800").unwrap(),
            "2024-04-05T19:34:38Z"
        );
        assert_eq!(
            rfc3339_from_message_ts("1712345678.999999").unwrap(),
            "2024-04-05T19:34:38Z"
        );
        assert_eq!(
            rfc3339_from_message_ts("0.000000").unwrap(),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(rfc3339_from_message_ts("nonsense"), None);
    }

    #[test]
    fn the_permalink_digits_are_a_one_way_rendering() {
        // The archives form concatenates seconds and microseconds into one
        // digit run. Nothing ever reads that run back as a time, and this
        // pins the direction: the same second with two different fractions
        // renders two different links and one identical stamp.
        assert_eq!(
            permalink("acme.slack.com", "C0ABCD", "1712345678.004800", None).unwrap(),
            "https://acme.slack.com/archives/C0ABCD/p1712345678004800"
        );
        assert_eq!(
            rfc3339_from_message_ts("1712345678.004800"),
            rfc3339_from_message_ts("1712345678.000200")
        );
    }

    #[test]
    fn an_entity_id_excludes_the_revision_so_an_edit_updates_in_place() {
        assert_eq!(
            message_entity_id("T0AB", "C0CD", "1712345678.000200"),
            "slack:T0AB:C0CD:1712345678.000200"
        );
    }

    #[test]
    fn a_channel_locator_is_workspace_level() {
        // Two observers of one channel must converge on one locator, or the
        // same message would be two documents with two purge terms.
        assert_eq!(channel_locator("T0AB", "C0CD"), "slack:T0AB/C0CD");
    }
}

#[cfg(test)]
mod gate_tests {
    //! The S2 gate, clause by clause: "a known message is findable by text and
    //! returns a correct permalink; unenrolling a channel purges its
    //! documents; a full reindex from landed records is deterministic and
    //! duplicate-free."
    //!
    //! Everything here drives REAL landed records through the real store, the
    //! real adapter, and the one real document builder, then indexes the
    //! result into a Tantivy index built from the production schema. Nothing
    //! is hand-rolled from a normalized event, because the failure these tests
    //! exist to catch is precisely a projection that agrees with itself and
    //! not with the store.

    use bbox_conversation_source::{
        AuthorKindV1, CONVERSATION_POLICY_VERSION, ChannelClassV1, ChannelObservationV1,
        ConversationBatchV1, ConversationMessageRecordV1, ConversationRevisionRecordV1,
        ConversationRevisionV1, ConversationRevisionsRequestV1, ConversationTombstoneRecordV1,
        SCHEMA_VERSION, batch_digest,
    };
    use bro_core::Provider;
    use bro_transcript::{MessageRole, ParsedEvent};
    use tantivy::collector::TopDocs;
    use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
    use tantivy::schema::IndexRecordOption;
    use tantivy::{Index, TantivyDocument, Term};

    use super::*;
    use crate::index::{FieldHandles, build_schema, first_text, register_code_tokenizer};
    use crate::transcripts::projection::normalized_to_doc;

    const WORKSPACE: &str = "T0FIXTURE";
    const CHANNEL: &str = "C0OPSFIX";
    const AUTHORITY: &str = "acme.slack.com";
    /// 2024-04-05T19:34:38Z.
    const PARENT_TS: &str = "1712345678.000200";
    /// 22 seconds later, same day.
    const APP_TS: &str = "1712345700.000100";
    /// The next day, and a reply to PARENT_TS.
    const REPLY_TS: &str = "1712432100.000300";

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new("csrc_5f2c1d9a4b6e470e", "slack").unwrap()
    }

    fn enrollment() -> ConversationSourceEnrollmentV1 {
        ConversationSourceEnrollmentV1 {
            scope: scope(),
            remote_authority: AUTHORITY.to_string(),
        }
    }

    fn record(
        message_ts: &str,
        author_id: &str,
        author_kind: AuthorKindV1,
        text: &str,
        thread_parent_ts: Option<&str>,
    ) -> ConversationMessageRecordV1 {
        ConversationMessageRecordV1 {
            channel_id: CHANNEL.to_string(),
            message_ts: message_ts.to_string(),
            revision: 0,
            author_id: author_id.to_string(),
            author_kind,
            thread_parent_ts: thread_parent_ts.map(str::to_string),
            subtype: None,
            text: text.to_string(),
            edited_ts: None,
            reactions: Vec::new(),
            attachments: Vec::new(),
            observed_at: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    fn batch(records: Vec<ConversationMessageRecordV1>) -> ConversationBatchV1 {
        ConversationBatchV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.to_string(),
            scope: scope(),
            workspace_id: WORKSPACE.to_string(),
            channel_id: CHANNEL.to_string(),
            batch_digest: batch_digest(&records),
            records,
            observed_at: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    fn revisions(revisions: Vec<ConversationRevisionV1>) -> ConversationRevisionsRequestV1 {
        ConversationRevisionsRequestV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.to_string(),
            scope: scope(),
            workspace_id: WORKSPACE.to_string(),
            channel_id: CHANNEL.to_string(),
            revisions,
            observed_at: "2026-08-13T00:10:00Z".to_string(),
        }
    }

    fn observation(is_member: bool) -> ChannelObservationV1 {
        ChannelObservationV1 {
            channel_id: CHANNEL.to_string(),
            observed_name: Some("ops-fixture".to_string()),
            class: ChannelClassV1::Public,
            is_member,
            observed_at: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    /// A landing store holding one channel of three messages: a human
    /// message, an app message, and a reply the next day to the first.
    fn landed_store(root: &Path) -> ConversationSourceStore {
        let store = ConversationSourceStore::open(root).unwrap();
        store
            .bind_workspace(&scope(), WORKSPACE, "2026-08-13T00:00:00Z")
            .unwrap();
        store
            .record_roster(
                &scope(),
                WORKSPACE,
                &[observation(true)],
                false,
                "2026-08-13T00:00:00Z",
            )
            .unwrap();
        store
            .land_batch(
                &scope(),
                &batch(vec![
                    record(
                        PARENT_TS,
                        "U0HUMAN",
                        AuthorKindV1::Human,
                        "the deploy pipeline wedged on the lock table again",
                        None,
                    ),
                    record(
                        APP_TS,
                        "B0BUILDBOT",
                        AuthorKindV1::App,
                        "build 4471 finished in 92 seconds",
                        None,
                    ),
                    record(
                        REPLY_TS,
                        "U0HUMAN",
                        AuthorKindV1::Human,
                        "we cleared the wedged lock by hand",
                        Some(PARENT_TS),
                    ),
                ]),
            )
            .unwrap();
        store
    }

    /// Everything the reindex pass would project for this store, in the order
    /// it would project it.
    fn project(root: &Path) -> Vec<(TranscriptLocation, Vec<NormalizedTranscriptEvent>)> {
        let adapter = ConversationTranscriptAdapter::open(root, vec![enrollment()])
            .expect("an enrolled source opens an adapter");
        adapter
            .scan_locations(TranscriptScanTarget::Sessions)
            .unwrap()
            .into_iter()
            .map(|location| {
                let snapshot = adapter.load_snapshot(&location).unwrap();
                (location, snapshot.events)
            })
            .collect()
    }

    /// The projected events for the single fixture channel.
    fn project_channel(root: &Path) -> (TranscriptLocation, Vec<NormalizedTranscriptEvent>) {
        let mut projected = project(root);
        assert_eq!(projected.len(), 1, "one channel, one location");
        projected.remove(0)
    }

    struct Corpus {
        index: Index,
        fields: FieldHandles,
    }

    impl Corpus {
        fn new() -> Self {
            let (schema, fields) = build_schema();
            let index = Index::create_in_ram(schema);
            register_code_tokenizer(&index);
            Self { index, fields }
        }

        /// Index one location's events exactly as `index_adapter_location`
        /// would: delete the location's previous documents by its locator,
        /// then add the freshly projected ones.
        fn reindex(&self, location: &TranscriptLocation, events: &[NormalizedTranscriptEvent]) {
            let mut writer = self.index.writer(50_000_000).unwrap();
            let locator = location.locator();
            writer.delete_term(Term::from_field_text(self.fields.file_path, &locator));
            let account = location
                .account
                .clone()
                .unwrap_or_else(|| location.source.label().to_string());
            for event in events {
                let doc = normalized_to_doc(
                    event,
                    &account,
                    &locator,
                    location.is_subagent,
                    location.project.as_deref().unwrap_or(""),
                    None,
                    self.fields,
                )
                .expect("a conversation event projects to a document");
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }

        /// Index one ordinary harness transcript event, so the source filter
        /// has something other than Slack to include and exclude.
        fn add_harness_document(&self, text: &str) {
            let mut writer = self.index.writer(50_000_000).unwrap();
            let event = NormalizedTranscriptEvent::from_parsed_event(
                TranscriptSource::Harness(Provider::Glm),
                ParsedEvent {
                    role: MessageRole::User,
                    content: text.to_string(),
                    session_id: "sess-1".to_string(),
                    timestamp: Some("2026-08-13T00:00:00Z".to_string()),
                    git_branch: None,
                    is_subagent: false,
                    agent_slug: None,
                    cwd: Some("/repo".to_string()),
                    tool_call: None,
                },
                RawTranscriptRef::jsonl(
                    TranscriptSource::Harness(Provider::Glm),
                    TranscriptStorage::JsonlFile,
                    "/tmp/sess-1.events.jsonl",
                    0,
                    0,
                    64,
                ),
            );
            let doc = normalized_to_doc(
                &event,
                "glm",
                "/tmp/sess-1.events.jsonl",
                false,
                "/repo",
                None,
                self.fields,
            )
            .unwrap();
            writer.add_document(doc).unwrap();
            writer.commit().unwrap();
        }

        fn search_content(&self, query: &str) -> Vec<TantivyDocument> {
            let reader = self.index.reader().unwrap();
            reader.reload().unwrap();
            let searcher = reader.searcher();
            let parser = QueryParser::for_index(&self.index, vec![self.fields.content]);
            let parsed = parser.parse_query(query).unwrap();
            searcher
                .search(&parsed, &TopDocs::with_limit(50))
                .unwrap()
                .into_iter()
                .map(|(_, address)| searcher.doc::<TantivyDocument>(address).unwrap())
                .collect()
        }

        fn search_term(&self, field: tantivy::schema::Field, value: &str) -> Vec<TantivyDocument> {
            let reader = self.index.reader().unwrap();
            reader.reload().unwrap();
            let searcher = reader.searcher();
            let query = TermQuery::new(
                Term::from_field_text(field, value),
                IndexRecordOption::Basic,
            );
            searcher
                .search(&query, &TopDocs::with_limit(50))
                .unwrap()
                .into_iter()
                .map(|(_, address)| searcher.doc::<TantivyDocument>(address).unwrap())
                .collect()
        }

        fn total_documents(&self) -> usize {
            let reader = self.index.reader().unwrap();
            reader.reload().unwrap();
            reader.searcher().num_docs() as usize
        }
    }

    #[test]
    fn a_known_message_is_findable_by_text_and_returns_a_correct_permalink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        landed_store(&root);
        let (location, events) = project_channel(&root);

        let corpus = Corpus::new();
        corpus.reindex(&location, &events);

        let hits = corpus.search_content("wedged lock table");
        assert!(!hits.is_empty(), "a landed message is findable by its text");
        let hit = hits
            .iter()
            .find(|doc| first_text(doc, corpus.fields.conversation_message_ts) == PARENT_TS)
            .expect("the message that carries the phrase");

        assert_eq!(
            first_text(hit, corpus.fields.permalink),
            "https://acme.slack.com/archives/C0OPSFIX/p1712345678000200",
            "the archives permalink is derived from workspace, channel, and ts"
        );
        assert_eq!(first_text(hit, corpus.fields.source), "slack");
        assert_eq!(first_text(hit, corpus.fields.author_id), "U0HUMAN");
        assert_eq!(first_text(hit, corpus.fields.author_kind), "human");
        assert_eq!(
            first_text(hit, corpus.fields.conversation_workspace_id),
            WORKSPACE
        );
        assert_eq!(
            first_text(hit, corpus.fields.conversation_channel_id),
            CHANNEL
        );
        assert_eq!(
            first_text(hit, corpus.fields.entity_id),
            format!("slack:{WORKSPACE}:{CHANNEL}:{PARENT_TS}")
        );
        assert_eq!(
            first_text(hit, corpus.fields.timestamp),
            "2024-04-05T19:34:38Z"
        );
        assert_eq!(
            first_text(hit, corpus.fields.file_path),
            channel_locator(WORKSPACE, CHANNEL),
            "documents are keyed by the record key, never by the journal path"
        );
    }

    #[test]
    fn unenrolling_a_channel_purges_its_documents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = landed_store(&root);
        let (location, events) = project_channel(&root);
        let corpus = Corpus::new();
        corpus.reindex(&location, &events);
        assert_eq!(corpus.total_documents(), 3);

        // Coverage ends: the observing identity is no longer a member.
        store
            .record_roster(
                &scope(),
                WORKSPACE,
                &[observation(false)],
                false,
                "2026-08-13T00:10:00Z",
            )
            .unwrap();

        let adapter = ConversationTranscriptAdapter::open(&root, vec![enrollment()]).unwrap();
        let locations = adapter
            .scan_locations(TranscriptScanTarget::Sessions)
            .unwrap();
        assert!(
            locations.is_empty(),
            "an unenrolled channel leaves the scan set, which is what exposes \
             it to the purge phase"
        );

        // What the purge phase then does with a freshness row whose key is
        // absent from that scan set.
        let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
            corpus.index.writer(50_000_000).unwrap();
        writer.delete_term(Term::from_field_text(
            corpus.fields.file_path,
            &location.locator(),
        ));
        writer.commit().unwrap();
        assert_eq!(
            corpus.total_documents(),
            0,
            "a channel no enrolled source covers must stop being searchable"
        );
    }

    #[test]
    fn a_complete_roster_that_omits_a_channel_purges_it_and_re_enrollment_restores_it() {
        // gap-e84231d3: the producer never posted `is_member: false`, it just
        // stopped listing the channel. A complete sweep must produce the same
        // purge `unenrolling_a_channel_purges_its_documents` proves for an
        // explicit `is_member: false`, and the channel must come back the
        // moment a later complete sweep re-lists it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = landed_store(&root);
        let (location, events) = project_channel(&root);
        let corpus = Corpus::new();
        corpus.reindex(&location, &events);
        assert_eq!(corpus.total_documents(), 3);

        // A complete sweep that simply does not mention the channel anymore.
        store
            .record_roster(&scope(), WORKSPACE, &[], true, "2026-08-13T00:20:00Z")
            .unwrap();

        let adapter = ConversationTranscriptAdapter::open(&root, vec![enrollment()]).unwrap();
        assert!(
            adapter
                .scan_locations(TranscriptScanTarget::Sessions)
                .unwrap()
                .is_empty(),
            "an omitted channel in a complete sweep leaves the scan set exactly \
             like an explicit is_member: false"
        );

        let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
            corpus.index.writer(50_000_000).unwrap();
        writer.delete_term(Term::from_field_text(
            corpus.fields.file_path,
            &location.locator(),
        ));
        writer.commit().unwrap();
        // Released explicitly, BEFORE `corpus.reindex` below opens its own
        // writer on the same directory: tantivy allows exactly one live
        // `IndexWriter` per directory, and this writer would otherwise stay
        // alive until the end of the test function (this is a single-writer
        // test, unlike the sibling purge-only test where the manual writer
        // is the last thing the function opens).
        drop(writer);
        assert_eq!(corpus.total_documents(), 0);

        // The channel reappears in a later complete sweep: re-enrollment.
        store
            .record_roster(
                &scope(),
                WORKSPACE,
                &[observation(true)],
                true,
                "2026-08-13T00:30:00Z",
            )
            .unwrap();
        let restored = adapter
            .scan_locations(TranscriptScanTarget::Sessions)
            .unwrap();
        assert_eq!(
            restored.len(),
            1,
            "re-listing in a later complete sweep restores coverage"
        );
        let snapshot = adapter.load_snapshot(&restored[0]).unwrap();
        corpus.reindex(&restored[0], &snapshot.events);
        assert_eq!(
            corpus.total_documents(),
            3,
            "a re-enrolled channel is searchable again"
        );
    }

    #[test]
    fn retiring_the_operator_grant_unenrolls_every_channel_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        landed_store(&root);

        // The other half of the enrollment signal: config, not the store. A
        // landing directory that outlives its grant must not keep projecting.
        assert!(
            ConversationTranscriptAdapter::open(&root, Vec::new()).is_none(),
            "no enrolled scope means no adapter, so nothing is scanned"
        );
    }

    #[test]
    fn a_full_reprojection_is_deterministic_and_duplicate_free() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        landed_store(&root);

        let first = project_channel(&root);
        let second = project_channel(&root);
        assert_eq!(first.0.locator(), second.0.locator());
        let fingerprint = |events: &[NormalizedTranscriptEvent]| {
            events
                .iter()
                .map(|event| {
                    (
                        event.session_id.clone(),
                        event.content.clone(),
                        event.timestamp.clone(),
                        event.raw.entity_id.clone(),
                        event.conversation.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            fingerprint(&first.1),
            fingerprint(&second.1),
            "the same journal projects the same events"
        );

        // And a second indexing pass over the same records replaces rather
        // than accumulates: the delete-by-locator is what makes a full
        // reindex duplicate-free.
        let corpus = Corpus::new();
        corpus.reindex(&first.0, &first.1);
        corpus.reindex(&second.0, &second.1);
        assert_eq!(
            corpus.total_documents(),
            3,
            "a reindex from landed records is duplicate-free"
        );
    }

    #[test]
    fn a_revision_updates_in_place_and_preserves_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = landed_store(&root);
        let corpus = Corpus::new();
        let (location, events) = project_channel(&root);
        corpus.reindex(&location, &events);

        store
            .land_revisions(
                &scope(),
                &revisions(vec![ConversationRevisionV1::Edit(
                    ConversationRevisionRecordV1 {
                        channel_id: CHANNEL.to_string(),
                        message_ts: PARENT_TS.to_string(),
                        revision: 1,
                        text: Some("the deploy pipeline wedged on the quota table".to_string()),
                        edited_ts: Some("1712345999.000000".to_string()),
                        observed_at: "2026-08-13T00:10:00Z".to_string(),
                    },
                )]),
            )
            .unwrap();

        let (location, events) = project_channel(&root);
        corpus.reindex(&location, &events);

        assert_eq!(
            corpus.total_documents(),
            3,
            "an edit supersedes in place rather than adding a second document"
        );
        let hits = corpus.search_content("quota");
        assert_eq!(hits.len(), 1, "the edited text is what is searchable now");
        assert_eq!(
            first_text(&hits[0], corpus.fields.entity_id),
            format!("slack:{WORKSPACE}:{CHANNEL}:{PARENT_TS}"),
            "identity excludes the revision, which is what makes an edit an update"
        );
        assert!(
            corpus.search_content("\"lock table\"").is_empty(),
            "the superseded text is gone, not shadowed by a newer copy"
        );
    }

    #[test]
    fn a_tombstone_removes_the_message_from_search() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = landed_store(&root);
        let corpus = Corpus::new();
        let (location, events) = project_channel(&root);
        corpus.reindex(&location, &events);
        assert_eq!(corpus.search_content("wedged").len(), 2);

        store
            .land_revisions(
                &scope(),
                &revisions(vec![ConversationRevisionV1::Tombstone(
                    ConversationTombstoneRecordV1 {
                        channel_id: CHANNEL.to_string(),
                        message_ts: PARENT_TS.to_string(),
                        revision: 1,
                        observed_at: "2026-08-13T00:10:00Z".to_string(),
                    },
                )]),
            )
            .unwrap();

        let (location, events) = project_channel(&root);
        assert_eq!(
            events.len(),
            2,
            "a tombstoned record projects to no document at all"
        );
        corpus.reindex(&location, &events);
        assert_eq!(corpus.total_documents(), 2);
        let survivors = corpus.search_content("wedged");
        assert_eq!(survivors.len(), 1);
        assert_eq!(
            first_text(&survivors[0], corpus.fields.conversation_message_ts),
            REPLY_TS,
            "the deleted message is gone and its thread reply is not"
        );
    }

    #[test]
    fn a_thread_reply_is_searchable_under_its_parents_context() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        landed_store(&root);
        let corpus = Corpus::new();
        let (location, events) = project_channel(&root);
        corpus.reindex(&location, &events);

        let hits = corpus.search_content("cleared by hand");
        let reply = hits
            .iter()
            .find(|doc| first_text(doc, corpus.fields.conversation_message_ts) == REPLY_TS)
            .expect("the reply is findable by its own text");
        assert_eq!(
            first_text(reply, corpus.fields.conversation_thread_ts),
            PARENT_TS
        );
        // The reply landed a day after its parent, and still reads under the
        // parent's bucket rather than opening a second one.
        assert_eq!(
            first_text(reply, corpus.fields.session_id),
            format!("{CHANNEL}/2024-04-05"),
            "a reply buckets with its thread parent, not with its own day"
        );
        assert_eq!(
            first_text(reply, corpus.fields.permalink),
            format!(
                "https://acme.slack.com/archives/{CHANNEL}/p1712432100000300\
                 ?thread_ts={PARENT_TS}&cid={CHANNEL}"
            )
        );
    }

    #[test]
    fn the_author_filter_selects_identity_while_role_only_says_human_or_app() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        landed_store(&root);
        let corpus = Corpus::new();
        let (location, events) = project_channel(&root);
        corpus.reindex(&location, &events);

        let by_author = corpus.search_term(corpus.fields.author_id, "B0BUILDBOT");
        assert_eq!(by_author.len(), 1);
        assert_eq!(
            first_text(&by_author[0], corpus.fields.conversation_message_ts),
            APP_TS
        );

        // The role lane carries only the human-versus-app distinction, which
        // is why identity needed a field of its own.
        assert_eq!(
            corpus.search_term(corpus.fields.role, "assistant").len(),
            1,
            "app messages are the assistant lane"
        );
        assert_eq!(
            corpus.search_term(corpus.fields.role, "user").len(),
            2,
            "human messages are the user lane"
        );
    }

    #[test]
    fn slack_is_searchable_by_default_and_one_filter_includes_or_excludes_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        landed_store(&root);
        let corpus = Corpus::new();
        let (location, events) = project_channel(&root);
        corpus.reindex(&location, &events);
        corpus.add_harness_document("the deploy pipeline wedged during the session too");

        // Default posture (design section 7): no source filter, Slack is in.
        assert_eq!(
            corpus.search_content("wedged").len(),
            3,
            "Slack conversations are searchable by default"
        );

        // The same terms the source filter builds: include, then exclude.
        // A bare lane term carries no text constraint, so it selects all
        // THREE landed messages, not just the two that say "wedged".
        assert_eq!(corpus.search_term(corpus.fields.source, "slack").len(), 3);

        let reader = corpus.index.reader().unwrap();
        reader.reload().unwrap();
        let searcher = reader.searcher();
        let parser = QueryParser::for_index(&corpus.index, vec![corpus.fields.content]);
        let text = parser.parse_query("wedged").unwrap();
        let without_slack = BooleanQuery::new(vec![
            (Occur::Must, text),
            (
                Occur::MustNot,
                Box::new(TermQuery::new(
                    Term::from_field_text(corpus.fields.source, "slack"),
                    IndexRecordOption::Basic,
                )) as Box<dyn tantivy::query::Query>,
            ),
        ]);
        let hits = searcher
            .search(&without_slack, &TopDocs::with_limit(10))
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "excluding one lane leaves every other lane searchable"
        );
    }
}
