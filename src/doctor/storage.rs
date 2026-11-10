//! Doctor `storage` section: free space on every filesystem holding a store
//! root the daemon opens, and the native transcript source store's
//! accounting. Both come from cached daemon state (the headroom sampler's
//! latest sample and the reaper's latest measurements), so doctor never
//! probes or walks a store and its body stays byte-stable until one of them
//! is replaced. Times are absolute; nothing serialized depends on when the
//! report was rendered. Each observation is judged stale on its own clock.
use super::{Finding, SectionReport};
use crate::server::transcript_source_reaper::{
    FsProbe, HeadroomSample, ProbedRoot, StorageObservation, StorageRoot,
};
use bbox_transcript_source_store::{
    GenerationPhase, ReservePolicy, SourceStorageHealth, StorageAccounting,
};
use serde::Serialize;
use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const SECTION: &str = "storage";
/// Below this available fraction any filesystem warns.
const LOW_HEADROOM_FRACTION: f64 = 0.15;
/// Stored bytes above live bytes by more than this ratio warn once a
/// generation phase completed.
const AMPLIFICATION_RATIO: f64 = 1.25;
/// A headroom sample or source measurement older than this many reaper
/// intervals warns.
const STALE_INTERVALS: u32 = 3;

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
    pub(crate) path: std::path::PathBuf,
    pub(crate) error: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TranscriptSourceStorage {
    pub(crate) connector_source_id: String,
    /// Whether a current producer grant enrolls the source. A source whose
    /// grant was removed keeps its stored bytes and stays reported.
    pub(crate) configured: bool,
    /// `measured` or `unavailable` (never measured).
    pub(crate) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) accounting: Option<StorageAccounting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stored_to_live_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) measured_at_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_attempt_at_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_phase: Option<GenerationPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_attempt_error: Option<String>,
}

/// The section's numbers at every severity, including `ok`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StorageSnapshot {
    /// When filesystem headroom was last sampled; `None` before the first
    /// sample completes.
    pub(crate) sampled_at_unix: Option<u64>,
    /// When the reaper last published source measurements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sources_measured_at_unix: Option<u64>,
    pub(crate) filesystems: Vec<FilesystemHeadroom>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) unprobed_roots: Vec<UnprobedRoot>,
    pub(crate) transcript_sources: Vec<TranscriptSourceStorage>,
    /// One line: the sample time, the worst filesystem and the worst source
    /// ratio.
    pub(crate) summary: String,
}

pub(crate) struct StorageInputs {
    pub(crate) observation: StorageObservation,
    /// Native transcript sources enrolled by current producer grants.
    pub(crate) configured_sources: Vec<String>,
    pub(crate) reserve: ReservePolicy,
    pub(crate) reaper_interval: Duration,
    pub(crate) now_unix: u64,
}

