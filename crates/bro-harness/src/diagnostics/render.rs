//! Wave-2 PRESENTATION (Drone 2 owns this file).
//!
//! 1. [`classify`] — map a raw `lsp_types::Diagnostic` into the window-0
//!    [`Enveloped`] form. MVP: error severity → `class = Error,
//!    confidence = Confirmed`; everything else → `class = Lint`. `tier` is
//!    always `Instant`. Adding the candidate/deferred branches is a small
//!    change once flycheck/truth tiers come online.
//! 2. [`build_rider`] — render the compact block appended to a tool result
//!    from the spine's `&[DiffResult]`. Returns `None` when no diff reports a
//!    new diagnostic; otherwise leads with a counts summary and lists each
//!    new diagnostic with file + 1-based line + message, grouped by file,
//!    errors first.
//!
//! Scope-honesty: a clean run yields `None` (no rider, no false "all good"
//! claim). When the rider IS emitted, it lists the exact files and lines
//! diffed — pkg/features are not threaded into this signature because
//! `DiffResult` does not carry a [`Scope`]; a workspace-level "[pkg, features]"
//! tag would have to come from a separate channel.

use std::fmt::Write as _;

use super::{Confidence, DiagClass, DiffResult, Enveloped, Scope, Tier};
use lsp_types::{Diagnostic, DiagnosticSeverity};

/// Classify a raw LSP diagnostic into the window-0 envelope. MVP: error tier
/// only — warnings/hints are tagged `Lint` for now and graduate to
/// `Candidate`/`Deferred` once flycheck and truth tiers are wired in.
pub fn classify(diag: &Diagnostic, scope: &Scope) -> Enveloped {
    let class = match diag.severity {
        Some(DiagnosticSeverity::ERROR) => DiagClass::Error,
        _ => DiagClass::Lint,
    };
    let confidence = match class {
        DiagClass::Error => Confidence::Confirmed,
        DiagClass::Lint => Confidence::Confirmed,
    };
    Enveloped {
        diagnostic: diag.clone(),
        class,
        tier: Tier::Instant,
        confidence,
        scope: scope.clone(),
    }
}

/// Stable opening marker for the window-0 rider. Because the rider is appended
/// to a tool-result body, consumers that see that body (notably the `bro fleet`
/// TUI) split the diagnostics block off by finding this marker. WIRE CONTRACT:
/// keep in sync with the detector in `src/fleet_tui.rs`.
pub const RIDER_MARKER: &str = "window-0 diagnostics:";

/// Build the rider block, or `None` when nothing is new across `diffs`.
pub fn build_rider(diffs: &[DiffResult]) -> Option<String> {
    let total_new: usize = diffs.iter().map(|d| d.new.len()).sum();
    if total_new == 0 {
        return None;
    }
    let total_fixed: usize = diffs.iter().map(|d| d.fixed).sum();
    let total_carried: usize = diffs.iter().map(|d| d.carried).sum();
    let total_err: usize = diffs
        .iter()
        .flat_map(|d| d.new.iter())
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .count();

    let mut out = String::new();
    out.push_str(RIDER_MARKER);
    out.push(' ');
    let _ = write!(
        out,
        "{total_new} new ({total_err} error{es}), {total_fixed} fixed, {total_carried} carried",
        es = if total_err == 1 { "" } else { "s" }
    );

    for diff in diffs {
        if diff.new.is_empty() {
            continue;
        }
        let mut ordered: Vec<&Diagnostic> = diff.new.iter().collect();
        ordered.sort_by_key(|d| severity_rank(d.severity));
        let _ = write!(out, "\n  {}:", diff.file);
        for d in ordered {
            let line = d.range.start.line + 1;
            let msg = d.message.lines().next().unwrap_or("").trim();
            let _ = write!(out, "\n    {line}: {msg}");
        }
    }
    Some(out)
}

