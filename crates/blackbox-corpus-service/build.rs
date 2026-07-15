use std::process::Command;

#[allow(clippy::disallowed_methods)]
fn main() {
    println!("cargo:rerun-if-env-changed=BLACKBOX_CORPUS_BUILD_ID");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    let build_id = std::env::var("BLACKBOX_CORPUS_BUILD_ID").unwrap_or_else(|_| {
        let revision = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".into());
        format!(
            "blackbox-corpusd-{}-{revision}",
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into())
        )
    });
    println!("cargo:rustc-env=BLACKBOX_CORPUS_BUILD_ID={build_id}");
}
