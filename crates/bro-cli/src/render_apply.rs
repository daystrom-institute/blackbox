//! `bro render` — satellite-side render apply (gap-4e2db371).
//!
//! A remote corpus daemon cannot materialize files on this machine, so the
//! daemon serves a computed plan (`GET /render/plan`) and this subcommand
//! owns the local writes: whole-file project provider files (gated on the
//! blackbox-generated header), managed-region splices into global provider
//! files, and repo-owned knowledge/gap record drops acked back via
//! `POST /render/ack`.
//!
//! The apply semantics deliberately mirror the daemon's own render module
//! (bbox-knowledge render.rs) BYTE FOR BYTE — block framing, append
//! separators, trailing-newline normalization, empty-clobber refusal — so a
//! satellite-applied render and a daemon-local render produce identical
//! files. The code is not shared because the client must not link the
//! daemon's crates (harness-daemon-boundary §7); the framing constants ride
//! `bro-protocol` instead.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bro_protocol::{
    GENERATED_HEADER_PREFIX, MANAGED_END, MANAGED_START, RenderAckResponseV1, RenderAckV1,
    RenderPlanItemV1, RenderPlanV1,
};
use clap::Args;

#[derive(Debug, Args)]
pub struct RenderArgs {
    /// Project checkout to render into (repo-owned records + provider
    /// files). Omit for a global-only render.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Render the global provider files (default when --project is absent;
    /// combine with --project for both scopes).
    #[arg(long)]
    pub global: bool,
    /// Daemon base URL. Defaults to $BLACKBOX_MCP_URL (with any /mcp suffix
    /// stripped), else the local daemon port.
    #[arg(long)]
    pub url: Option<String>,
    /// Compare only: report what would change and exit 2 when stale;
    /// write nothing, ack nothing.
    #[arg(long)]
    pub check: bool,
    /// Apply whole-file replacements that shrink an existing generated file
    /// by more than half. Off by default: a dramatic shrink usually means
    /// the daemon does not see this project's repo-owned entries (project
    /// identity mismatch, e.g. a remote daemon keying the repo by its mount
    /// path), and applying would clobber real rendered memory.
    #[arg(long)]
    pub force: bool,
}

