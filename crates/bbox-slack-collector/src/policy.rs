//! Channel enrollment policy (design section 6).
//!
//! Four rules, in this order, and the order is the policy:
//!
//! 1. **A class the operator did not enable is never enrolled.** Private
//!    channels are a separate flag, default off. Direct and group-direct
//!    messages have no flag at all: the deployed posture has none and
//!    [`bbox_conversation_source::ChannelClassV1`] is closed to channel
//!    classes, so a DM is refused here rather than mislabeled as a channel.
//! 2. **Membership is the visibility bound.** Under the ruled one-app posture
//!    the bot indexes exactly what its own membership allows (design 3.1), and
//!    the collector never widens that: it does not call `conversations.join`,
//!    it does not enroll a channel because it was mentioned, and it cannot,
//!    because neither method is representable
//!    ([`crate::slack::SlackReadMethod`]).
//! 3. **Exclude beats include.** A denylist overrides an allowlist, same shape
//!    as remote project onboarding, for the same reason: the rule an operator
//!    writes to keep something OUT has to be the one that wins.
//! 4. **Under the DEFAULT `explicit` mode, an empty include list enrolls
//!    nothing.** This is the deliberate inversion of the file lane, where an
//!    empty include means "everything the other rules allow": the
//!    empty-config default is a satellite that observes NOTHING and says so,
//!    not one that indexes a workspace because a config field was left blank.
//!    The operator may instead opt in BY NAME to `enrollment = "membership"`
//!    (ruled 2026-08-14 for the first deployment, whose bot has deliberately
//!    narrow membership): every member channel of an enabled class enrolls,
//!    an invite is enrollment, a non-empty include still narrows, and
//!    excludes still win. Membership mode never widens visibility beyond
//!    rule 2; it only stops re-stating rule 2 in globs.
//!
//! Every refusal is counted under a named reason, because a policy quietly
//! dropping half a workspace is indistinguishable from a broken connector.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use bbox_conversation_source::ChannelClassV1;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::slack::RawChannel;

/// Stable skip labels, shown to operators on the satellite's status output.
pub const REASON_NOT_INCLUDED: &str = "not_included";
pub const REASON_EXCLUDED: &str = "excluded";
pub const REASON_NOT_A_MEMBER: &str = "not_a_member";
pub const REASON_CLASS_DISABLED: &str = "class_disabled";
pub const REASON_DIRECT_MESSAGE: &str = "direct_message";
pub const REASON_ARCHIVED: &str = "archived";
pub const REASON_UNNAMED: &str = "unnamed";

/// How a channel becomes enrolled: named in config, never inferred.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentMode {
    /// Only channels matching a non-empty `include` glob enroll.
    #[default]
    Explicit,
    /// Every member channel of an enabled class enrolls (an invite is
    /// enrollment); a non-empty `include` still narrows and `exclude` still
    /// wins. The operator opt-in for bots whose membership IS the intended
    /// index boundary.
    Membership,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ChannelPolicy {
    /// Enrollment mode; defaults to `explicit`.
    #[serde(default)]
    pub enrollment: EnrollmentMode,
    /// Channel-name globs to enroll. EMPTY ENROLLS NOTHING; see the module
    /// note. Patterns are matched against the observed name without a leading
    /// `#`.
    #[serde(default)]
    pub include: Vec<String>,
    /// Channel-name globs to refuse, overriding `include`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Enable the private-channel class. Still requires an `include` match per
    /// channel: enabling a class is not enrolling its members.
    #[serde(default)]
    pub private_channels: bool,
    /// Observe archived channels. Off by default: an archived channel is
    /// history nobody is adding to, so it belongs to backfill rather than to
    /// steady state.
    #[serde(default)]
    pub include_archived: bool,
}

impl ChannelPolicy {
    pub fn validate(&self) -> Result<()> {
        CompiledChannelPolicy::compile(self)?;
        if self.enrollment == EnrollmentMode::Explicit && self.include.is_empty() {
            // Not an error: a satellite that observes nothing is a legitimate
            // (if pointless) deployment, and refusing it would break the
            // onboard-then-configure order. It is a warning because the far
            // more likely reading is an operator who expected a default.
            tracing::warn!(
                "channels.include is empty; this satellite will enroll no channels at all"
            );
        }
        Ok(())
    }
}

/// The enrolled/refused verdict for one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelDecision {
    Enroll { class: ChannelClassV1 },
    Skip { reason: &'static str },
}

#[derive(Debug)]
pub struct CompiledChannelPolicy {
    enrollment: EnrollmentMode,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    private_channels: bool,
    include_archived: bool,
}

impl CompiledChannelPolicy {
    pub fn compile(policy: &ChannelPolicy) -> Result<Self> {
        Ok(Self {
            enrollment: policy.enrollment,
            include: compile_set(&policy.include).context("compiling channel include globs")?,
            exclude: compile_set(&policy.exclude).context("compiling channel exclude globs")?,
            private_channels: policy.private_channels,
            include_archived: policy.include_archived,
        })
    }

