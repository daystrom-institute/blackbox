use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::notes::{NoteResolution, Notes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapTrailerRef {
    pub note_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapTrailerStatus {
    Addressed,
    Open(NoteResolution),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapTrailerCheck {
    pub note_id: String,
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
            if is_canonical_note_id(id) {
                Some(GapTrailerRef {
                    note_id: id.to_ascii_lowercase(),
                })
            } else {
                None
            }
        })
        .collect()
}

pub fn check_trailers(notes: &Notes, refs: &[GapTrailerRef]) -> Vec<GapTrailerCheck> {
    refs.iter()
        .map(|trailer| {
            let status = notes
                .all()
                .iter()
                .find(|note| note.id == trailer.note_id)
                .map(|note| match note.resolution {
                    NoteResolution::Addressed => GapTrailerStatus::Addressed,
                    other => GapTrailerStatus::Open(other),
                })
                .unwrap_or(GapTrailerStatus::Missing);
            GapTrailerCheck {
                note_id: trailer.note_id.clone(),
                status,
            }
        })
        .collect()
}

pub fn render_git_closeout_check(
    notes: &Notes,
    repo: &Path,
    range: Option<&str>,
) -> Result<String> {
    let refs = scan_git_trailers(repo, range)?;
    if refs.is_empty() {
        return Ok(String::new());
    }
    let checks = check_trailers(notes, &refs);
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
                out.push_str(&format!("  {} — addressed\n", check.note_id));
            }
            GapTrailerStatus::Open(resolution) => {
                out.push_str(&format!(
                    "  {} — still {}\n",
                    check.note_id,
                    resolution.as_ref()
                ));
            }
            GapTrailerStatus::Missing => {
                out.push_str(&format!("  {} — missing\n", check.note_id));
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

fn is_canonical_note_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("note-") else {
        return false;
    };
    suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{Note, NoteKind, NoteStore};
    use tempfile::tempdir;

    fn notes_with(entries: Vec<(&str, NoteResolution)>) -> Notes {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notes.json");
        let notes = entries
            .into_iter()
            .map(|(id, resolution)| Note {
                id: id.into(),
                kind: NoteKind::Followup,
                body: "{}".into(),
                task_id: None,
                session_id: None,
                project: None,
                thread_id: None,
                provider: None,
                bro: None,
                resolution,
                created_at: "2026-05-12T00:00:00Z".into(),
                updated_at: "2026-05-12T00:00:00Z".into(),
                resolved_at: None,
                resolution_note: None,
            })
            .collect();
        std::fs::write(
            &path,
            serde_json::to_string(&NoteStore { version: 1, notes }).unwrap(),
        )
        .unwrap();
        Notes::open(&path).unwrap()
    }

    #[test]
    fn scanner_recognizes_canonical_trailers() {
        let refs = scan_commit_trailers(
            "implement thing\n\nAddresses-Gap-Note: note-a1b2c3d4\naddresses-gap-note: note-ffffffff\n",
        );
        assert_eq!(
            refs,
            vec![
                GapTrailerRef {
                    note_id: "note-a1b2c3d4".into()
                },
                GapTrailerRef {
                    note_id: "note-ffffffff".into()
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
    fn trailer_checks_report_missing_addressed_and_open() {
        let notes = notes_with(vec![
            ("note-a1b2c3d4", NoteResolution::Addressed),
            ("note-11111111", NoteResolution::Unresolved),
        ]);
        let checks = check_trailers(
            &notes,
            &[
                GapTrailerRef {
                    note_id: "note-a1b2c3d4".into(),
                },
                GapTrailerRef {
                    note_id: "note-11111111".into(),
                },
                GapTrailerRef {
                    note_id: "note-22222222".into(),
                },
            ],
        );

        assert_eq!(checks[0].status, GapTrailerStatus::Addressed);
        assert_eq!(
            checks[1].status,
            GapTrailerStatus::Open(NoteResolution::Unresolved)
        );
        assert_eq!(checks[2].status, GapTrailerStatus::Missing);
    }
}
