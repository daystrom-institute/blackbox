use super::Provider;

/// Session-store discovery for a provider (daemon-side: reaches local session
/// stores). Part of the provider dispatch surface — see
/// [`super::dispatch_prelude`].
pub trait ProviderSession {
    /// Locate the cwd a prior session was recorded in. Enables agents to resume
    /// across repo boundaries without hand-passing project_dir. Returns None
    /// when the provider has no cwd-aware session store or when the session
    /// can't be found locally.
    fn resolve_session_cwd(&self, session_id: &str) -> Option<std::path::PathBuf>;
}

impl ProviderSession for Provider {
    fn resolve_session_cwd(&self, _session_id: &str) -> Option<std::path::PathBuf> {
        // Neither lane offers cwd-aware session discovery here: the harness
        // persists sessions in its own store, and a claude-lane session's
        // cwd is recorded on the daemon task rather than looked up in the
        // CLI's projects dir. Resume needs an explicit project_dir.
        let _ = self;
        None
    }
}