    /// Whether the roster read should ask for private channels at all.
    ///
    /// Policy reaches the REQUEST, not just the filter: a satellite whose
    /// operator did not enable the private class does not enumerate private
    /// channels and then discard them, it never asks. What is not requested
    /// cannot be logged, cached, or leaked by a later refactor.
    pub fn include_private(&self) -> bool {
        self.private_channels
    }

    pub fn include_archived(&self) -> bool {
        self.include_archived
    }

    pub fn decide(&self, channel: &RawChannel) -> ChannelDecision {
        if channel.is_im || channel.is_mpim {
            return ChannelDecision::Skip {
                reason: REASON_DIRECT_MESSAGE,
            };
        }
        if channel.is_private && !self.private_channels {
            return ChannelDecision::Skip {
                reason: REASON_CLASS_DISABLED,
            };
        }
        if channel.is_archived && !self.include_archived {
            return ChannelDecision::Skip {
                reason: REASON_ARCHIVED,
            };
        }
        if !channel.is_member {
            return ChannelDecision::Skip {
                reason: REASON_NOT_A_MEMBER,
            };
        }
        let Some(name) = channel.name.as_deref() else {
            // Every allow rule this policy has is written against a NAME. A
            // channel without one cannot be matched, and enrolling it would
            // mean enrolling something no operator rule describes.
            return ChannelDecision::Skip {
                reason: REASON_UNNAMED,
            };
        };
        let name = name.trim_start_matches('#');
        if let Some(exclude) = &self.exclude
            && exclude.is_match(name)
        {
            return ChannelDecision::Skip {
                reason: REASON_EXCLUDED,
            };
        }
        let class = if channel.is_private {
            ChannelClassV1::Private
        } else {
            ChannelClassV1::Public
        };
        match (&self.enrollment, &self.include) {
            // Membership mode with no narrowing globs: membership (already
            // proven above) is the whole rule.
            (EnrollmentMode::Membership, None) => ChannelDecision::Enroll { class },
            // A non-empty include narrows either mode.
            (_, Some(include)) if include.is_match(name) => ChannelDecision::Enroll { class },
            (EnrollmentMode::Membership, Some(_)) | (EnrollmentMode::Explicit, _) => {
                ChannelDecision::Skip {
                    reason: REASON_NOT_INCLUDED,
                }
            }
        }
    }
}

fn compile_set(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.trim().is_empty() {
            bail!("an empty channel glob matches nothing and is almost certainly a typo");
        }
        builder.add(Glob::new(pattern).with_context(|| format!("compiling glob {pattern}"))?);
    }
    Ok(Some(builder.build()?))
}

/// Per-reason refusal counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkipCounters {
    counts: BTreeMap<String, u64>,
}

impl SkipCounters {
    pub fn record(&mut self, reason: &str) {
        *self.counts.entry(reason.to_string()).or_default() += 1;
    }

    pub fn get(&self, reason: &str) -> u64 {
        self.counts.get(reason).copied().unwrap_or(0)
    }

