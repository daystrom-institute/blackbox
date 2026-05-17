use crate::config;
use crate::dispatch_mcp::dispatch_mcp_url;
use crate::util;
use std::io;
use std::path::{Path, PathBuf};

pub(super) fn init_logging(home: &Path, migrated: Vec<String>) {
    let log_dir = util::blackbox_log_dir(home);
    std::fs::create_dir_all(&log_dir).expect("failed to create log directory");
    let file_appender = tracing_appender::rolling::Builder::new()
        .max_log_files(3)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("blackbox")
        .filename_suffix("log")
        .build(&log_dir)
        .expect("failed to create log appender");

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "blackbox=info".into());

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false),
        )
        .init();

    std::panic::set_hook(Box::new(|info| {
        tracing::error!("PANIC: {}", info);
    }));
    for msg in migrated {
        tracing::info!("migrated legacy blackbox path: {msg}");
    }
}

fn expand_home_path(home: &Path, path: &str) -> PathBuf {
    if path.starts_with('~') {
        home.join(&path[2..])
    } else {
        PathBuf::from(path)
    }
}

pub(super) fn discover_transcript_roots(
    cfg: &config::Config,
    home: &Path,
) -> Vec<(String, PathBuf)> {
    if let Some(ref roots_str) = cfg.transcripts.roots {
        return roots_str
            .split(',')
            .filter_map(|entry| {
                let (name, path) = entry.split_once('=')?;
                Some((name.to_string(), expand_home_path(home, path)))
            })
            .collect();
    }

    if let Ok(val) = std::env::var("TRANSCRIPT_SEARCH_ROOTS") {
        return val
            .split(',')
            .filter_map(|entry| {
                let (name, path) = entry.split_once('=')?;
                Some((name.to_string(), expand_home_path(home, path)))
            })
            .collect();
    }

    let mut found = vec![("claude".to_string(), home.join(".claude"))];
    if let Ok(entries) = std::fs::read_dir(home) {
        let mut extras: Vec<(String, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with(".claude-")
                    && !name.contains("shared")
                    && e.path().join("projects").exists()
            })
            .map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let label = name.trim_start_matches(".claude-").to_string();
                (label, e.path())
            })
            .collect();
        extras.sort_by(|a, b| a.0.cmp(&b.0));
        found.extend(extras);
    }
    found
}

pub(super) fn resolve_codex_root(cfg: &config::Config, home: &Path) -> Option<PathBuf> {
    cfg.transcripts
        .codex_root
        .clone()
        .map(|p| {
            if p.to_string_lossy().starts_with('~') {
                home.join(&p.to_string_lossy()[2..])
            } else {
                p
            }
        })
        .or_else(|| {
            std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
                .ok()
                .map(PathBuf::from)
        })
        .or_else(|| {
            let default = home.join(".codex");
            if default.join("sessions").exists() {
                Some(default)
            } else {
                None
            }
        })
}

pub(super) fn configure_dispatch_mcp_env(cfg: &config::Config) {
    let bbox_url = dispatch_mcp_url(&cfg.daemon.bind, cfg.daemon.port);
    let bbox_mcp_name = cfg.daemon.mcp_name.clone();
    // Export for provider arg-builders so they can inject `--mcp-config`
    // etc. at dispatch time. Provider-owned MCP config files are never
    // rewritten on daemon startup; persistent registration is user-owned
    // or happens only through explicit `bro_mcp` calls.
    unsafe {
        std::env::set_var("BLACKBOX_MCP_URL", &bbox_url);
    }
    unsafe {
        std::env::set_var("BLACKBOX_MCP_NAME", &bbox_mcp_name);
    }
    tracing::info!(
        "blackbox MCP dispatch injection configured (name={}, url={})",
        bbox_mcp_name,
        bbox_url
    );
}