fn severity_rank(s: Option<DiagnosticSeverity>) -> u8 {
    match s {
        Some(DiagnosticSeverity::ERROR) => 0,
        Some(DiagnosticSeverity::WARNING) => 1,
        Some(DiagnosticSeverity::INFORMATION) => 2,
        Some(DiagnosticSeverity::HINT) => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn diag(line: u32, severity: DiagnosticSeverity, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity: Some(severity),
            code: None,
            code_description: None,
            source: Some("rust-analyzer".into()),
            message: msg.into(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn scope() -> Scope {
        Scope {
            pkg: "bro-harness".into(),
            features: "default".into(),
        }
    }

    #[test]
    fn build_rider_returns_none_on_empty_input() {
        assert!(build_rider(&[]).is_none());
    }

    #[test]
    fn build_rider_returns_none_when_no_diff_has_new() {
        let diffs = vec![
            DiffResult {
                file: "src/a.rs".into(),
                new: vec![],
                fixed: 3,
                carried: 7,
            },
            DiffResult {
                file: "src/b.rs".into(),
                new: vec![],
                fixed: 0,
                carried: 0,
            },
        ];
        assert!(build_rider(&diffs).is_none());
    }

    #[test]
    fn build_rider_lists_two_errors_with_file_line_message() {
        let diffs = vec![DiffResult {
            file: "src/lib.rs".into(),
            new: vec![
                diag(
                    11,
                    DiagnosticSeverity::ERROR,
                    "cannot find type `Bar` in this scope",
                ),
                diag(44, DiagnosticSeverity::ERROR, "mismatched types"),
            ],
            fixed: 0,
            carried: 0,
        }];
        let rider = build_rider(&diffs).expect("rider should be Some");
        assert!(rider.contains("src/lib.rs"), "{rider}");
        assert!(rider.contains("12:"), "1-based line for error 1: {rider}");
        assert!(rider.contains("cannot find type `Bar` in this scope"));
        assert!(rider.contains("45:"), "1-based line for error 2: {rider}");
        assert!(rider.contains("mismatched types"));
    }

    #[test]
    fn build_rider_orders_errors_before_warnings_within_file() {
        let diffs = vec![DiffResult {
            file: "src/lib.rs".into(),
            new: vec![
                diag(10, DiagnosticSeverity::WARNING, "unused variable `x`"),
                diag(20, DiagnosticSeverity::ERROR, "expected `;`"),
            ],
            fixed: 0,
            carried: 0,
        }];
        let rider = build_rider(&diffs).unwrap();
        let err_pos = rider.find("expected `;`").unwrap();
        let warn_pos = rider.find("unused variable `x`").unwrap();
        assert!(
            err_pos < warn_pos,
            "errors must precede warnings within a file: {rider}"
        );
    }

    #[test]
    fn build_rider_summary_surfaces_fixed_and_carried_counts() {
        let diffs = vec![
            DiffResult {
                file: "src/a.rs".into(),
                new: vec![diag(0, DiagnosticSeverity::ERROR, "boom")],
                fixed: 1,
                carried: 4,
            },
            DiffResult {
                file: "src/b.rs".into(),
                new: vec![diag(0, DiagnosticSeverity::WARNING, "meh")],
                fixed: 2,
                carried: 1,
            },
        ];
        let rider = build_rider(&diffs).unwrap();
        assert!(rider.contains("2 new"), "{rider}");
        assert!(rider.contains("1 error"), "{rider}");
        assert!(rider.contains("3 fixed"), "{rider}");
        assert!(rider.contains("5 carried"), "{rider}");
    }

    #[test]
    fn build_rider_skips_files_with_no_new_diagnostics() {
        let diffs = vec![
            DiffResult {
                file: "src/quiet.rs".into(),
                new: vec![],
                fixed: 0,
                carried: 12,
            },
            DiffResult {
                file: "src/loud.rs".into(),
                new: vec![diag(2, DiagnosticSeverity::ERROR, "boom")],
                fixed: 0,
                carried: 0,
            },
        ];
        let rider = build_rider(&diffs).unwrap();
        assert!(!rider.contains("src/quiet.rs"), "{rider}");
        assert!(rider.contains("src/loud.rs"), "{rider}");
    }

    #[test]
    fn classify_error_severity_is_error_instant_confirmed() {
        let d = diag(0, DiagnosticSeverity::ERROR, "boom");
        let env = classify(&d, &scope());
        assert_eq!(env.class, DiagClass::Error);
        assert_eq!(env.tier, Tier::Instant);
        assert_eq!(env.confidence, Confidence::Confirmed);
        assert_eq!(env.diagnostic.message, "boom");
        assert_eq!(env.scope.pkg, "bro-harness");
        assert_eq!(env.scope.features, "default");
    }

    #[test]
    fn classify_non_error_severity_is_lint() {
        for sev in [
            DiagnosticSeverity::WARNING,
            DiagnosticSeverity::INFORMATION,
            DiagnosticSeverity::HINT,
        ] {
            let d = diag(0, sev, "meh");
            let env = classify(&d, &scope());
            assert_eq!(env.class, DiagClass::Lint, "severity {sev:?}");
            assert_eq!(env.tier, Tier::Instant);
        }
    }
}
