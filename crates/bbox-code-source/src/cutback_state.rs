//! Typed cutback state for catalog-mode activation records (Phase 4-A).
//!
//! These types are the single reducer input alongside desired assignment and
//! effective activation (design durable-project-catalog-phase4-impl.md
//! section 4.1). They live in this dependency-clean leaf so the store crate
//! can embed them in [`ActivationRecordV2`] without crossing the server
//! boundary.

use serde::{Deserialize, Serialize};

/// The structural cause of a cutback. Structural states persist without
/// polling and are re-evaluated by an attachment event or config reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CutbackReason {
    NoLocalAttachment,
    AmbiguousAttachment,
    ScopeMismatch,
}

/// The closed set of recoverable failure classes for transient, manual, and
/// terminal cutback states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CutbackErrorClass {
    WriterContention,
    IoPressure,
    Deadline,
    IndexCommit,
    ValidationFailure,
    SecurityFailure,
}

/// The complete cutback state enum persisted on the activation record.
///
/// There is no unclassified placeholder variant (section 4.1). The
/// [`CutbackStateV2::validate`] method enforces the per-variant invariants;
/// the layered coherence clause between `cutback` and the derived
/// `cutback_pending` mirror is checked by the activation record's own
/// validate (section 4.10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields, tag = "kind")]
pub enum CutbackStateV2 {
    /// Persists without polling. An attachment event or config reload
    /// re-evaluates the project through the reducer.
    Structural { reason: CutbackReason },
    /// Persists attempt count, closed error class, and deadline. After the
    /// configured cap, state becomes [`Self::ManualRetryRequired`].
    Transient {
        attempt: u32,
        error_class: CutbackErrorClass,
        deadline_unix_secs: u64,
    },
    /// Released only by a config reload. The reconciler never auto-retries.
    ManualRetryRequired {
        error_class: CutbackErrorClass,
        attempt: u32,
    },
    /// A validation or security failure that is unrecoverable. The collected
    /// generation stays active and authoritative; the activation record is the
    /// GC root and no automatic retry ever fires.
    Terminal { error_class: CutbackErrorClass },
}

impl CutbackStateV2 {
    /// True when this state pins the collected generation as a GC root (every
    /// variant does; provided for clarity at call sites).
    pub fn pins_generation(&self) -> bool {
        true
    }

    /// Validate the per-variant invariants (section 5.2 item 3).
    ///
    /// Refuses [`Self::Transient`] with `attempt == 0` or a zero deadline,
    /// and refuses [`Self::ManualRetryRequired`] with `attempt == 0`. The
    /// [`Self::Structural`] variant carries only a reason and cannot carry
    /// retry fields by construction; a malformed tagged shape fails to decode
    /// before reaching this method.
    pub fn validate(&self) -> Result<(), CutbackStateError> {
        match self {
            Self::Structural { .. } => Ok(()),
            Self::Transient {
                attempt,
                deadline_unix_secs,
                ..
            } => {
                if *attempt == 0 {
                    return Err(CutbackStateError::TransientZeroAttempt);
                }
                if *deadline_unix_secs == 0 {
                    return Err(CutbackStateError::TransientZeroDeadline);
                }
                Ok(())
            }
            Self::ManualRetryRequired { attempt, .. } => {
                if *attempt == 0 {
                    return Err(CutbackStateError::ManualRetryZeroAttempt);
                }
                Ok(())
            }
            Self::Terminal { .. } => Ok(()),
        }
    }
}

/// Errors returned by [`CutbackStateV2::validate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CutbackStateError {
    #[error("transient cutback state requires attempt >= 1")]
    TransientZeroAttempt,
    #[error("transient cutback state requires a non-zero deadline")]
    TransientZeroDeadline,
    #[error("manual retry cutback state requires attempt >= 1")]
    ManualRetryZeroAttempt,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_round_trip() {
        for reason in [
            CutbackReason::NoLocalAttachment,
            CutbackReason::AmbiguousAttachment,
            CutbackReason::ScopeMismatch,
        ] {
            let state = CutbackStateV2::Structural { reason };
            let json = serde_json::to_string(&state).unwrap();
            let back: CutbackStateV2 = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
            state.validate().unwrap();
        }
    }

    #[test]
    fn transient_round_trip_and_validate() {
        let state = CutbackStateV2::Transient {
            attempt: 2,
            error_class: CutbackErrorClass::WriterContention,
            deadline_unix_secs: 1_700_000_000,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: CutbackStateV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
        state.validate().unwrap();
    }

    #[test]
    fn transient_zero_attempt_refused() {
        let state = CutbackStateV2::Transient {
            attempt: 0,
            error_class: CutbackErrorClass::IoPressure,
            deadline_unix_secs: 1_700_000_000,
        };
        assert_eq!(
            state.validate(),
            Err(CutbackStateError::TransientZeroAttempt)
        );
    }

    #[test]
    fn transient_zero_deadline_refused() {
        let state = CutbackStateV2::Transient {
            attempt: 1,
            error_class: CutbackErrorClass::Deadline,
            deadline_unix_secs: 0,
        };
        assert_eq!(
            state.validate(),
            Err(CutbackStateError::TransientZeroDeadline)
        );
    }

    #[test]
    fn manual_retry_round_trip_and_validate() {
        let state = CutbackStateV2::ManualRetryRequired {
            error_class: CutbackErrorClass::IndexCommit,
            attempt: 9,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: CutbackStateV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
        state.validate().unwrap();
    }

    #[test]
    fn manual_retry_zero_attempt_refused() {
        let state = CutbackStateV2::ManualRetryRequired {
            error_class: CutbackErrorClass::ValidationFailure,
            attempt: 0,
        };
        assert_eq!(
            state.validate(),
            Err(CutbackStateError::ManualRetryZeroAttempt)
        );
    }

    #[test]
    fn terminal_round_trip_and_validate() {
        let state = CutbackStateV2::Terminal {
            error_class: CutbackErrorClass::SecurityFailure,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: CutbackStateV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
        state.validate().unwrap();
    }

    #[test]
    fn deny_unknown_fields_on_tagged_shape() {
        let json = r#"{"kind":"structural","reason":"no_local_attachment","extra":true}"#;
        assert!(serde_json::from_str::<CutbackStateV2>(json).is_err());
    }

    #[test]
    fn unknown_variant_kind_refused() {
        let json = r#"{"kind":"bogus"}"#;
        assert!(serde_json::from_str::<CutbackStateV2>(json).is_err());
    }
}
