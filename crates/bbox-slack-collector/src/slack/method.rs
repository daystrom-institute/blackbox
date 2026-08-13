//! The closed read-method set: layer two of the write-safety contract.
//!
//! Under the ruled one-app posture (design 3.1) the collector reads with the
//! interactive bot's own token, which carries write scopes. There is no
//! read-only credential to hold, so "this process cannot write to Slack" has to
//! be a property of the code rather than of the grant.
//!
//! This enum is that property. It is CLOSED, it is the only thing
//! [`super::SlackClient`] will compose a request path from, and the client
//! exposes no string-taking entry point. Adding a write method is not a
//! configuration mistake anyone can make at runtime or an operator can make in
//! a config file: it is a deliberate edit to this file, in a pull request, next
//! to this comment.
//!
//! Two consequences worth stating because they are easy to erode later:
//!
//! - **No SDK.** A Slack SDK would hand the crate `chat.postMessage` and the
//!   whole write surface as ordinary functions, and layer two would degrade
//!   from a construction to a convention that a future call site can violate
//!   silently.
//! - **No `from_api_name` in the request path.** [`SlackReadMethod::parse`]
//!   exists for diagnostics and for the acceptance test, and it REFUSES every
//!   name outside this set. It is not how the client picks a method; the client
//!   takes the enum by value.

/// A Slack Web API method this collector is permitted to call.
///
/// Every variant is a read. There is no variant, and no representable value of
/// this type, that mutates a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlackReadMethod {
    /// Identity probe. Names the workspace and the bot, and its response
    /// headers carry the granted scope list the status surface records.
    AuthTest,
    /// The channel roster, bounded to the classes policy allows.
    ConversationsList,
    /// One channel's forward history from a `ts` watermark.
    ConversationsHistory,
    /// One thread parent's replies. Design 5.3: threaded replies do not appear
    /// in a history sweep unless they were also broadcast, so this is not an
    /// optional extra, it is most of the conversation.
    ConversationsReplies,
    /// The user roster. Enumerated because the allowlist is the whole contract
    /// and a later enrichment pass will want it; v1 does not call it (see
    /// [`crate::normalize`] on why author kind comes from message structure).
    UsersList,
    /// One user. Same standing as [`Self::UsersList`].
    UsersInfo,
}

impl SlackReadMethod {
    /// Every method this collector may call, in a stable order.
    ///
    /// The acceptance test asserts this list against the design's enumerated
    /// read set, so a variant added without updating the design fails there.
    pub const ALL: &'static [Self] = &[
        Self::AuthTest,
        Self::ConversationsList,
        Self::ConversationsHistory,
        Self::ConversationsReplies,
        Self::UsersList,
        Self::UsersInfo,
    ];

    /// The API method name, which is also the last path segment.
    ///
    /// Returning `&'static str` rather than a `String` is deliberate: there is
    /// no code path that can produce a method name at runtime, so there is no
    /// code path that can produce an unlisted one.
    pub const fn api_name(self) -> &'static str {
        match self {
            Self::AuthTest => "auth.test",
            Self::ConversationsList => "conversations.list",
            Self::ConversationsHistory => "conversations.history",
            Self::ConversationsReplies => "conversations.replies",
            Self::UsersList => "users.list",
            Self::UsersInfo => "users.info",
        }
    }

    /// Whether this method's rate band is one of the paging-heavy ones.
    ///
    /// `conversations.history` and `conversations.replies` sit in Slack's
    /// lower-tier per-method bands and the thread sweep multiplies call count
    /// by thread cardinality rather than by message count (design 5.5), so the
    /// pacer treats them as the scarce resource and lets the cheap identity and
    /// roster probes through on the same conservative budget rather than a
    /// separate one.
    pub const fn is_paging_read(self) -> bool {
        matches!(
            self,
            Self::ConversationsHistory | Self::ConversationsReplies
        )
    }

    /// Resolve a method NAME, refusing anything outside the allowlist.
    ///
    /// Diagnostics and acceptance only. The client never calls this: it takes
    /// [`SlackReadMethod`] by value, so a caller holding only a string cannot
    /// reach the network at all. The test that matters is
    /// `parse_refuses_a_write_method`, which is the construction refusal in the
    /// gate clause list.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|method| method.api_name() == name)
    }
}

impl std::fmt::Display for SlackReadMethod {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.api_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_refuses_a_write_method() {
        // The construction refusal. Every one of these is a real Slack method
        // and none of them is representable as a `SlackReadMethod`, so no call
        // site in this crate -- present or future -- can hand one to the
        // client without editing the enum.
        for write_method in [
            "chat.postMessage",
            "chat.update",
            "chat.delete",
            "chat.postEphemeral",
            "reactions.add",
            "reactions.remove",
            "conversations.join",
            "conversations.invite",
            "conversations.create",
            "conversations.archive",
            "files.upload",
            "views.open",
            "views.publish",
        ] {
            assert!(
                SlackReadMethod::parse(write_method).is_none(),
                "{write_method} must not be representable"
            );
        }
    }

    #[test]
    fn the_allowlist_is_exactly_the_designs_read_set() {
        let names: Vec<&str> = SlackReadMethod::ALL
            .iter()
            .map(|method| method.api_name())
            .collect();
        assert_eq!(
            names,
            vec![
                "auth.test",
                "conversations.list",
                "conversations.history",
                "conversations.replies",
                "users.list",
                "users.info",
            ]
        );
    }

    #[test]
    fn every_allowlisted_name_round_trips() {
        for method in SlackReadMethod::ALL {
            assert_eq!(SlackReadMethod::parse(method.api_name()), Some(*method));
        }
    }
}
