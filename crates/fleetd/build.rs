use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[allow(clippy::disallowed_methods)]
fn git_head_short(short_len: u32) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", &format!("--short={short_len}"), "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn fallback_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    let build_id = git_head_short(12).unwrap_or_else(|| fallback_timestamp().to_string());
    println!("cargo:rustc-env=FLEETD_BUILD_ID={build_id}");
}