    pub fn into_map(self) -> BTreeMap<String, u64> {
        self.counts
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(name: &str) -> RawChannel {
        RawChannel {
            id: format!("C{}", name.to_uppercase()),
            name: Some(name.to_string()),
            is_private: false,
            is_archived: false,
            is_member: true,
            is_im: false,
            is_mpim: false,
            created: None,
        }
    }

    fn policy(policy: ChannelPolicy) -> CompiledChannelPolicy {
        CompiledChannelPolicy::compile(&policy).unwrap()
    }

    #[test]
    fn an_empty_include_list_enrolls_nothing() {
        let compiled = policy(ChannelPolicy::default());
        assert_eq!(
            compiled.decide(&channel("engineering")),
            ChannelDecision::Skip {
                reason: REASON_NOT_INCLUDED
            }
        );
    }

    #[test]
    fn exclude_beats_include() {
        let compiled = policy(ChannelPolicy {
            include: vec!["eng-*".into()],
            exclude: vec!["eng-secrets".into()],
            ..ChannelPolicy::default()
        });
        assert!(matches!(
            compiled.decide(&channel("eng-platform")),
            ChannelDecision::Enroll { .. }
        ));
        assert_eq!(
            compiled.decide(&channel("eng-secrets")),
            ChannelDecision::Skip {
                reason: REASON_EXCLUDED
            }
        );
    }

    #[test]
    fn a_channel_the_bot_has_not_joined_is_never_enrolled() {
        let compiled = policy(ChannelPolicy {
            include: vec!["*".into()],
            ..ChannelPolicy::default()
        });
        let mut outsider = channel("public-but-unjoined");
        outsider.is_member = false;
        assert_eq!(
            compiled.decide(&outsider),
            ChannelDecision::Skip {
                reason: REASON_NOT_A_MEMBER
            }
        );
    }

    #[test]
    fn the_private_class_needs_its_flag_and_an_include_match() {
        let mut private = channel("leadership");
        private.is_private = true;

        let without_flag = policy(ChannelPolicy {
            include: vec!["*".into()],
            ..ChannelPolicy::default()
        });
        assert_eq!(
            without_flag.decide(&private),
            ChannelDecision::Skip {
                reason: REASON_CLASS_DISABLED
            }
        );
        assert!(!without_flag.include_private());

        let with_flag = policy(ChannelPolicy {
            include: vec!["leadership".into()],
            private_channels: true,
            ..ChannelPolicy::default()
        });
        assert_eq!(
            with_flag.decide(&private),
            ChannelDecision::Enroll {
                class: ChannelClassV1::Private
            }
        );
        assert!(with_flag.include_private());
    }

    #[test]
    fn a_direct_message_is_refused_before_any_other_rule() {
        // Belt and braces against the class the wire cannot even express: a DM
        // is refused even with every flag on and a matching include.
        let compiled = policy(ChannelPolicy {
            include: vec!["*".into()],
            private_channels: true,
            include_archived: true,
            ..ChannelPolicy::default()
        });
        let mut dm = channel("dm");
        dm.is_im = true;
        dm.is_private = true;
        assert_eq!(
            compiled.decide(&dm),
            ChannelDecision::Skip {
                reason: REASON_DIRECT_MESSAGE
            }
        );
    }

    #[test]
    fn a_leading_hash_in_an_observed_name_does_not_defeat_a_glob() {
        let compiled = policy(ChannelPolicy {
            include: vec!["general".into()],
            ..ChannelPolicy::default()
        });
        let mut hashed = channel("general");
        hashed.name = Some("#general".into());
        assert!(matches!(
            compiled.decide(&hashed),
            ChannelDecision::Enroll { .. }
        ));
    }

    #[test]
    fn an_archived_channel_is_out_of_steady_state_by_default() {
        let compiled = policy(ChannelPolicy {
            include: vec!["*".into()],
            ..ChannelPolicy::default()
        });
        let mut archived = channel("old-project");
        archived.is_archived = true;
        assert_eq!(
            compiled.decide(&archived),
            ChannelDecision::Skip {
                reason: REASON_ARCHIVED
            }
        );
    }

    #[test]
    fn membership_mode_enrolls_a_member_channel_with_no_globs() {
        let policy = ChannelPolicy {
            enrollment: EnrollmentMode::Membership,
            ..ChannelPolicy::default()
        };
        let compiled = CompiledChannelPolicy::compile(&policy).unwrap();
        let channel = channel("anything-at-all");
        assert_eq!(
            compiled.decide(&channel),
            ChannelDecision::Enroll {
                class: ChannelClassV1::Public
            },
            "an invite is enrollment under membership mode"
        );
    }

    #[test]
    fn membership_mode_still_honors_exclude_and_membership() {
        let policy = ChannelPolicy {
            enrollment: EnrollmentMode::Membership,
            exclude: vec!["secret-*".into()],
            ..ChannelPolicy::default()
        };
        let compiled = CompiledChannelPolicy::compile(&policy).unwrap();
        let excluded = channel("secret-plans");
        assert_eq!(
            compiled.decide(&excluded),
            ChannelDecision::Skip {
                reason: REASON_EXCLUDED
            },
            "the rule an operator writes to keep something out still wins"
        );
        let mut outsider = channel("general");
        outsider.is_member = false;
        assert_eq!(
            compiled.decide(&outsider),
            ChannelDecision::Skip {
                reason: REASON_NOT_A_MEMBER
            },
            "membership mode never widens past the membership bound"
        );
    }

    #[test]
    fn membership_mode_with_globs_narrows_rather_than_widens() {
        let policy = ChannelPolicy {
            enrollment: EnrollmentMode::Membership,
            include: vec!["pg-*".into()],
            ..ChannelPolicy::default()
        };
        let compiled = CompiledChannelPolicy::compile(&policy).unwrap();
        assert_eq!(
            compiled.decide(&channel("pg-triage")),
            ChannelDecision::Enroll {
                class: ChannelClassV1::Public
            }
        );
        assert_eq!(
            compiled.decide(&channel("random")),
            ChannelDecision::Skip {
                reason: REASON_NOT_INCLUDED
            },
            "a non-empty include narrows membership mode"
        );
    }

    #[test]
    fn explicit_mode_default_is_unchanged_by_the_new_field() {
        let policy = ChannelPolicy::default();
        assert_eq!(policy.enrollment, EnrollmentMode::Explicit);
        let compiled = CompiledChannelPolicy::compile(&policy).unwrap();
        assert_eq!(
            compiled.decide(&channel("general")),
            ChannelDecision::Skip {
                reason: REASON_NOT_INCLUDED
            },
            "empty include still enrolls nothing under the default mode"
        );
    }
}
