//! Retained accepted observation batches, content addressed.
//!
//! Retention policy (operator decision 98d9f430f62ad8ca): accepted batches are
//! retained content addressed, defaulting to the current plus the prior
//! generation and everything younger than a retention window, per source
//! class and widenable by deployment policy. Reprojection beyond the retained
//! horizon degrades honestly to re-observe rather than silently projecting a
//! partial history.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::delta::projection_failure;
use crate::{ObservationBatch, is_sha256_hex};

pub const RETAINED_OBSERVATION_VERSION: u32 = 1;
pub const DEFAULT_RETAINED_GENERATIONS: u64 = 2;
pub const DEFAULT_RETENTION_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;

/// Per source class retention. `retained_generations` counts back from the
/// accepted generation and is never less than one, so the batch behind the
/// accepted generation is always retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationRetentionPolicy {
    pub retained_generations: u64,
    pub retention_window_secs: u64,
}

impl Default for ObservationRetentionPolicy {
    fn default() -> Self {
        Self {
            retained_generations: DEFAULT_RETAINED_GENERATIONS,
            retention_window_secs: DEFAULT_RETENTION_WINDOW_SECS,
        }
    }
}

impl ObservationRetentionPolicy {
    /// The oldest generation still protected by the generation window.
    pub fn horizon(&self, accepted_generation: u64) -> u64 {
        let span = self.retained_generations.max(1);
        accepted_generation
            .saturating_sub(span.saturating_sub(1))
            .max(1)
    }

    pub fn retains(&self, batch: &RetainedObservationBatch, accepted: u64, now_unix: u64) -> bool {
        batch.generation >= self.horizon(accepted)
            || batch.accepted_at_unix >= now_unix.saturating_sub(self.retention_window_secs)
    }
}

/// One retained batch on disk. The file is named by [`observation_digest`] of
/// the BATCH alone, so an exact replay addresses the same bytes at the same
/// path; the wrapper's `generation` and `accepted_at_unix` describe the first
/// acceptance that retained it and are never rewritten.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetainedObservationBatch {
    pub version: u32,
    pub scope_id: String,
    pub graph_id: String,
    pub generation: u64,
    pub accepted_at_unix: u64,
    pub digest: String,
    pub batch: ObservationBatch,
}

/// A retained batch's identity without its payload. Status and planning
/// surfaces use this so no observation payload leaks into an operator view.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RetainedObservationRef {
    pub digest: String,
    pub batch_id: String,
    pub generation: u64,
    pub accepted_at_unix: u64,
}

/// The content address of an observation batch.
pub fn observation_digest(batch: &ObservationBatch) -> Result<String> {
    let bytes = serde_json::to_vec(batch)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// An ordered replay of retained batches, and whether it is complete back to
/// the requested baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayPlan {
    /// Retained batches with generation greater than the baseline, oldest
    /// first.
    pub batches: Vec<RetainedObservationRef>,
    /// The oldest generation still retained, if anything is retained.
    pub earliest_retained_generation: Option<u64>,
    /// False when a generation in the requested range is no longer retained.
    /// A false plan must degrade to re-observation rather than project a
    /// partial history.
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ObservationSweepStats {
    pub examined: usize,
    pub reclaimed: usize,
}

pub(crate) fn read_retained(path: &Path) -> Result<RetainedObservationBatch> {
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let retained: RetainedObservationBatch =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if retained.version != RETAINED_OBSERVATION_VERSION {
        return projection_failure(
            "observations.unsupported_version",
            format!(
                "retained observation version {} is unsupported; expected {}",
                retained.version, RETAINED_OBSERVATION_VERSION
            ),
        );
    }
    if !is_sha256_hex(&retained.digest) {
        return projection_failure(
            "observations.invalid_digest",
            "retained observation carries an invalid digest",
        );
    }
    // A content-addressed file that does not hash to its own name is
    // corruption, not a stale copy: refuse it rather than replay it.
    if observation_digest(&retained.batch)? != retained.digest {
        return projection_failure(
            "observations.digest_mismatch",
            format!(
                "retained observation `{}` does not hash to its recorded digest",
                retained.digest
            ),
        );
    }
    Ok(retained)
}

/// Every retained batch under `observations_dir`, oldest generation first.
/// Non-blob debris (temp files left by a crash between create and rename) is
/// skipped rather than parsed.
pub(crate) fn list_retained(
    observations_dir: &Path,
) -> Result<Vec<(std::path::PathBuf, RetainedObservationBatch)>> {
    let mut retained = Vec::new();
    let Ok(prefixes) = fs::read_dir(observations_dir) else {
        return Ok(retained);
    };
    for prefix in prefixes.filter_map(Result::ok) {
        if !prefix
            .file_type()
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(entries) = fs::read_dir(prefix.path()) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(digest) = name.strip_suffix(".json") else {
                continue;
            };
            if !is_sha256_hex(digest) {
                continue;
            }
            retained.push((entry.path(), read_retained(&entry.path())?));
        }
    }
    retained.sort_by(|left, right| {
        left.1
            .generation
            .cmp(&right.1.generation)
            .then(left.1.digest.cmp(&right.1.digest))
    });
    Ok(retained)
}

impl RetainedObservationBatch {
    pub fn reference(&self) -> RetainedObservationRef {
        RetainedObservationRef {
            digest: self.digest.clone(),
            batch_id: self.batch.batch_id.clone(),
            generation: self.generation,
            accepted_at_unix: self.accepted_at_unix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retained(generation: u64, accepted_at_unix: u64) -> RetainedObservationBatch {
        RetainedObservationBatch {
            version: RETAINED_OBSERVATION_VERSION,
            scope_id: "scope".into(),
            graph_id: "assets".into(),
            generation,
            accepted_at_unix,
            digest: "a".repeat(64),
            batch: ObservationBatch {
                batch_id: "batch".into(),
                source_connector: "synthetic-api".into(),
                source_scope: "dataset:fixture".into(),
                reconciliation_mode: crate::ReconciliationMode::Incremental,
                observations: Vec::new(),
                checkpoint_transition: Default::default(),
            },
        }
    }

    /// Current plus prior generation, plus everything inside the window.
    #[test]
    fn retention_keeps_current_prior_and_the_window() {
        let policy = ObservationRetentionPolicy {
            retained_generations: 2,
            retention_window_secs: 100,
        };
        let now = 1_000;
        assert_eq!(policy.horizon(5), 4);
        assert!(
            policy.retains(&retained(5, 0), 5, now),
            "current generation"
        );
        assert!(policy.retains(&retained(4, 0), 5, now), "prior generation");
        assert!(
            !policy.retains(&retained(3, 0), 5, now),
            "older than both windows"
        );
        assert!(
            policy.retains(&retained(3, 950), 5, now),
            "older generation still inside the time window"
        );
        assert!(
            !policy.retains(&retained(3, 899), 5, now),
            "outside both windows"
        );
    }

    /// The generation window never reaches below the first generation, and a
    /// zero policy still keeps the accepted generation.
    #[test]
    fn the_generation_horizon_is_clamped() {
        let policy = ObservationRetentionPolicy {
            retained_generations: 0,
            retention_window_secs: 0,
        };
        assert_eq!(policy.horizon(1), 1);
        assert_eq!(policy.horizon(9), 9);
        assert!(policy.retains(&retained(9, 0), 9, 10_000));
        assert!(!policy.retains(&retained(8, 0), 9, 10_000));
    }
}
