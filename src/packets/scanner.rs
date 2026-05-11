use super::*;

// ── Self-heal scanner ─────────────────────────────────────────────
//
// A periodic read-only walk over the packet event log that flags
// candidates for repair: packets whose applies miss too often (no-match
// rate above threshold) or whose audits show fidelity drift. Emits
// `op="repair_candidate"` events; does NOT auto-dispatch. An orchestrator
// or the human operator decides what to do with the signal.
//
// Off by default. Enable via `PACKET_SELF_HEAL_ENABLED=true`.
//
// This is intentionally scanner-only. A future revision may add an
// opt-in dispatcher that spawns an AST-repair agent when a candidate
// fires; that's its own feature and lives behind a separate flag.

const ENV_SELF_HEAL_ENABLED: &str = "PACKET_SELF_HEAL_ENABLED";

const ENV_SELF_HEAL_INTERVAL_SECS: &str = "PACKET_SELF_HEAL_INTERVAL_SECS";

const ENV_SELF_HEAL_WINDOW_HOURS: &str = "PACKET_SELF_HEAL_WINDOW_HOURS";

const ENV_SELF_HEAL_NO_MATCH_THRESHOLD: &str = "PACKET_SELF_HEAL_NO_MATCH_THRESHOLD";

const ENV_SELF_HEAL_FIDELITY_THRESHOLD: &str = "PACKET_SELF_HEAL_FIDELITY_THRESHOLD";

const ENV_SELF_HEAL_MIN_APPLIES: &str = "PACKET_SELF_HEAL_MIN_APPLIES";

const ENV_SELF_HEAL_COOLDOWN_HOURS: &str = "PACKET_SELF_HEAL_COOLDOWN_HOURS";

/// Default interval between scanner ticks when the env var is unset.
/// Hourly is cheap for a log-walk and matches the scale at which
/// packet behaviour drifts meaningfully.
const DEFAULT_INTERVAL_SECS: u64 = 3600;

const DEFAULT_WINDOW_HOURS: u64 = 24;

const DEFAULT_NO_MATCH_THRESHOLD: f32 = 0.2;

const DEFAULT_FIDELITY_THRESHOLD: f32 = 0.9;

const DEFAULT_MIN_APPLIES: usize = 5;

const DEFAULT_COOLDOWN_HOURS: u64 = 24;

#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub enabled: bool,
    pub interval: Duration,
    /// Trailing window over which apply / audit events are aggregated.
    pub window: Duration,
    /// Fraction of applies that must be no_match to flag (e.g. 0.2 = 20%).
    pub no_match_threshold: f32,
    /// Fidelity floor — the most recent audit below this flags the packet.
    pub fidelity_threshold: f32,
    /// Minimum apply count in the window before no_match_rate is trusted.
    /// Prevents flagging packets that have only been tried once or twice.
    pub min_apply_samples: usize,
    /// Suppression window after a candidate event fires for a packet, so
    /// the same packet isn't spammed every tick until the operator acts.
    pub repair_cooldown: Duration,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            window: Duration::from_secs(DEFAULT_WINDOW_HOURS * 3600),
            no_match_threshold: DEFAULT_NO_MATCH_THRESHOLD,
            fidelity_threshold: DEFAULT_FIDELITY_THRESHOLD,
            min_apply_samples: DEFAULT_MIN_APPLIES,
            repair_cooldown: Duration::from_secs(DEFAULT_COOLDOWN_HOURS * 3600),
        }
    }
}

