//! Doctor `storage` section: free space on every filesystem holding a store
//! root the daemon opens, and the native transcript source store's
//! accounting as its reaper cached it. Reads filesystem statistics and the
//! per-source health records only; it never walks a store.
use super::{Finding, SectionReport};
use bbox_transcript_source_store::{
    GenerationPhase, ReservePolicy, SourceStorageHealth, StorageAccounting, TranscriptSourceStore,
    nearest_existing_ancestor,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const SECTION: &str = "storage";
/// Below this available fraction any filesystem warns.
const LOW_HEADROOM_FRACTION: f64 = 0.15;
/// Stored bytes above live bytes by more than this ratio warn once a
/// generation phase completed.
const AMPLIFICATION_RATIO: f64 = 1.25;
/// A measurement older than this many reaper intervals warns.
const STALE_INTERVALS: u32 = 3;

/// What one filesystem probe reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FsProbe {
    pub(crate) device: u64,
    pub(crate) total: u64,
    pub(crate) available: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StorageRoot {
    pub(crate) label: String,
    pub(crate) path: PathBuf,
    /// The directory actually probed: the root itself, or its nearest
    /// existing ancestor (the parent first, for a file root).
    #[serde(skip)]
    pub(crate) probe_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FilesystemHeadroom {
    pub(crate) roots: Vec<StorageRoot>,
    pub(crate) size_bytes: u64,
    pub(crate) available_bytes: u64,
    pub(crate) available_fraction: f64,
    pub(crate) holds_transcript_source_store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) transcript_reserve_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnprobedRoot {
    pub(crate) label: String,
    pub(crate) path: PathBuf,
    pub(crate) error: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TranscriptSourceStorage {
    pub(crate) connector_source_id: String,
    /// `measured`, `unavailable` (never measured) or `unreadable`.
    pub(crate) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) accounting: Option<StorageAccounting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stored_to_live_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) measured_at_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) measurement_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_attempt_at_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_phase: Option<GenerationPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_attempt_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// The section's numbers at every severity, including `ok`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StorageSnapshot {
    pub(crate) filesystems: Vec<FilesystemHeadroom>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) unprobed_roots: Vec<UnprobedRoot>,
    pub(crate) transcript_sources: Vec<TranscriptSourceStorage>,
    /// One line: the worst filesystem and the worst source ratio.
    pub(crate) summary: String,
}

pub(crate) struct StorageInputs {
    /// Probe set, the transcript source store root included.
    pub(crate) roots: Vec<StorageRoot>,
    pub(crate) transcript_root: StorageRoot,
    pub(crate) reserve: ReservePolicy,
    pub(crate) reaper_interval: Duration,
    pub(crate) sources: Vec<(String, Result<Option<SourceStorageHealth>, String>)>,
    pub(crate) now_unix: u64,
}

fn probe_dir(path: &Path, directory: bool) -> PathBuf {
    let start = if directory {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    nearest_existing_ancestor(start)
}

fn probe_filesystem(path: &Path) -> std::io::Result<FsProbe> {
    let space = bbox_transcript_source_store::probe_filesystem(path)?;
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path)?.dev()
    };
    // Without device ids every probe is its own filesystem.
    #[cfg(not(unix))]
    let device = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        hasher.finish()
    };
    Ok(FsProbe {
        device,
        total: space.total,
        available: space.available,
    })
}

