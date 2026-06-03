use super::Provider;

impl Provider {
    /// Locate the cwd a prior session was recorded in. Enables agents to resume
    /// across repo boundaries without hand-passing project_dir. Returns None
    /// when the provider has no cwd-aware session store or when the session
    /// can't be found locally.
    pub fn resolve_session_cwd(&self, _session_id: &str) -> Option<std::path::PathBuf> {
        match self {
            // Harness providers persist sessions in their own store
            // (~/.bro-harness), not the claude projects dir; no cwd-aware
            // discovery, so resume needs an explicit project_dir.
            Provider::Glm | Provider::Deepseek | Provider::Brodex | Provider::VibeBh => None,
            Provider::Workflow => None,
        }
    }
}
