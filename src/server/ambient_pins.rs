use crate::pins::AmbientPinQuery;
use crate::server::BlackboxServer;
impl BlackboxServer {
    pub(crate) fn ambient_pin_block(
        &self,
        project_dir: Option<&str>,
        bro_name: Option<&str>,
        session_id: Option<&str>,
        thread_id: Option<&str>,
        work_item_id: Option<&str>,
    ) -> Option<String> {
        // A worktree dispatch cwd resolves to its registered base — the
        // durable pin scope — while the literal cwd stays matchable as an
        // alias so pins keyed to the worktree path (pre-rescope writes)
        // keep injecting for the same work.
        let resolved = match project_dir {
            Some(raw) => match self.resolve_project_write_scope(raw) {
                Ok((scope, _)) => Some(scope),
                Err(error) => {
                    // Authority failure must not fall back to the literal cwd:
                    // that could inject pins from a different checkout scope.
                    // Dispatch continues without ambient pins and the error is
                    // retained in daemon diagnostics for operator repair.
                    tracing::error!(
                        project = raw,
                        %error,
                        "ambient pin scope authority resolution failed"
                    );
                    return None;
                }
            },
            None => None,
        };
        let alias = match (project_dir, resolved.as_deref()) {
            (Some(raw), Some(scope)) if raw != scope => Some(raw),
            _ => None,
        };
        self.state.pins.read().render_for_ambient(&AmbientPinQuery {
            project: resolved.as_deref().or(project_dir),
            project_alias: alias,
            bro: bro_name,
            session_id,
            thread_id,
            work_item_id,
        })
    }
}

#[cfg(test)]
mod tests {}