/// Collect the section for the running daemon.
pub(crate) fn collect(
    state: &crate::server::state::SharedState,
) -> (SectionReport, StorageSnapshot) {
    let cfg = state.config.read().clone();
    let transcript_path = cfg.paths.state_dir.join("transcript-sources");
    let transcript_root = StorageRoot {
        label: "transcript source store".into(),
        probe_dir: probe_dir(&transcript_path, true),
        path: transcript_path.clone(),
    };
    let mut roots = crate::server::instance_lock::instance_lock_roots(&cfg)
        .into_iter()
        .map(|root| StorageRoot {
            label: root.label.into(),
            probe_dir: probe_dir(&root.path, root.is_directory()),
            path: root.path,
        })
        .collect::<Vec<_>>();
    roots.push(transcript_root.clone());
    let store = TranscriptSourceStore::for_read(&transcript_path);
    let mut sources = Vec::new();
    if cfg.source_connectors.enabled {
        let mut seen = std::collections::BTreeSet::new();
        for grant in cfg
            .source_connectors
            .producers
            .iter()
            .flat_map(|producer| producer.scopes.iter())
            .filter(|grant| grant.profile == crate::config::ConnectorProfile::Transcript)
        {
            let scope = grant.scope();
            let id = scope.connector_source_id().as_str().to_owned();
            if seen.insert(id.clone()) {
                sources.push((
                    id,
                    store
                        .source_storage_health(&scope)
                        .map_err(|error| format!("{error:#}")),
                ));
            }
        }
    }
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    build(
        StorageInputs {
            roots,
            transcript_root,
            reserve: state.transcript_source.reserve_policy(),
            reaper_interval: state.transcript_source.interval(),
            sources,
            now_unix,
        },
        &probe_filesystem,
    )
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit + 1 < UNITS.len() {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{scaled:.1} {}", UNITS[unit])
    }
}

fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn root_names(roots: &[StorageRoot]) -> String {
    roots
        .iter()
        .map(|root| format!("{} ({})", root.label, root.path.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn phase_name(phase: Option<GenerationPhase>) -> &'static str {
    match phase {
        Some(GenerationPhase::Ran) => "ran",
        Some(GenerationPhase::LeaseHeld) => "found the reader lease held",
        None => "did not run",
    }
}

/// Build the section from probes and cached records. Pure, so tests can
/// supply filesystems that a test host cannot mount.
pub(crate) fn build(
    inputs: StorageInputs,
    probe: &dyn Fn(&Path) -> std::io::Result<FsProbe>,
) -> (SectionReport, StorageSnapshot) {
    let mut findings = Vec::new();
    let mut groups: Vec<(FsProbe, Vec<StorageRoot>)> = Vec::new();
    let mut unprobed = Vec::new();
    let transcript_device = probe(&inputs.transcript_root.probe_dir)
        .ok()
        .map(|found| found.device);
    for root in inputs.roots {
        match probe(&root.probe_dir) {
            Ok(found) => match groups
                .iter_mut()
                .find(|(group, _)| group.device == found.device)
            {
                Some((_, roots)) => {
                    if !roots.iter().any(|known| known.path == root.path) {
                        roots.push(root);
                    }
                }
                None => groups.push((found, vec![root])),
            },
            Err(error) => {
                findings.push(Finding::warn(format!(
                    "could not read free space for {} ({}): {error}",
                    root.label,
                    root.path.display()
                )));
                unprobed.push(UnprobedRoot {
                    label: root.label,
                    path: root.path,
                    error: error.to_string(),
                });
            }
        }
    }
    let mut filesystems = Vec::new();
    for (found, roots) in groups {
        let fraction = if found.total == 0 {
            0.0
        } else {
            found.available as f64 / found.total as f64
        };
        let holds_transcript = transcript_device == Some(found.device);
        let reserve = holds_transcript.then(|| inputs.reserve.reserve_for(found.total));
        let headroom = format!(
            "{:.1}% available ({} of {})",
            fraction * 100.0,
            bytes(found.available),
            bytes(found.total)
        );
        let names = root_names(&roots);
        findings.push(match reserve {
            Some(reserve) if found.available < reserve => Finding::action(
                format!(
                    "filesystem holding {names}: {headroom}, below the transcript ingest \
                     reserve of {}; transcript ingest admission is refusing or near refusal",
                    bytes(reserve)
                ),
                "free space on this filesystem, or lower transcript_source.min_free_bytes \
                 in the daemon config and restart",
            ),
            _ if fraction < LOW_HEADROOM_FRACTION => {
                Finding::warn(format!("filesystem holding {names}: {headroom}"))
            }
            _ => Finding::ok(format!("filesystem holding {names}: {headroom}")),
        });
        filesystems.push(FilesystemHeadroom {
            roots,
            size_bytes: found.total,
            available_bytes: found.available,
            available_fraction: fraction,
            holds_transcript_source_store: holds_transcript,
            transcript_reserve_bytes: reserve,
        });
    }

    let stale_after = inputs.reaper_interval.as_secs() * u64::from(STALE_INTERVALS);
    let mut sources = Vec::new();
    for (id, record) in inputs.sources {
        let mut row = TranscriptSourceStorage {
            connector_source_id: id.clone(),
            state: "unavailable",
            accounting: None,
            stored_to_live_ratio: None,
            measured_at_unix: None,
            measurement_age_secs: None,
            last_attempt_at_unix: None,
            generation_phase: None,
            last_attempt_error: None,
            error: None,
        };
        let health = match record {
            Err(error) => {
                findings.push(Finding::warn(format!(
                    "transcript source {id} storage record is unreadable: {error}"
                )));
                row.state = "unreadable";
                row.error = Some(error);
                sources.push(row);
                continue;
            }
            Ok(health) => health,
        };
        row.last_attempt_at_unix = health.as_ref().map(|h| h.last_attempt_at_unix);
        row.last_attempt_error = health.as_ref().and_then(|h| h.last_attempt_error.clone());
        let measured = health.as_ref().and_then(|h| {
            Some((
                h.accounting.clone()?,
                h.measured_at_unix?,
                h.generation_phase,
            ))
        });
        let Some((accounting, measured_at, phase)) = measured else {
            findings.push(Finding::info(format!(
                "transcript source {id} storage accounting unavailable: the reaper has not \
                 measured it yet"
            )));
            sources.push(row);
            continue;
        };
        let age_secs = inputs.now_unix.saturating_sub(measured_at);
        let ratio = (accounting.live_bytes > 0)
            .then(|| accounting.stored_bytes as f64 / accounting.live_bytes as f64);
        let usage = match ratio {
            Some(ratio) => format!(
                "stores {} for {} live (ratio {ratio:.2})",
                bytes(accounting.stored_bytes),
                bytes(accounting.live_bytes)
            ),
            None => format!(
                "stores {} with no live bytes",
                bytes(accounting.stored_bytes)
            ),
        };
        let detail = format!(
            "{usage}; staged chunks {}, non-current generations {}; measured {} ago, \
             generation phase {}",
            bytes(accounting.staged_chunk_bytes),
            bytes(accounting.non_current_generation_bytes),
            age(age_secs),
            phase_name(phase)
        );
        let mut flagged = false;
        if age_secs > stale_after {
            flagged = true;
            findings.push(Finding::warn(format!(
                "transcript source {id} storage measurement is {} old, older than three \
                 reaper intervals; {detail}",
                age(age_secs)
            )));
        }
        if phase == Some(GenerationPhase::Ran)
            && ratio.is_some_and(|ratio| ratio > AMPLIFICATION_RATIO)
        {
            flagged = true;
            findings.push(Finding::warn(format!(
                "transcript source {id} stores more than 1.25x its live bytes after a \
                 completed generation phase; {detail}"
            )));
        }
        if !flagged {
            findings.push(Finding::ok(format!("transcript source {id} {detail}")));
        }
        row.state = "measured";
        row.stored_to_live_ratio = ratio;
        row.measured_at_unix = Some(measured_at);
        row.measurement_age_secs = Some(age_secs);
        row.generation_phase = phase;
        row.accounting = Some(accounting);
        sources.push(row);
    }

    let worst_filesystem = filesystems
        .iter()
        .min_by(|a, b| a.available_fraction.total_cmp(&b.available_fraction))
        .map_or("no filesystem probed".to_owned(), |fs| {
            format!(
                "{} at {:.1}% available ({} of {})",
                fs.roots
                    .first()
                    .map_or_else(String::new, |root| root.path.display().to_string()),
                fs.available_fraction * 100.0,
                bytes(fs.available_bytes),
                bytes(fs.size_bytes)
            )
        });
    let worst_ratio = sources
        .iter()
        .filter_map(|row| Some((row, row.stored_to_live_ratio?)))
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(
            "no measured source with live bytes".to_owned(),
            |(row, ratio)| format!("{} at {ratio:.2}", row.connector_source_id),
        );
    let summary =
        format!("worst filesystem {worst_filesystem}; worst transcript source ratio {worst_ratio}");
    (
        SectionReport {
            section: SECTION,
            findings,
        },
        StorageSnapshot {
            filesystems,
            unprobed_roots: unprobed,
            transcript_sources: sources,
            summary,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::FindingLevel;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn root(label: &str, path: &str) -> StorageRoot {
        StorageRoot {
            label: label.into(),
            path: PathBuf::from(path),
            probe_dir: PathBuf::from(path),
        }
    }

    fn measured(
        live: u64,
        stored: u64,
        measured_at: u64,
        phase: GenerationPhase,
    ) -> SourceStorageHealth {
        SourceStorageHealth {
            schema_version: bbox_transcript_source_store::HEALTH_SCHEMA_VERSION,
            last_attempt_at_unix: measured_at,
            last_attempt_error: None,
            measured_at_unix: Some(measured_at),
            accounting: Some(StorageAccounting {
                live_bytes: live,
                stored_bytes: stored,
                ..StorageAccounting::default()
            }),
            generation_phase: Some(phase),
            last_counters: None,
        }
    }

    /// `/state` and `/index` share one filesystem; the transcript store is a
    /// separate mount under the state directory.
    fn probe(path: &Path) -> std::io::Result<FsProbe> {
        if path.starts_with("/state/transcript-sources") {
            Ok(FsProbe {
                device: 2,
                total: 100 * GIB,
                available: GIB,
            })
        } else if path.starts_with("/state") || path.starts_with("/index") {
            Ok(FsProbe {
                device: 1,
                total: 100 * GIB,
                available: 50 * GIB,
            })
        } else {
            Err(std::io::Error::other("probe refused"))
        }
    }

    fn inputs(
        sources: Vec<(String, Result<Option<SourceStorageHealth>, String>)>,
    ) -> StorageInputs {
        let transcript = root("transcript source store", "/state/transcript-sources");
        StorageInputs {
            roots: vec![
                root("state root", "/state"),
                root("transcript index", "/index"),
                root("knowledge store", "/state/knowledge.json"),
                transcript.clone(),
            ],
            transcript_root: transcript,
            reserve: ReservePolicy::Default,
            reaper_interval: Duration::from_secs(1800),
            sources,
            now_unix: 100_000,
        }
    }

    #[test]
    fn probes_deduplicate_by_filesystem_and_attribute_the_reserve_to_the_transcript_mount() {
        let (section, snapshot) = build(inputs(Vec::new()), &probe);
        assert_eq!(snapshot.filesystems.len(), 2);
        let shared = &snapshot.filesystems[0];
        assert_eq!(
            shared
                .roots
                .iter()
                .map(|root| root.label.as_str())
                .collect::<Vec<_>>(),
            ["state root", "transcript index", "knowledge store"]
        );
        assert!(!shared.holds_transcript_source_store);
        assert_eq!(shared.transcript_reserve_bytes, None);
        let transcript = &snapshot.filesystems[1];
        assert!(transcript.holds_transcript_source_store);
        assert_eq!(transcript.transcript_reserve_bytes, Some(3 * GIB));
        let levels = section
            .findings
            .iter()
            .map(|finding| finding.level)
            .collect::<Vec<_>>();
        assert_eq!(levels, [FindingLevel::Ok, FindingLevel::Action]);
        assert!(
            section.findings[1]
                .message
                .contains("transcript ingest admission")
        );
        assert!(section.findings[1].next.is_some());
        assert!(
            !section.findings[0].message.contains("admission"),
            "the transcript attribution applies only to its filesystem"
        );
        assert!(
            snapshot
                .summary
                .contains("/state/transcript-sources at 1.0%")
        );
    }

    #[test]
    fn low_headroom_warns_on_any_filesystem_and_unprobed_roots_are_named() {
        let mut low = inputs(Vec::new());
        low.roots.push(root("bro home", "/elsewhere"));
        let tight = |path: &Path| {
            probe(path).map(|found| FsProbe {
                available: found.total / 10,
                ..found
            })
        };
        let (section, snapshot) = build(low, &tight);
        // 10% available: warn everywhere; the transcript mount's default
        // reserve (3 GiB) is below its 10 GiB available.
        assert_eq!(
            section
                .findings
                .iter()
                .filter(|finding| finding.level == FindingLevel::Warn)
                .count(),
            3
        );
        assert_eq!(snapshot.unprobed_roots.len(), 1);
        assert_eq!(snapshot.unprobed_roots[0].label, "bro home");
    }

    #[test]
    fn cached_accounting_reports_age_ratio_unavailable_and_zero_live_states() {
        let now = 100_000;
        let sources = vec![
            (
                "csrc_000000000000000a".into(),
                Ok(Some(measured(1000, 1100, now - 60, GenerationPhase::Ran))),
            ),
            (
                "csrc_000000000000000b".into(),
                Ok(Some(measured(1000, 2000, now - 60, GenerationPhase::Ran))),
            ),
            (
                "csrc_000000000000000c".into(),
                Ok(Some(measured(
                    1000,
                    1900,
                    now - 60,
                    GenerationPhase::LeaseHeld,
                ))),
            ),
            (
                "csrc_000000000000000d".into(),
                Ok(Some(measured(
                    1000,
                    1000,
                    now - 4 * 1800,
                    GenerationPhase::Ran,
                ))),
            ),
            (
                "csrc_000000000000000e".into(),
                Ok(Some(measured(0, 500, now - 60, GenerationPhase::Ran))),
            ),
            ("csrc_000000000000000f".into(), Ok(None)),
            ("csrc_0000000000000010".into(), Err("bad record".into())),
        ];
        let (section, snapshot) = build(inputs(sources), &probe);
        let source_findings = section
            .findings
            .iter()
            .filter(|finding| finding.message.starts_with("transcript source "))
            .map(|finding| (finding.level, finding.message.clone()))
            .collect::<Vec<_>>();
        let level_of = |id: &str| {
            source_findings
                .iter()
                .filter(|(_, message)| message.contains(id))
                .map(|(level, _)| *level)
                .collect::<Vec<_>>()
        };
        assert_eq!(level_of("000a"), [FindingLevel::Ok]);
        assert_eq!(
            level_of("000b"),
            [FindingLevel::Warn],
            "amplified after a generation phase"
        );
        assert_eq!(
            level_of("000c"),
            [FindingLevel::Ok],
            "lease held: not yet reclaimable"
        );
        assert_eq!(
            level_of("000d"),
            [FindingLevel::Warn],
            "older than three intervals"
        );
        assert_eq!(level_of("000e"), [FindingLevel::Ok]);
        assert_eq!(level_of("000f"), [FindingLevel::Info]);
        assert_eq!(level_of("0010"), [FindingLevel::Warn]);

        let row = |id: &str| {
            snapshot
                .transcript_sources
                .iter()
                .find(|row| row.connector_source_id.ends_with(id))
                .unwrap()
        };
        assert_eq!(row("000a").state, "measured");
        assert_eq!(row("000a").measurement_age_secs, Some(60));
        assert_eq!(row("000a").stored_to_live_ratio, Some(1.1));
        assert_eq!(row("000d").measurement_age_secs, Some(4 * 1800));
        assert_eq!(row("000e").stored_to_live_ratio, None);
        assert_eq!(row("000e").accounting.as_ref().unwrap().stored_bytes, 500);
        assert!(
            level_of("000e").len() == 1
                && source_findings
                    .iter()
                    .any(|(_, m)| m.contains("000e stores 500 B with no live bytes"))
        );
        assert_eq!(row("000f").state, "unavailable");
        assert_eq!(row("0010").state, "unreadable");
        assert!(
            snapshot
                .summary
                .contains("worst transcript source ratio csrc_000000000000000b at 2.00")
        );
    }
}