pub async fn run(args: RenderArgs) -> Result<()> {
    let base = resolve_base_url(args.url.clone());
    let scope = match (&args.project, args.global) {
        (Some(_), true) => "both",
        (Some(_), false) => "project",
        (None, _) => "global",
    };

    let mut url = format!("{}/render/plan?scope={scope}", base.trim_end_matches('/'));
    let project = match &args.project {
        Some(p) => {
            let canon = p
                .canonicalize()
                .with_context(|| format!("project dir {} not found", p.display()))?;
            let canon_str = canon.to_string_lossy().into_owned();
            url.push_str(&format!(
                "&project={}&has_project_md={}",
                urlencode(&canon_str),
                canon.join("PROJECT.md").is_file()
            ));
            Some(canon)
        }
        None => None,
    };
    if scope != "project" {
        url.push_str(&format!(
            "&common_path={}",
            urlencode(&global_target("common")?.to_string_lossy())
        ));
    }

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            bro_fleet_client::service_authorization_header()?,
        )
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("render plan failed: {} — {}", resp.status(), resp.text().await?);
    }
    let plan: RenderPlanV1 = resp.json().await.context("parsing render plan")?;

    let mut changed = 0usize;
    let mut reports = Vec::new();
    let mut knowledge_ids = Vec::new();
    let mut gap_ids = Vec::new();

    for item in &plan.items {
        match item {
            RenderPlanItemV1::WholeFile {
                file_name,
                contents,
            } => {
                let dir = project
                    .as_ref()
                    .context("plan contains project files but no --project was given")?;
                let target = dir.join(file_name);
                match apply_whole_file(&target, contents, args.check, args.force)? {
                    ApplyOutcome::Unchanged => reports.push(format!("UNCHANGED {}", target.display())),
                    ApplyOutcome::Refused(why) => {
                        reports.push(format!("REFUSED {} — {why}", target.display()))
                    }
                    ApplyOutcome::Wrote(label) => {
                        changed += 1;
                        reports.push(format!("{label} {}", target.display()));
                    }
                }
            }
            RenderPlanItemV1::ManagedRegion { target, block } => {
                let path = global_target(target)?;
                match apply_managed_region(&path, block, args.check)? {
                    ApplyOutcome::Unchanged => reports.push(format!("UNCHANGED {}", path.display())),
                    ApplyOutcome::Refused(why) => {
                        reports.push(format!("REFUSED {} — {why}", path.display()))
                    }
                    ApplyOutcome::Wrote(label) => {
                        changed += 1;
                        reports.push(format!("{label} {}", path.display()));
                    }
                }
            }
        }
    }

    for record in &plan.repo_records {
        let dir = project
            .as_ref()
            .context("plan contains repo records but no --project was given")?;
        let target = dir.join(&record.relpath);
        let current = std::fs::read_to_string(&target).ok();
        if current.as_deref() == Some(record.contents.as_str()) {
            reports.push(format!("UNCHANGED {}", target.display()));
        } else if args.check {
            changed += 1;
            reports.push(format!("WOULD PLACE {}", target.display()));
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            write_atomic(&target, &record.contents)?;
            changed += 1;
            reports.push(format!("PLACED {}", target.display()));
        }
        if !args.check {
            match record.store.as_str() {
                "knowledge" => knowledge_ids.push(record.id.clone()),
                "gaps" => gap_ids.push(record.id.clone()),
                other => reports.push(format!("(unknown record store {other}: not acked)")),
            }
        }
    }

    if !args.check && (!knowledge_ids.is_empty() || !gap_ids.is_empty()) {
        let ack = RenderAckV1 {
            project: project
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            knowledge_ids,
            gap_ids,
        };
        let ack_url = format!("{}/render/ack", base.trim_end_matches('/'));
        let resp = client
            .post(&ack_url)
            .header(
                reqwest::header::AUTHORIZATION,
                bro_fleet_client::service_authorization_header()?,
            )
            .json(&ack)
            .send()
            .await
            .with_context(|| format!("POST {ack_url}"))?;
        if resp.status().is_success() {
            let acked: RenderAckResponseV1 = resp.json().await.context("parsing ack response")?;
            reports.push(format!(
                "ACKED {} knowledge + {} gap record(s) with the daemon",
                acked.knowledge_marked, acked.gaps_marked
            ));
        } else {
            reports.push(format!(
                "WARNING: placed records but ack failed ({}); the daemon will re-offer them (re-run is safe, writes are idempotent)",
                resp.status()
            ));
        }
    }

    for line in &reports {
        println!("{line}");
    }
    println!(
        "render {}: {} item(s), {} change(s) [hash {}]",
        if args.check { "check" } else { "apply" },
        plan.items.len() + plan.repo_records.len(),
        changed,
        &plan.render_hash[..12.min(plan.render_hash.len())]
    );
    if args.check && changed > 0 {
        std::process::exit(2);
    }
    Ok(())
}

