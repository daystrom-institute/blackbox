//! Per-mount include/exclude/size policy, evaluated at enumeration time so
//! exclusion prevents FETCHING, not merely indexing.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Aligned with the walker's existing per-path byte caps
/// (`MAX_DOCUMENT_FILE_BYTES` in the corpus-index crate) so a mount never
/// fetches bytes indexing would refuse anyway. Kept as a local constant
/// rather than a cross-crate dependency - bbox-connectors does not link
/// bbox-corpus-index (materialize-then-index keeps the boundary at the
/// filesystem, not the walker's internals).
const DEFAULT_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountPolicy {
    /// Glob patterns (scope-relative). Non-empty means allowlist: a path
    /// must match at least one to be included.
    #[serde(default)]
    pub include: Vec<String>,
    /// Glob patterns (scope-relative). A match excludes regardless of
    /// `include`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Per-entry byte cap.
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
    /// Office-suite export on/off for native documents.
    #[serde(default = "default_true")]
    pub native_export: bool,
    /// Per-mount total byte cap for one sync pass. `None` = unbounded.
    /// Breaching this aborts the sync with a loud error rather than
    /// silently truncating.
    #[serde(default)]
    pub max_total_bytes: Option<u64>,
    /// Per-mount override for visual-chunk embedding participation.
    /// `None` defers to the global `[embed.routes.visual]` policy.
    #[serde(default)]
    pub visual_embed: Option<bool>,
}

fn default_true() -> bool {
    true
}

fn default_max_file_bytes() -> u64 {
    DEFAULT_MAX_FILE_BYTES
}

impl Default for MountPolicy {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            max_file_bytes: default_max_file_bytes(),
            native_export: true,
            max_total_bytes: None,
            visual_embed: None,
        }
    }
}

/// Outcome of evaluating one scope-relative path against a [`MountPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Include,
    /// Carries a human-readable reason - recorded verbatim on the
    /// manifest's `Skipped` entry state, so operators can see WHY without
    /// re-deriving it from the policy.
    Exclude(String),
}

impl MountPolicy {
    /// Evaluate whether `path` (scope-relative) should be fetched. `size`
    /// is the enumeration-time metadata size when the store reports one
    /// (native-doc exports do not, until after fetch).
    pub fn evaluate(&self, path: &str, size: Option<u64>) -> PolicyDecision {
        if !self.include.is_empty() && !self.include.iter().any(|pat| glob_match(pat, path)) {
            return PolicyDecision::Exclude(format!("`{path}` does not match any include pattern"));
        }
        if let Some(pat) = self.exclude.iter().find(|pat| glob_match(pat, path)) {
            return PolicyDecision::Exclude(format!("`{path}` matches exclude pattern `{pat}`"));
        }
        if let Some(size) = size
            && size > self.max_file_bytes
        {
            return PolicyDecision::Exclude(format!(
                "`{path}` is {size} bytes, over the {} byte per-file cap",
                self.max_file_bytes
            ));
        }
        PolicyDecision::Include
    }
}

fn glob_match(pattern: &str, path: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|p| p.matches(path))
        .unwrap_or(false)
}

/// Tracks bytes spent against a per-mount total cap for one sync pass.
/// Breaching the cap is a hard error - the sync aborts loudly rather than
/// silently truncating - and the error names the biggest offenders so an
/// operator can act without re-running with verbose logging.
#[derive(Debug, Default)]
pub struct TotalBytesBudget {
    cap: Option<u64>,
    spent: u64,
    /// Largest charges seen, path -> bytes, capped at a small top-N for the
    /// abort message.
    offenders: Vec<(String, u64)>,
}

const TOP_OFFENDERS: usize = 5;

/// A `max_total_bytes` breach, distinguished from an ordinary per-entry
/// fetch error so the driver can tell them apart: this must ABORT the
/// whole sync pass ("aborts loudly rather than silently truncating"), never
/// get folded into `SyncSummary::errors` and let the pass continue.
/// Callers that need to tell the two apart downcast via
/// `anyhow::Error::downcast_ref::<BudgetExceeded>`.
#[derive(Debug)]
pub struct BudgetExceeded(String);

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BudgetExceeded {}

impl TotalBytesBudget {
    pub fn new(cap: Option<u64>) -> Self {
        Self {
            cap,
            spent: 0,
            offenders: Vec::new(),
        }
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    /// Record `bytes` charged for `path`. Errs (with a [`BudgetExceeded`]
    /// downcastable cause) when the mount's `max_total_bytes` cap is
    /// breached.
    pub fn charge(&mut self, path: &str, bytes: u64) -> Result<()> {
        self.spent += bytes;
        self.offenders.push((path.to_string(), bytes));
        self.offenders
            .sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
        self.offenders.truncate(TOP_OFFENDERS);

        if let Some(cap) = self.cap
            && self.spent > cap
        {
            let offenders = self
                .offenders
                .iter()
                .map(|(path, bytes)| format!("{path} ({bytes} bytes)"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(BudgetExceeded(format!(
                "mount byte cap exceeded: {} bytes spent (cap {cap}) while fetching `{path}`; \
                 biggest entries this pass: {offenders}",
                self.spent
            ))
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_allowlist_rejects_non_matching_paths() {
        let policy = MountPolicy {
            include: vec!["docs/**".to_string()],
            ..Default::default()
        };
        assert_eq!(
            policy.evaluate("docs/readme.md", None),
            PolicyDecision::Include
        );
        assert!(matches!(
            policy.evaluate("src/main.rs", None),
            PolicyDecision::Exclude(_)
        ));
    }

    #[test]
    fn exclude_wins_over_include() {
        let policy = MountPolicy {
            include: vec!["**/*".to_string()],
            exclude: vec!["**/*.tmp".to_string()],
            ..Default::default()
        };
        assert!(matches!(
            policy.evaluate("a/b.tmp", None),
            PolicyDecision::Exclude(_)
        ));
        assert_eq!(policy.evaluate("a/b.rs", None), PolicyDecision::Include);
    }

    #[test]
    fn per_file_cap_excludes_oversized_entries() {
        let policy = MountPolicy {
            max_file_bytes: 100,
            ..Default::default()
        };
        assert!(matches!(
            policy.evaluate("big.bin", Some(200)),
            PolicyDecision::Exclude(_)
        ));
        assert_eq!(
            policy.evaluate("small.bin", Some(50)),
            PolicyDecision::Include
        );
    }

    #[test]
    fn total_bytes_budget_aborts_loudly_and_names_offenders() {
        let mut budget = TotalBytesBudget::new(Some(150));
        budget.charge("a.bin", 100).unwrap();
        let err = budget.charge("b.bin", 100).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mount byte cap exceeded"));
        assert!(msg.contains("b.bin"));
        assert!(msg.contains("a.bin"));
    }

    #[test]
    fn total_bytes_budget_unbounded_when_no_cap() {
        let mut budget = TotalBytesBudget::new(None);
        for i in 0..10 {
            budget.charge(&format!("f{i}"), 1_000_000).unwrap();
        }
        assert_eq!(budget.spent(), 10_000_000);
    }
}
