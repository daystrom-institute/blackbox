//! The Slack read surface: a closed method set, an allowlist client, the
//! response shapes it parses, and the pacer that keeps it a polite minority
//! consumer of a credential it shares with an interactive process.
//!
//! Read [`method`] first. It is the layer that makes a write unrepresentable
//! rather than merely absent, and every other module here depends on that
//! property holding.

pub mod client;
pub mod method;
pub mod model;
pub mod throttle;

pub use client::{
    ChannelListRequest, ChannelRoster, ClientStats, DEFAULT_API_BASE_URL, DEFAULT_PAGE_LIMIT,
    HistoryRequest, MAX_RESPONSE_BYTES, RepliesRequest, SlackClient, SlackRead, Sweep,
};
pub use method::SlackReadMethod;
pub use model::{
    AuthTestResponse, ConversationsHistoryResponse, ConversationsListResponse, RawChannel,
    RawEdited, RawFile, RawMessage, RawReaction, ResponseMetadata, SlackEnvelope, SlackIdentity,
};
pub use throttle::{Pacer, RatePolicy};