/// Collect the section for the running daemon.
pub(crate) fn collect(
    state: &crate::server::state::SharedState,
) -> (SectionReport, StorageSnapshot) {
    let configured_sources = {
        let cfg = state.config.read();
        let mut ids = BTreeSet::new();
        if cfg.source_connectors.enabled {
            for grant in cfg
                .source_connectors
                .producers
                .iter()
                .flat_map(|producer| producer.scopes.iter())
                .filter(|grant| grant.profile == crate::config::ConnectorProfile::Transcript)
            {
                ids.insert(grant.connector_source_id.as_str().to_owned());
            }
        }
        ids.into_iter().collect()
    };
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    build(StorageInputs {
        observation: state.transcript_source.observation(),
        configured_sources,
        reserve: state.transcript_source.reserve_policy(),
        reaper_interval: state.transcript_source.interval(),
        now_unix,
    })
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

fn timestamp(unix: u64) -> String {
    i64::try_from(unix)
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map_or_else(
            || format!("unix {unix}"),
            |time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
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

fn filesystem_findings(
    observation: &HeadroomSample,
    reserve_policy: ReservePolicy,
    findings: &mut Vec<Finding>,
) -> (Vec<FilesystemHeadroom>, Vec<UnprobedRoot>) {
    let sampled = timestamp(observation.sampled_at_unix);
    let transcript_device = observation
        .transcript_root
        .probe
        .as_ref()
        .ok()
        .map(|found| found.device);
    let mut groups: Vec<(FsProbe, Vec<StorageRoot>)> = Vec::new();
    let mut unprobed = Vec::new();
    for ProbedRoot { root, probe } in &observation.roots {
        match probe {
            Ok(found) => match groups
                .iter_mut()
                .find(|(group, _)| group.device == found.device)
            {
                Some((_, roots)) => {
                    if !roots.iter().any(|known| known.path == root.path) {
                        roots.push(root.clone());
                    }
                }
                None => groups.push((*found, vec![root.clone()])),
            },
            Err(error) => {
                findings.push(Finding::warn(format!(
                    "could not read free space for {} ({}) at {sampled}: {error}",
                    root.label,
                    root.path.display()
                )));
                unprobed.push(UnprobedRoot {
                    label: root.label.clone(),
                    path: root.path.clone(),
                    error: error.clone(),
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
        let reserve = holds_transcript.then(|| reserve_policy.reserve_for(found.total));
        let headroom = format!(
            "{:.1}% available ({} of {}) at {sampled}",
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
    (filesystems, unprobed)
}

fn source_row(
    id: &str,
    configured: bool,
    health: Option<&SourceStorageHealth>,
    stale_after: u64,
    now_unix: u64,
    findings: &mut Vec<Finding>,
) -> TranscriptSourceStorage {
    let label = if configured {
        format!("transcript source {id}")
    } else {
        format!("transcript source {id} (no current grant)")
    };
    let mut row = TranscriptSourceStorage {
        connector_source_id: id.to_owned(),
        configured,
        state: "unavailable",
        accounting: None,
        stored_to_live_ratio: None,
        measured_at_unix: None,
        last_attempt_at_unix: health.map(|h| h.last_attempt_at_unix),
        generation_phase: None,
        last_attempt_error: health.and_then(|h| h.last_attempt_error.clone()),
    };
    let measured = health.and_then(|h| {
        Some((
            h.accounting.clone()?,
            h.measured_at_unix?,
            h.generation_phase,
        ))
    });
    let Some((accounting, measured_at, phase)) = measured else {
        findings.push(Finding::info(format!(
            "{label} storage accounting unavailable: the reaper has not measured it yet"
        )));
        return row;
    };
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
        "{usage}; staged chunks {}, non-current generations {}; measured at {}, \
         generation phase {}",
        bytes(accounting.staged_chunk_bytes),
        bytes(accounting.non_current_generation_bytes),
        timestamp(measured_at),
        phase_name(phase)
    );
    let mut flagged = false;
    if now_unix.saturating_sub(measured_at) > stale_after {
        flagged = true;
        findings.push(Finding::warn(format!(
            "{label} storage measurement is older than three reaper intervals; {detail}"
        )));
    }
    if phase == Some(GenerationPhase::Ran) && ratio.is_some_and(|ratio| ratio > AMPLIFICATION_RATIO)
    {
        flagged = true;
        findings.push(Finding::warn(format!(
            "{label} stores more than 1.25x its live bytes after a completed generation \
             phase; {detail}"
        )));
    }
    if !flagged {
        findings.push(Finding::ok(format!("{label} {detail}")));
    }
    row.state = "measured";
    row.stored_to_live_ratio = ratio;
    row.measured_at_unix = Some(measured_at);
    row.generation_phase = phase;
    row.accounting = Some(accounting);
    row
}

/// Build the section from the reaper's observation and the configured
/// sources. Pure, so tests can supply filesystems a test host cannot mount.
pub(crate) fn build(inputs: StorageInputs) -> (SectionReport, StorageSnapshot) {
    let mut findings = Vec::new();
    let stale_after = inputs.reaper_interval.as_secs() * u64::from(STALE_INTERVALS);
    let headroom = inputs.observation.headroom.as_ref();
    let (filesystems, unprobed) = match headroom {
        Some(sample) => {
            // Judged on its own clock, so a daemon with no sources still
            // learns that its headroom numbers stopped refreshing.
            if inputs.now_unix.saturating_sub(sample.sampled_at_unix) > stale_after {
                findings.push(Finding::warn(format!(
                    "filesystem headroom was last sampled at {}, older than three reaper \
                     intervals; the free space below may be outdated (transcript ingest \
                     admission still reads live free space)",
                    timestamp(sample.sampled_at_unix)
                )));
            }
            filesystem_findings(sample, inputs.reserve, &mut findings)
        }
        None => {
            findings.push(Finding::info(
                "filesystem headroom not sampled yet: the daemon samples every store \
                 filesystem shortly after start and then periodically",
            ));
            (Vec::new(), Vec::new())
        }
    };

    // Configured sources, plus every source the reaper measured, including
    // retained sources whose grant was removed.
    let configured = inputs
        .configured_sources
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let measured = inputs.observation.sources.as_ref();
    let mut ids = configured.clone();
    if let Some(measured) = measured {
        ids.extend(measured.sources.keys().cloned());
    }
    let sources = ids
        .iter()
        .map(|id| {
            source_row(
                id,
                configured.contains(id),
                measured.and_then(|measured| measured.sources.get(id)),
                stale_after,
                inputs.now_unix,
                &mut findings,
            )
        })
        .collect::<Vec<_>>();

    let worst_filesystem = filesystems
        .iter()
        .min_by(|a, b| a.available_fraction.total_cmp(&b.available_fraction))
        .map_or("not sampled".to_owned(), |fs| {
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
    let sampled = headroom.map_or("headroom not sampled".to_owned(), |sample| {
        format!("headroom sampled at {}", timestamp(sample.sampled_at_unix))
    });
    let summary = format!(
        "{sampled}; worst filesystem {worst_filesystem}; worst transcript source ratio \
         {worst_ratio}"
    );
    (
        SectionReport {
            section: SECTION,
            findings,
        },
        StorageSnapshot {
            sampled_at_unix: headroom.map(|sample| sample.sampled_at_unix),
            sources_measured_at_unix: measured.map(|measured| measured.measured_at_unix),
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
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

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
    fn probe(path: &Path) -> Result<FsProbe, String> {
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
            Err("probe refused".into())
        }
    }

    fn observation(
        extra: &[StorageRoot],
        tighten: bool,
        sources: BTreeMap<String, SourceStorageHealth>,
    ) -> StorageObservation {
        let sample = |root: &StorageRoot| ProbedRoot {
            root: root.clone(),
            probe: probe(&root.probe_dir).map(|found| FsProbe {
                available: if tighten {
                    found.total / 10
                } else {
                    found.available
                },
                ..found
            }),
        };
        let transcript = sample(&root(
            "transcript source store",
            "/state/transcript-sources",
        ));
        let mut roots = [
            root("state root", "/state"),
            root("transcript index", "/index"),
            root("knowledge store", "/state/knowledge.json"),
        ]
        .iter()
        .chain(extra)
        .map(sample)
        .collect::<Vec<_>>();
        roots.push(transcript.clone());
        StorageObservation {
            headroom: Some(HeadroomSample {
                sampled_at_unix: 99_000,
                roots,
                transcript_root: transcript,
            }),
            sources: (!sources.is_empty()).then(|| {
                crate::server::transcript_source_reaper::SourceMeasurements {
                    measured_at_unix: 99_000,
                    sources,
                }
            }),
        }
    }

    fn inputs(observation: Option<StorageObservation>, configured: &[&str]) -> StorageInputs {
        StorageInputs {
            observation: observation.unwrap_or_default(),
            configured_sources: configured.iter().map(|id| id.to_string()).collect(),
            reserve: ReservePolicy::Default,
            reaper_interval: Duration::from_secs(1800),
            now_unix: 100_000,
        }
    }

    #[test]
    fn probes_deduplicate_by_filesystem_and_attribute_the_reserve_to_the_transcript_mount() {
        let (section, snapshot) =
            build(inputs(Some(observation(&[], false, BTreeMap::new())), &[]));
        assert_eq!(snapshot.sampled_at_unix, Some(99_000));
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
        let (section, snapshot) = build(inputs(
            Some(observation(
                &[root("bro home", "/elsewhere")],
                true,
                BTreeMap::new(),
            )),
            &[],
        ));
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
    fn an_unsampled_daemon_reports_configured_sources_as_unavailable() {
        let (section, snapshot) = build(inputs(None, &["csrc_000000000000000a"]));
        assert_eq!(snapshot.sampled_at_unix, None);
        assert!(snapshot.filesystems.is_empty());
        assert_eq!(snapshot.transcript_sources.len(), 1);
        assert_eq!(snapshot.transcript_sources[0].state, "unavailable");
        assert!(
            section
                .findings
                .iter()
                .all(|finding| finding.level == FindingLevel::Info)
        );
    }

    #[test]
    fn cached_accounting_reports_ratio_staleness_unavailable_retained_and_zero_live_states() {
        let now = 100_000;
        let sources = BTreeMap::from([
            (
                "csrc_000000000000000a".to_owned(),
                measured(1000, 1100, now - 60, GenerationPhase::Ran),
            ),
            (
                "csrc_000000000000000b".to_owned(),
                measured(1000, 2000, now - 60, GenerationPhase::Ran),
            ),
            (
                "csrc_000000000000000c".to_owned(),
                measured(1000, 1900, now - 60, GenerationPhase::LeaseHeld),
            ),
            (
                "csrc_000000000000000d".to_owned(),
                measured(1000, 1000, now - 4 * 1800, GenerationPhase::Ran),
            ),
            (
                "csrc_000000000000000e".to_owned(),
                measured(0, 500, now - 60, GenerationPhase::Ran),
            ),
            (
                "csrc_0000000000000010".to_owned(),
                measured(700, 700, now - 60, GenerationPhase::Ran),
            ),
        ]);
        let configured = [
            "csrc_000000000000000a",
            "csrc_000000000000000b",
            "csrc_000000000000000c",
            "csrc_000000000000000d",
            "csrc_000000000000000e",
            "csrc_000000000000000f",
        ];
        let (section, snapshot) =
            build(inputs(Some(observation(&[], false, sources)), &configured));
        let level_of = |id: &str| {
            section
                .findings
                .iter()
                .filter(|finding| {
                    finding.message.starts_with("transcript source ")
                        && finding.message.contains(id)
                })
                .map(|finding| finding.level)
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
        assert_eq!(level_of("0010"), [FindingLevel::Ok]);

        let row = |id: &str| {
            snapshot
                .transcript_sources
                .iter()
                .find(|row| row.connector_source_id.ends_with(id))
                .unwrap()
        };
        assert_eq!(snapshot.transcript_sources.len(), 7);
        assert_eq!(row("000a").state, "measured");
        assert_eq!(row("000a").measured_at_unix, Some(now - 60));
        assert_eq!(row("000a").stored_to_live_ratio, Some(1.1));
        assert_eq!(row("000e").stored_to_live_ratio, None);
        assert_eq!(row("000e").accounting.as_ref().unwrap().stored_bytes, 500);
        assert!(section.findings.iter().any(|finding| {
            finding
                .message
                .contains("000e stores 500 B with no live bytes")
        }));
        assert_eq!(row("000f").state, "unavailable");
        assert!(row("000f").configured);
        assert_eq!(row("0010").state, "measured");
        assert!(!row("0010").configured, "a retained source stays visible");
        assert!(
            snapshot
                .summary
                .contains("worst transcript source ratio csrc_000000000000000b at 2.00")
        );
    }

    #[test]
    fn an_old_headroom_sample_warns_even_with_no_sources_and_dates_the_summary() {
        let mut old = observation(&[], false, BTreeMap::new());
        let sample = old.headroom.as_mut().unwrap();
        // Now is 100_000; three intervals of 1800s is 5400s; this is 7200s old.
        sample.sampled_at_unix = 100_000 - 7200;
        let (section, snapshot) = build(inputs(Some(old), &[]));
        assert!(snapshot.transcript_sources.is_empty());
        let stale = section
            .findings
            .iter()
            .filter(|finding| {
                finding.level == FindingLevel::Warn
                    && finding
                        .message
                        .contains("older than three reaper intervals")
            })
            .collect::<Vec<_>>();
        assert_eq!(stale.len(), 1, "{:?}", section.findings);
        assert!(stale[0].message.contains("1970-01-02T01:46:40Z"));
        assert!(
            snapshot
                .summary
                .starts_with("headroom sampled at 1970-01-02T01:46:40Z;"),
            "{}",
            snapshot.summary
        );

        // A fresh sample does not warn on age.
        let (fresh, _) = build(inputs(Some(observation(&[], false, BTreeMap::new())), &[]));
        assert!(
            fresh
                .findings
                .iter()
                .all(|finding| !finding.message.contains("older than three"))
        );
    }

    /// A batch held up by a slow mount keeps the time its first reading was
    /// taken, so it is still judged stale once that is three intervals old.
    #[test]
    fn a_delayed_headroom_batch_keeps_its_start_time_and_is_still_stale() {
        use crate::server::transcript_source_reaper::{
            HeadroomProbe, TranscriptSourceRuntime, probe_filesystem,
        };
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let slow = root.join("slow-mount");
        let first_probe_at = Arc::new(Mutex::new(None));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let probe: HeadroomProbe = {
            let slow = slow.clone();
            let first_probe_at = first_probe_at.clone();
            Arc::new(move |path: &Path| {
                first_probe_at.lock().unwrap().get_or_insert_with(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                });
                if path == slow {
                    let _ = release_rx.lock().unwrap().recv();
                }
                probe_filesystem(path)
            })
        };
        // One-second interval: three intervals is three seconds.
        let runtime = Arc::new(
            TranscriptSourceRuntime::new(
                root.join("transcript-sources"),
                &bbox_config::config::TranscriptSourceConfig {
                    reaper_interval_secs: 1,
                    ..Default::default()
                },
                vec![StorageRoot {
                    label: "slow mount".into(),
                    path: slow.clone(),
                    probe_dir: slow,
                }],
            )
            .with_probe(probe),
        );
        assert!(runtime.request_headroom_sample());
        // The slow mount holds the batch past three intervals, then returns.
        std::thread::sleep(Duration::from_millis(4200));
        release_tx.send(()).unwrap();
        for _ in 0..400 {
            if runtime.observation().headroom.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let observation = runtime.observation();
        let sample = observation.headroom.as_ref().expect("the batch published");
        let first = first_probe_at.lock().unwrap().unwrap();
        assert!(
            sample.sampled_at_unix <= first,
            "stamped {} after the first probe at {first}",
            sample.sampled_at_unix
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (section, _) = build(StorageInputs {
            observation,
            configured_sources: Vec::new(),
            reserve: ReservePolicy::Default,
            reaper_interval: runtime.interval(),
            now_unix: now,
        });
        assert!(
            section
                .findings
                .iter()
                .any(|finding| finding.level == FindingLevel::Warn
                    && finding
                        .message
                        .contains("older than three reaper intervals")),
            "{:?}",
            section.findings
        );
    }
}