enum ApplyOutcome {
    Unchanged,
    Refused(String),
    Wrote(&'static str),
}

/// Whole project provider file. Mirrors the daemon's
/// `should_write_project_projection` gate (write only when absent or
/// blackbox-generated) and adds a shrink guard: a replacement that halves
/// an existing generated file usually means the serving daemon does not
/// see this project's repo-owned entries (project identity mismatch) and
/// its plan is a subset, not the truth.
fn apply_whole_file(
    target: &Path,
    contents: &str,
    check: bool,
    force: bool,
) -> Result<ApplyOutcome> {
    match std::fs::read_to_string(target) {
        Ok(existing) if existing == contents => Ok(ApplyOutcome::Unchanged),
        Ok(existing) if !existing.contains(GENERATED_HEADER_PREFIX) => Ok(ApplyOutcome::Refused(
            "existing file is not blackbox-generated; move hand-authored content to PROJECT.md first"
                .into(),
        )),
        Ok(existing) if !force && contents.len() * 2 < existing.len() => Ok(ApplyOutcome::Refused(
            format!(
                "replacement shrinks the file {} -> {} bytes; the daemon may not see this \
                 project's repo-owned entries (identity mismatch). Re-run with --force if intended",
                existing.len(),
                contents.len()
            ),
        )),
        Ok(_) if check => Ok(ApplyOutcome::Wrote("WOULD WRITE")),
        Ok(_) => {
            snapshot_file(target)?;
            write_atomic(target, contents)?;
            Ok(ApplyOutcome::Wrote("WROTE"))
        }
        Err(_) if check => Ok(ApplyOutcome::Wrote("WOULD CREATE")),
        Err(_) => {
            write_atomic(target, contents)?;
            Ok(ApplyOutcome::Wrote("CREATED"))
        }
    }
}

/// Managed-region splice. Byte-for-byte mirror of the daemon's
/// plan_managed_patch/apply_managed_patch: block framing, append separator,
/// trailing-newline normalization, and the empty-clobber refusal.
fn apply_managed_region(path: &Path, body: &str, check: bool) -> Result<ApplyOutcome> {
    let block = format!("{MANAGED_START}\n{}\n{MANAGED_END}", body.trim_end());
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => {
            if check {
                return Ok(ApplyOutcome::Wrote("WOULD CREATE"));
            }
            write_atomic(path, &format!("{block}\n"))?;
            return Ok(ApplyOutcome::Wrote("CREATED"));
        }
    };

    let starts: Vec<_> = existing.match_indices(MANAGED_START).collect();
    let ends: Vec<_> = existing.match_indices(MANAGED_END).collect();
    if starts.len() > 1 || ends.len() > 1 {
        return Ok(ApplyOutcome::Refused(
            "multiple managed-region markers found".into(),
        ));
    }
    match (starts.first(), ends.first()) {
        (Some((s, _)), Some((e, _))) => {
            let existing_block = &existing[*s..*e + MANAGED_END.len()];
            if existing_block == block {
                return Ok(ApplyOutcome::Unchanged);
            }
            if body.trim().is_empty() && !region_body(existing_block).trim().is_empty() {
                return Ok(ApplyOutcome::Refused(
                    "refusing to clobber a non-empty managed region with an empty render (scoping mismatch?)"
                        .into(),
                ));
            }
            if check {
                return Ok(ApplyOutcome::Wrote("WOULD REPLACE"));
            }
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..*s]);
            out.push_str(&block);
            out.push_str(&existing[*e + MANAGED_END.len()..]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            snapshot_file(path)?;
            write_atomic(path, &out)?;
            Ok(ApplyOutcome::Wrote("REPLACED region in"))
        }
        (None, None) => {
            if check {
                return Ok(ApplyOutcome::Wrote("WOULD APPEND"));
            }
            let sep = if existing.ends_with("\n\n") || existing.is_empty() {
                ""
            } else if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            snapshot_file(path)?;
            write_atomic(path, &format!("{existing}{sep}{block}\n"))?;
            Ok(ApplyOutcome::Wrote("APPENDED region to"))
        }
        _ => Ok(ApplyOutcome::Refused(
            "unbalanced managed-region markers".into(),
        )),
    }
}

fn region_body(block: &str) -> &str {
    block
        .strip_prefix(MANAGED_START)
        .and_then(|s| s.strip_suffix(MANAGED_END))
        .unwrap_or("")
}