impl ScannerConfig {
    pub fn from_env() -> Self {
        fn flag(name: &str) -> Option<bool> {
            std::env::var(name).ok().map(|v| {
                let v = v.to_ascii_lowercase();
                matches!(v.as_str(), "1" | "true" | "yes" | "on")
            })
        }
        fn u64_env(name: &str) -> Option<u64> {
            std::env::var(name).ok().and_then(|v| v.parse().ok())
        }
        fn f32_env(name: &str) -> Option<f32> {
            std::env::var(name).ok().and_then(|v| v.parse().ok())
        }
        fn usize_env(name: &str) -> Option<usize> {
            std::env::var(name).ok().and_then(|v| v.parse().ok())
        }

        let mut cfg = ScannerConfig::default();
        if let Some(on) = flag(ENV_SELF_HEAL_ENABLED) {
            cfg.enabled = on;
        }
        if let Some(s) = u64_env(ENV_SELF_HEAL_INTERVAL_SECS) {
            cfg.interval = Duration::from_secs(s.max(10));
        }
        if let Some(h) = u64_env(ENV_SELF_HEAL_WINDOW_HOURS) {
            cfg.window = Duration::from_secs(h.max(1) * 3600);
        }
        if let Some(t) = f32_env(ENV_SELF_HEAL_NO_MATCH_THRESHOLD) {
            cfg.no_match_threshold = t.clamp(0.0, 1.0);
        }
        if let Some(t) = f32_env(ENV_SELF_HEAL_FIDELITY_THRESHOLD) {
            cfg.fidelity_threshold = t.clamp(0.0, 1.0);
        }
        if let Some(n) = usize_env(ENV_SELF_HEAL_MIN_APPLIES) {
            cfg.min_apply_samples = n;
        }
        if let Some(h) = u64_env(ENV_SELF_HEAL_COOLDOWN_HOURS) {
            cfg.repair_cooldown = Duration::from_secs(h * 3600);
        }
        cfg
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepairCandidate {
    pub packet_id: String,
    pub domain: Option<String>,
    /// Machine-readable reason tags: `"high_no_match_rate"`, `"low_fidelity"`.
    pub reasons: Vec<String>,
    pub apply_count: usize,
    pub no_match_count: usize,
    pub no_match_rate: Option<f32>,
    pub fidelity: Option<f32>,
    pub last_audit_timestamp: Option<String>,
}

/// Subtract `dur` from `now` in ISO-8601 lexicographic space. We use
/// string comparison on ISO-8601 timestamps (already done in
/// `list_events`), so the cutoff is a string too. Returns the cutoff
/// timestamp. On clock parse failure, returns `None` — the caller should
/// fall back to "keep everything".
fn window_cutoff(now_iso: &str, dur: Duration) -> Option<String> {
    let now = chrono::DateTime::parse_from_rfc3339(now_iso).ok()?;
    let cutoff = now.checked_sub_signed(chrono::Duration::from_std(dur).ok()?)?;
    Some(cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Pure function. Given a newest-first slice of events and a config,
/// return the packets that qualify for repair this tick. Events older
/// than the window are ignored. Packets with a recent
/// `repair_candidate` event inside the cooldown are skipped.
pub fn find_repair_candidates(
    events: &[PacketEvent],
    config: &ScannerConfig,
    now_iso: &str,
) -> Vec<RepairCandidate> {
    let cutoff = window_cutoff(now_iso, config.window);
    let cooldown_cutoff = window_cutoff(now_iso, config.repair_cooldown);

    // Aggregate per packet_id.
    struct Agg {
        domain: Option<String>,
        applies: usize,
        no_matches: usize,
        latest_fidelity: Option<f32>,
        latest_audit_ts: Option<String>,
        latest_candidate_ts: Option<String>,
    }
    let mut agg: BTreeMap<String, Agg> = BTreeMap::new();
    for ev in events {
        let Some(pid) = ev.packet_id.as_ref() else {
            continue;
        };
        let in_window = match &cutoff {
            Some(c) => ev.timestamp.as_str() >= c.as_str(),
            None => true,
        };

        let slot = agg.entry(pid.clone()).or_insert_with(|| Agg {
            domain: ev.domain.clone(),
            applies: 0,
            no_matches: 0,
            latest_fidelity: None,
            latest_audit_ts: None,
            latest_candidate_ts: None,
        });
        if slot.domain.is_none() {
            slot.domain = ev.domain.clone();
        }

        // Cooldown tracking always looks at the latest candidate
        // regardless of window, so a stale-but-recent candidate still
        // suppresses re-flag.
        if ev.op == "repair_candidate" {
            if slot
                .latest_candidate_ts
                .as_deref()
                .is_none_or(|t| t < ev.timestamp.as_str())
            {
                slot.latest_candidate_ts = Some(ev.timestamp.clone());
            }
            continue;
        }

        if !in_window {
            continue;
        }

        match ev.op.as_str() {
            "apply" => {
                slot.applies += 1;
                if ev.outcome == "no_match" {
                    slot.no_matches += 1;
                }
            }
            "audit" => {
                // Latest audit wins (events are newest-first, so first
                // audit we see for this packet is the freshest).
                if slot.latest_audit_ts.is_none()
                    && let Some(f) = ev.details.get("fidelity").and_then(|v| v.as_f64())
                {
                    slot.latest_fidelity = Some(f as f32);
                    slot.latest_audit_ts = Some(ev.timestamp.clone());
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for (pid, a) in agg {
        // Skip packets still inside cooldown.
        if let (Some(ts), Some(cd)) = (&a.latest_candidate_ts, &cooldown_cutoff) {
            if ts.as_str() >= cd.as_str() {
                continue;
            }
        }

        let no_match_rate = if a.applies >= config.min_apply_samples {
            Some(a.no_matches as f32 / a.applies as f32)
        } else {
            None
        };

        let mut reasons: Vec<String> = Vec::new();
        if let Some(rate) = no_match_rate {
            if rate >= config.no_match_threshold {
                reasons.push("high_no_match_rate".to_string());
            }
        }
        if let Some(f) = a.latest_fidelity {
            if f < config.fidelity_threshold {
                reasons.push("low_fidelity".to_string());
            }
        }
        if reasons.is_empty() {
            continue;
        }

        out.push(RepairCandidate {
            packet_id: pid,
            domain: a.domain,
            reasons,
            apply_count: a.applies,
            no_match_count: a.no_matches,
            no_match_rate,
            fidelity: a.latest_fidelity,
            last_audit_timestamp: a.latest_audit_ts,
        });
    }
    out
}
