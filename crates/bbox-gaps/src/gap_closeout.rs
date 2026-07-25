use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::gaps::{GapResolution, GapStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapTrailerRef {
    pub gap_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapTrailerStatus {
    Addressed,
    Open(GapResolution),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapTrailerCheck {
    pub gap_id: String,
    pub status: GapTrailerStatus,
}

pub fn scan_commit_trailers(message: &str) -> Vec<GapTrailerRef> {
    message
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            if !key.trim().eq_ignore_ascii_case("Addresses-Gap-Note") {
                return None;
            }
            let id = value.trim();
            if is_canonical_gap_id(id) {
                Some(GapTrailerRef {
                    gap_id: id.to_ascii_lowercase(),
                })
            } else {
                None
            }
        })
        .collect()
}

pub fn check_trailers(gaps: &GapStore, refs: &[GapTrailerRef]) -> Vec<GapTrailerCheck> {
    refs.iter()
        .map(|trailer| {
            let status = gaps
                .all()
                .iter()
                .find(|gap| gap.id == trailer.gap_id)
                .map(|gap| match gap.resolution {
                    GapResolution::Addressed => GapTrailerStatus::Addressed,
                    other => GapTrailerStatus::Open(other),
                })
                .unwrap_or(GapTrailerStatus::Missing);
            GapTrailerCheck {
                gap_id: trailer.gap_id.clone(),
                status,
            }
        })
        .collect()
}

pub fn render_git_closeout_check(
    gaps: &GapStore,
    repo: &Path,
    range: Option<&str>,
) -> Result<String> {
    let refs = scan_git_trailers(repo, range)?;
    if refs.is_empty() {
        return Ok(String::new());
    }
    let checks = check_trailers(gaps, &refs);
    Ok(render_checks(&checks))
}

pub fn render_checks(checks: &[GapTrailerCheck]) -> String {
    if checks.is_empty() {
        return String::new();
    }

    let mut out = format!("## Gap close-out checks ({})\n", checks.len());
    for check in checks {
        match check.status {
            GapTrailerStatus::Addressed => {
                out.push_str(&format!("  {} — addressed\n", check.gap_id));
            }
            GapTrailerStatus::Open(resolution) => {
                out.push_str(&format!(
                    "  {} — still {}\n",
                    check.gap_id,
                    resolution.as_ref()
                ));
            }
            GapTrailerStatus::Missing => {
                out.push_str(&format!("  {} — missing\n", check.gap_id));
            }
        }
    }
    out.push('\n');
    out
}

fn scan_git_trailers(repo: &Path, range: Option<&str>) -> Result<Vec<GapTrailerRef>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).arg("log").arg("--format=%B");
    if let Some(range) = range.filter(|s| !s.trim().is_empty()) {
        cmd.arg(range);
    } else {
        cmd.arg("HEAD");
    }
    let output = cmd
        .output()
        .with_context(|| format!("running git log in {}", repo.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git log failed in {}: {stderr}", repo.display());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(scan_commit_trailers(&stdout))
}

fn is_canonical_gap_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("gap-") else {
        return false;
    };
    suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gaps::{GapFileParams, GapStore};
    use tempfile::tempdir;

    fn file_gap(store: &mut GapStore, slug: &str) -> String {
        store
            .file(&GapFileParams {
                title: slug.into(),
                gap_kind: "tooling".into(),
                domain: "closeout-test".into(),
                wanted_capability: "x".into(),
                dedupe_key: format!("tooling/closeout-test/{slug}"),
                impact: None,
                blocking_level: None,
                missing_primitive: None,
                fallback_used: None,
                evidence: None,
                suggested_owner: None,
                notes: None,
                scope: Some("global".into()),
                project: None,
                project_id: None,
                write_dir: None,
                task_id: None,
                session_id: None,
                provider: None,
                bro: None,
                thread_id: None,
                allow_recurrence: None,
            })
            .unwrap()
            .0
    }

    #[test]
    fn scanner_recognizes_canonical_trailers() {
        let refs = scan_commit_trailers(
            "implement thing\n\nAddresses-Gap-Note: gap-a1b2c3d4\naddresses-gap-note: gap-ffffffff\n",
        );
        assert_eq!(
            refs,
            vec![
                GapTrailerRef {
                    gap_id: "gap-a1b2c3d4".into()
                },
                GapTrailerRef {
                    gap_id: "gap-ffffffff".into()
                }
            ]
        );
    }

    #[test]
    fn scanner_ignores_bare_hex_to_avoid_false_positives() {
        let refs = scan_commit_trailers("Addresses-Gap-Note: a1b2c3d4");
        assert!(refs.is_empty());
    }

    #[test]
    fn scanner_ignores_legacy_note_trailers() {
        // Clean break: `note-<8hex>` trailers in old commits are historical and
        // no longer resolve against the gap store.
        let refs = scan_commit_trailers("Addresses-Gap-Note: note-a1b2c3d4");
        assert!(refs.is_empty());
    }

    #[test]
    fn trailer_checks_report_missing_addressed_and_open() {
        // Seed a store with two real gaps: one addressed, one unresolved.
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = GapStore::open(&root.join("gaps.json")).unwrap();
        let addressed_id = file_gap(&mut store, "addressed");
        let open_id = file_gap(&mut store, "open");
        store
            .resolve(&crate::gaps::GapResolveParams {
                id: addressed_id.clone(),
                resolution: "addressed".into(),
                ..Default::default()
            })
            .unwrap();

        let checks = check_trailers(
            &store,
            &[
                GapTrailerRef {
                    gap_id: addressed_id,
                },
                GapTrailerRef { gap_id: open_id },
                GapTrailerRef {
                    gap_id: "gap-22222222".into(),
                },
            ],
        );

        assert_eq!(checks[0].status, GapTrailerStatus::Addressed);
        assert_eq!(
            checks[1].status,
            GapTrailerStatus::Open(GapResolution::Unresolved)
        );
        assert_eq!(checks[2].status, GapTrailerStatus::Missing);
    }
}
