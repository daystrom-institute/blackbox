use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// ── Storage ───────────────────────────────────────────────────────

pub(super) fn scope_dir(packets_dir: &Path, scope: &str) -> PathBuf {
    packets_dir.join(scope)
}

pub(super) fn packet_path(packets_dir: &Path, scope: &str, id: &str) -> PathBuf {
    scope_dir(packets_dir, scope).join(format!("{id}.json"))
}

pub(super) fn events_log_path(packets_dir: &Path) -> PathBuf {
    packets_dir.join("events.jsonl")
}

/// Atomic-ish append: opens the file in append mode and writes one
/// line terminated by `\n`. Small concurrent writes on Linux are
/// atomic below PIPE_BUF (~4KiB), which covers every plausible event
/// payload. No rotation in v1 — revisit if the log grows unbounded.
pub(super) fn append_line(path: &Path, line: &str) -> Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

// ── Event log ─────────────────────────────────────────────────────

/// One entry in the packet event log. Written as a JSON line on every
/// `compile`, `apply`, `audit`, or `gap` operation. The log is the
/// discovery-layer's observability surface: compile errors and
/// `bbox_packet_gap` entries tell us which primitives are missing in
/// practice; low-fidelity audits tell us which packets drifted; no-
/// match applies tell us where catchall rules are missing.
///
/// Deliberately small and un-versioned in v1 — rotate or migrate when
/// a real use case surfaces. Query via `bbox_packet_events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacketEvent {
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// `compile` | `apply` | `audit` | `gap`.
    pub op: String,
    /// `ok` | `error` | `no_match` | `low_fidelity` | `logged` (for gaps).
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Op-specific structured payload. Schema: compile → {rules_count,
    /// lattice_size, referenced_packets?}. apply → {mode, matched,
    /// rule_id?, verdict?}. audit → {mode, total, correct, fidelity,
    /// mismatch_count}. gap → {description, attempted_sketch?,
    /// fallback_used?, ast_feature_requested?}.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl PacketEvent {
    pub(super) fn now(op: &str, outcome: &str) -> Self {
        Self {
            timestamp: crate::util::now_iso(),
            op: op.to_string(),
            outcome: outcome.to_string(),
            packet_id: None,
            domain: None,
            details: serde_json::Value::Null,
        }
    }

    pub(super) fn with_packet_id(mut self, id: impl Into<String>) -> Self {
        self.packet_id = Some(id.into());
        self
    }

    pub(super) fn with_domain(mut self, d: impl Into<String>) -> Self {
        self.domain = Some(d.into());
        self
    }

    pub(super) fn with_details(mut self, d: serde_json::Value) -> Self {
        self.details = d;
        self
    }
}
