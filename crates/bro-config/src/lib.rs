//! Connection and dispatch configuration shared by the daemon and headless CLI.

mod client;
mod dispatch;
mod mcp;

pub use client::{bearer_headers, client_bearer, daemon_port, daemon_url};
pub use dispatch::{DispatchConfig, ProjectDispatch};
pub use mcp::McpServerConfig;

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub(crate) struct SavedEnv(&'static str, Option<std::ffi::OsString>);

#[cfg(test)]
impl SavedEnv {
    /// Call while holding `test_env_lock`.
    pub(crate) fn capture(key: &'static str) -> Self {
        Self(key, std::env::var_os(key))
    }
}

#[cfg(test)]
impl Drop for SavedEnv {
    fn drop(&mut self) {
        // SAFETY: tests retain the environment lock until this guard is dropped.
        unsafe {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }
}
