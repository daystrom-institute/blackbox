//! Issuances of the bound-workspace render plans this daemon handed out.
//!
//! A workspace render plan leaves the daemon with its daemon-clock issuance
//! beside the plan bytes, and the completion echoes that issuance back. Only
//! an issuance this daemon remembers orders the completion against owner
//! renders of the same checkout scope; any other completion is ordered after
//! every issued render.

use std::collections::BTreeSet;
use std::sync::Arc;

use parking_lot::Mutex;

const MAX_WORKSPACE_ISSUANCES: usize = 4_096;

/// Workspace plans this daemon issued, as `(issued_at_ms, plan_sha256)`. In
/// memory and bounded: a restart forgets every issuance, and the oldest are
/// evicted first.
#[derive(Clone, Default)]
pub struct RenderIssuancesV1 {
    workspace_issuances: Arc<Mutex<BTreeSet<(u64, String)>>>,
}

impl RenderIssuancesV1 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember that this daemon issued the workspace plan `plan_sha256` at
    /// `issued_at_ms`.
    pub fn note_workspace_issuance(&self, plan_sha256: &str, issued_at_ms: u64) {
        let mut issuances = self.workspace_issuances.lock();
        issuances.insert((issued_at_ms, plan_sha256.to_owned()));
        while issuances.len() > MAX_WORKSPACE_ISSUANCES {
            issuances.pop_first();
        }
    }

    /// Whether this daemon issued the workspace plan `plan_sha256` at
    /// `issued_at_ms`.
    pub fn issued_workspace_plan(&self, plan_sha256: &str, issued_at_ms: u64) -> bool {
        self.workspace_issuances
            .lock()
            .contains(&(issued_at_ms, plan_sha256.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_remembered_issuance_is_confirmed() {
        let issuances = RenderIssuancesV1::new();
        issuances.note_workspace_issuance("a", 100);
        assert!(issuances.issued_workspace_plan("a", 100));
        assert!(!issuances.issued_workspace_plan("a", 101));
        assert!(!issuances.issued_workspace_plan("b", 100));
        // A fresh runtime (a restart) remembers nothing.
        assert!(!RenderIssuancesV1::new().issued_workspace_plan("a", 100));
    }

    #[test]
    fn the_oldest_issuances_are_evicted_first() {
        let issuances = RenderIssuancesV1::new();
        for issued_at_ms in 0..=MAX_WORKSPACE_ISSUANCES as u64 {
            issuances.note_workspace_issuance("plan", issued_at_ms);
        }
        assert!(!issuances.issued_workspace_plan("plan", 0));
        assert!(issuances.issued_workspace_plan("plan", 1));
        assert!(issuances.issued_workspace_plan("plan", MAX_WORKSPACE_ISSUANCES as u64));
    }
}