/// Map a plan target key to this machine's provider-memory path, honoring
/// the same env overrides the daemon documents.
fn global_target(key: &str) -> Result<PathBuf> {
    let from_env = |var: &str| std::env::var_os(var).map(PathBuf::from);
    let home = || dirs::home_dir().context("cannot resolve home dir");
    Ok(match key {
        "common" => from_env("BLACKBOX_GLOBAL_COMMON_MD")
            .map_or_else(|| home().map(|h| h.join(".blackbox/BLACKBOX.md")), Ok)?,
        "claude" => from_env("BLACKBOX_GLOBAL_CLAUDE_MD")
            .map_or_else(|| home().map(|h| h.join(".claude/CLAUDE.md")), Ok)?,
        "agents" => from_env("BLACKBOX_GLOBAL_CODEX_MD")
            .map_or_else(|| home().map(|h| h.join(".codex/AGENTS.md")), Ok)?,
        "gemini" => from_env("BLACKBOX_GLOBAL_GEMINI_MD")
            .map_or_else(|| home().map(|h| h.join(".gemini/GEMINI.md")), Ok)?,
        other => anyhow::bail!("unknown global render target '{other}'"),
    })
}

fn resolve_base_url(flag: Option<String>) -> String {
    if let Some(url) = flag {
        return url;
    }
    if let Ok(mcp) = std::env::var("BLACKBOX_MCP_URL") {
        let trimmed = mcp.trim_end_matches('/');
        return trimmed.trim_end_matches("/mcp").to_string();
    }
    format!("http://127.0.0.1:{}", bro_fleet_client::daemon_port())
}

fn backup_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("BLACKBOX_BACKUP_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::home_dir()
        .context("cannot resolve home dir")?
        .join(".local/state/blackbox/backups"))
}

fn snapshot_file(src: &Path) -> Result<Option<PathBuf>> {
    if !src.exists() {
        return Ok(None);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = backup_dir()?.join(format!("bro-render-{stamp}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(src.file_name().context("backup source has no file name")?);
    std::fs::copy(src, &dest).with_context(|| format!("backing up {}", src.display()))?;
    Ok(Some(dest))
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let tmp = path.with_extension("bbox.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_file_guard_refuses_hand_authored_files() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("AGENTS.md");
        std::fs::write(&target, "hand-authored\n").unwrap();
        let out = apply_whole_file(&target, "<!-- Generated by blackbox -->\nnew\n", false, false).unwrap();
        assert!(matches!(out, ApplyOutcome::Refused(_)));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hand-authored\n");
    }

    #[test]
    fn whole_file_writes_generated_and_fresh_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("CLAUDE.md");
        let body = "<!-- Generated by blackbox. -->\ncontent\n";
        assert!(matches!(
            apply_whole_file(&target, body, false, false).unwrap(),
            ApplyOutcome::Wrote("CREATED")
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), body);
        let body2 = "<!-- Generated by blackbox. -->\ncontent v2\n";
        std::env::var_os("HOME"); // no-op; keep clippy quiet about unused imports in cfg(test)
        // Overwrite requires a backup dir; point it into the tempdir.
        // SAFETY: process-per-test under nextest.
        unsafe { std::env::set_var("BLACKBOX_BACKUP_DIR", tmp.path().join("backups")) };
        assert!(matches!(
            apply_whole_file(&target, body2, false, false).unwrap(),
            ApplyOutcome::Wrote("WROTE")
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), body2);
    }

    #[test]
    fn managed_region_appends_replaces_and_refuses_empty_clobber() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: process-per-test under nextest.
        unsafe { std::env::set_var("BLACKBOX_BACKUP_DIR", tmp.path().join("backups")) };
        let path = tmp.path().join("CLAUDE.md");
        std::fs::write(&path, "user content\n").unwrap();

        assert!(matches!(
            apply_managed_region(&path, "alpha", false).unwrap(),
            ApplyOutcome::Wrote(_)
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            format!("user content\n\n{MANAGED_START}\nalpha\n{MANAGED_END}\n")
        );

        assert!(matches!(
            apply_managed_region(&path, "beta", false).unwrap(),
            ApplyOutcome::Wrote(_)
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("user content\n"));
        assert!(text.contains("beta"));
        assert!(!text.contains("alpha"));

        assert!(matches!(
            apply_managed_region(&path, "   ", false).unwrap(),
            ApplyOutcome::Refused(_)
        ));
        assert!(matches!(
            apply_managed_region(&path, "beta", false).unwrap(),
            ApplyOutcome::Unchanged
        ));
    }
}
