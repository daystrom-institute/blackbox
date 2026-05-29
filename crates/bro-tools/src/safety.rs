//! Command denylist + sensitive-file guard. Mirrors daystrom-mk2's
//! `ShellTools`/`GitWorkspaceTools` guards and the operator's standing
//! process-management and data-safety rules:
//!   - never blanket-kill by port (`lsof -t :PORT | xargs kill`, `fuser -k`)
//!   - never discard working-tree changes you did not create
//!     (`git reset --hard`, `git clean -f`, `git checkout .`)
//!   - scope destructive ops; never `rm -rf /`
//!
//! These are *categorical* refusals enforced regardless of the upstream
//! model's intent. They are not a substitute for the daemon's outer
//! supervision — they are the harness's own floor.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// Policy object threaded through `ToolCx`. Currently stateless (all rules
/// are compile-time patterns) but carried as a type so per-dispatch
/// overrides (e.g. an operator-authorized escape hatch) can be added without
/// touching tool signatures.
#[derive(Debug, Default)]
pub struct SafetyPolicy {
    _private: (),
}

impl SafetyPolicy {
    pub fn new() -> Self {
        SafetyPolicy::default()
    }

    /// Returns `Some(reason)` if a shell command is categorically denied.
    pub fn deny_command(&self, command: &str) -> Option<&'static str> {
        for (re, reason) in denylist() {
            if re.is_match(command) {
                return Some(reason);
            }
        }
        None
    }

    /// Returns `true` if a path looks like a secret/credential that must not
    /// be staged or committed.
    pub fn is_sensitive_path(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let full = path.to_string_lossy();
        for re in sensitive_patterns() {
            if re.is_match(name) || re.is_match(&full) {
                return true;
            }
        }
        false
    }
}

fn denylist() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let raw: &[(&str, &str)] = &[
            (r"rm\s+-[a-zA-Z]*r[a-zA-Z]*f[a-zA-Z]*\s+/\s*$", "rm -rf / (root wipe)"),
            (r"rm\s+-[a-zA-Z]*r[a-zA-Z]*f[a-zA-Z]*\s+/\s", "rm -rf on an absolute root path"),
            (r"\bgit\s+reset\s+--hard\b", "git reset --hard discards uncommitted work (may not be yours)"),
            (r"\bgit\s+clean\s+-[a-zA-Z]*f", "git clean -f discards untracked files (may not be yours)"),
            (r"\bgit\s+checkout\s+\.\s*$", "git checkout . discards working-tree changes"),
            (r"\bgit\s+checkout\s+--\s+\.", "git checkout -- . discards working-tree changes"),
            (r"\bpkill\b", "pkill is too broad; kill a specific known PID instead"),
            (r"\bkillall\b", "killall is too broad; kill a specific known PID instead"),
            (r"\bfuser\s+-k", "fuser -k kills by resource; never blanket-kill"),
            (r"lsof\s+-t.*\|\s*xargs\s+kill", "kill-by-port takes down unrelated processes"),
            (r"kill\s+\$\(\s*lsof", "kill-by-port takes down unrelated processes"),
            (r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:", "fork bomb"),
            (r"\bmkfs\.", "mkfs reformats a filesystem"),
            (r"dd\s+.*of=/dev/", "dd to a device node"),
            (r">\s*/dev/sd[a-z]", "raw write to a block device"),
        ];
        raw.iter()
            .map(|(p, reason)| (Regex::new(p).expect("valid denylist regex"), *reason))
            .collect()
    })
}

fn sensitive_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let raw: &[&str] = &[
            r"^\.env($|\.)",
            r"\.env$",
            r"\.(pem|key|p12|pfx|jks)$",
            r"(^|/)id_(rsa|dsa|ecdsa|ed25519)$",
            r"(^|/)credentials$",
            r"\.aws/credentials",
            r"\.gcp/.*\.json$",
            r"service[-_]?account.*\.json$",
            r"(^|/)\.npmrc$",
            r"(^|/)\.pypirc$",
        ];
        raw.iter()
            .map(|p| Regex::new(p).expect("valid sensitive-path regex"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn denies_destructive_commands() {
        let p = SafetyPolicy::new();
        for cmd in [
            "rm -rf /",
            "git reset --hard origin/main",
            "git clean -fd",
            "git checkout .",
            "pkill -f node",
            "killall chrome",
            "fuser -k 7264/tcp",
            "lsof -t -i:7264 | xargs kill -9",
        ] {
            assert!(p.deny_command(cmd).is_some(), "should deny: {cmd}");
        }
    }

    #[test]
    fn allows_scoped_commands() {
        let p = SafetyPolicy::new();
        for cmd in [
            "cargo test --lib",
            "git status",
            "git add src/main.rs",
            "rm -rf target/debug/incremental",
            "kill 48213",
            "ls -la",
        ] {
            assert!(p.deny_command(cmd).is_none(), "should allow: {cmd}");
        }
    }

    #[test]
    fn flags_sensitive_paths() {
        let p = SafetyPolicy::new();
        for path in [
            ".env",
            ".env.production",
            "config/server.pem",
            "secrets/id_rsa",
            "deploy/credentials",
            "home/.aws/credentials",
            "sa-service-account.json",
        ] {
            assert!(p.is_sensitive_path(&PathBuf::from(path)), "should flag: {path}");
        }
        for path in ["src/main.rs", "README.md", "Cargo.toml"] {
            assert!(!p.is_sensitive_path(&PathBuf::from(path)), "should pass: {path}");
        }
    }
}
