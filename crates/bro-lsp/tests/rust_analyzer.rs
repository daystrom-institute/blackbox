use std::path::PathBuf;
use std::time::Duration;

use bro_lsp::{Language, LspConfig, SessionPool};
use lsp_types::DiagnosticSeverity;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_analyzer_diagnostics_follow_document_versions() -> anyhow::Result<()> {
    let Some(rust_analyzer) = rust_analyzer_bin() else {
        eprintln!("skipping rust-analyzer integration test: rust-analyzer not found");
        return Ok(());
    };

    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    std::fs::create_dir(root.join("src"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "bro_lsp_ra_fixture"
version = "0.1.0"
edition = "2024"
"#,
    )?;

    let source_path = root.join("src/lib.rs");
    let clean = r#"pub fn value() -> u32 {
    let x: u32 = 1;
    x
}
"#;
    let broken = r#"pub fn value() -> u32 {
    let x: u32 = "s";
    x
}
"#;
    std::fs::write(&source_path, clean)?;

    let pool = SessionPool::new(LspConfig {
        request_timeout: Duration::from_secs(90),
        init_timeout: Duration::from_secs(90),
        ready_timeout: Duration::from_secs(5),
        rust_analyzer_bin: Some(rust_analyzer),
        ..LspConfig::default()
    });
    let mut doc = pool
        .open_document(&root, Language::Rust, &source_path, 1, clean.to_string())
        .await?;

    pool.apply_change(&mut doc, 2, broken.to_string()).await?;
    let stale = pool.diagnostics(&doc, 1).await.unwrap_err();
    assert!(
        stale.is_superseded(),
        "expected a superseded signal for old version 1, got {stale:?}"
    );
    let broken_diagnostics = pool.diagnostics(&doc, 2).await?;
    assert!(
        broken_diagnostics
            .iter()
            .any(|diag| diag.severity == Some(DiagnosticSeverity::ERROR)),
        "expected an error diagnostic, got {broken_diagnostics:#?}"
    );

    pool.apply_change(&mut doc, 3, clean.to_string()).await?;
    let fixed_diagnostics = pool.diagnostics(&doc, 3).await?;
    assert!(
        fixed_diagnostics.is_empty(),
        "expected clean diagnostics after fix, got {fixed_diagnostics:#?}"
    );

    pool.shutdown_all().await;
    Ok(())
}

fn rust_analyzer_bin() -> Option<PathBuf> {
    if let Some(path) = env_path("BRO_LSP_RUST_ANALYZER_BIN")
        .or_else(|| env_path("BRO_RUST_ANALYZER_BIN"))
        .or_else(|| env_path("BLACKBOX_RUST_ANALYZER_BIN"))
        .filter(|path| command_runs(path))
    {
        return Some(path);
    }

    if command_runs("rust-analyzer") {
        return Some(PathBuf::from("rust-analyzer"));
    }

    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let cargo_bin = home.join(".cargo/bin/rust-analyzer");
    command_runs(&cargo_bin).then_some(cargo_bin)
}

fn command_runs(path: impl AsRef<std::ffi::OsStr>) -> bool {
    std::process::Command::new(path)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
