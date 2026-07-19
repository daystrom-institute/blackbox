//! The provenance ledger — host-side issuance records that make
//! `semantic_status` computable for adversarially careless cell code
//! (design/bro-harness/code-mode-cell-dsl.md §4).
//!
//! Threat model: not malice, *carelessness* — a cell that hand-builds a
//! changes array with `"lsp_verified"` written into it because the model
//! pattern-matched an example. Cell-supplied tags are therefore worthless;
//! the ledger records what the HOST produced and recognizes it again at
//! consumption. Laundering is possible and priced: hand-built edits apply
//! fine, they just floor at `syntax_only`.
//!
//! Recognition is **digest-based**: the cell idiom passes `r.changes`
//! through plain JS (`edits.merge({ es, changes: r.changes })` — often
//! filtered, spread, or JSON round-tripped), so an id-only envelope would
//! not survive idiomatic code. Each issued change is keyed by a canonical
//! content digest; consumption recomputes the digest and looks it up. The
//! cell-dsl §9 "ledger ergonomics" backstop is the primary mechanism here
//! by design — per-change keying means a filtered subset keeps its
//! provenance. Issuance ids still ride the producer's result envelope, but
//! as a debugging/correlation handle, not the recognition key.
//!
//! Lineage semantics (pressure-test §6 decision 4): the ledger tracks edit
//! PRODUCERS, not selectors. A span used to *aim* `lsp.rename` never enters
//! the lineage of the edits the server authors.

use std::collections::HashMap;
use std::sync::Mutex;

use bbox_refactor::sha256_hex;
use serde::Serialize;

use super::code_facts::Span;

/// Authority tier of an edit's producer. Ordering is the weakest-link rule:
/// the smallest tier across an EditSet is the set's `semantic_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AuthorityTier {
    /// Bytes a program manipulated without semantic authority — the floor,
    /// and the tier of every cell-authored edit.
    SyntaxOnly,
    /// Authored by the compiler (rustc/clippy) as a verbatim
    /// `MachineApplicable` `suggested_replacement`, recognized by the ledger
    /// at consumption. Stronger than a tree-sitter guess (the compiler
    /// asserts it compiles), weaker than a semantics-preserving server
    /// transformation (the compiler suggests what compiles, not what was
    /// meant). Recorded only for edits whose span AND replacement come
    /// verbatim from a compiler suggestion (design/refactor-tools/rust/
    /// rust-isolate-surface.md §8.1).
    CompilerSuggested,
    /// Authored by a language server (rename, code action) and recognized
    /// by the ledger at consumption.
    LspVerified,
}

impl AuthorityTier {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthorityTier::SyntaxOnly => "syntax_only",
            AuthorityTier::CompilerSuggested => "compiler_suggested",
            AuthorityTier::LspVerified => "lsp_verified",
        }
    }
}

/// Canonical serialization of one issued change, fixing field order so the
/// digest is stable regardless of how the JSON traveled through the isolate.
#[derive(Serialize)]
struct CanonicalChange<'a> {
    file: &'a str,
    byte_start: usize,
    byte_end: usize,
    content_sha256: &'a str,
    new_text: &'a str,
}

/// Digest of one `{span, new_text}` change — the ledger's recognition key.
pub fn change_digest(span: &Span, new_text: &str) -> String {
    let canonical = serde_json::to_string(&CanonicalChange {
        file: &span.file,
        byte_start: span.byte_start,
        byte_end: span.byte_end,
        content_sha256: &span.content_sha256,
        new_text,
    })
    .expect("canonical change serialization is infallible");
    sha256_hex(canonical.as_bytes())
}

#[derive(Debug, Clone)]
struct Entry {
    issuance: String,
    producer: &'static str,
    tier: AuthorityTier,
}

/// Session-scoped issuance ledger. One instance is shared across the
/// producing bindings (`lsp.*`) and the consuming algebra (`edits.merge`)
/// by [`super::binding_tools`]; like the EditStore it lives for the
/// session, so `store()`d changes recognized in a later cell keep their
/// tier (cell-dsl §4's cross-cell continuity, for free).
#[derive(Debug, Default)]
pub struct ProvenanceLedger {
    inner: Mutex<LedgerInner>,
}

#[derive(Debug, Default)]
struct LedgerInner {
    next_issuance: u64,
    by_digest: HashMap<String, Entry>,
}

impl ProvenanceLedger {
    /// Record a batch of host-produced changes under one issuance id.
    /// Returns the id for the producer's result envelope.
    pub fn record_changes<'a>(
        &self,
        producer: &'static str,
        tier: AuthorityTier,
        changes: impl IntoIterator<Item = (&'a Span, &'a str)>,
    ) -> String {
        let mut inner = self.inner.lock().expect("ledger poisoned");
        inner.next_issuance += 1;
        let issuance = format!("led-{}", inner.next_issuance);
        for (span, new_text) in changes {
            inner.by_digest.insert(
                change_digest(span, new_text),
                Entry {
                    issuance: issuance.clone(),
                    producer,
                    tier,
                },
            );
        }
        issuance
    }

    /// Recognize a consumed change: the tier its producer recorded, or
    /// `None` for unledgered material (which callers floor at
    /// [`AuthorityTier::SyntaxOnly`]).
    pub fn recognize(&self, span: &Span, new_text: &str) -> Option<AuthorityTier> {
        let inner = self.inner.lock().expect("ledger poisoned");
        inner
            .by_digest
            .get(&change_digest(span, new_text))
            .map(|e| e.tier)
    }

    /// The `(issuance id, producer)` a change was recorded under — a
    /// correlation/debugging surface, never the recognition key.
    pub fn issuance_of(&self, span: &Span, new_text: &str) -> Option<(String, &'static str)> {
        let inner = self.inner.lock().expect("ledger poisoned");
        inner
            .by_digest
            .get(&change_digest(span, new_text))
            .map(|e| (e.issuance.clone(), e.producer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(file: &str, start: usize, end: usize) -> Span {
        Span {
            file: file.to_string(),
            byte_start: start,
            byte_end: end,
            content_sha256: "abc123".to_string(),
        }
    }

    #[test]
    fn digest_is_stable_across_json_field_order() {
        // A change that round-tripped through the isolate with reordered
        // fields deserializes into the same Span → same digest.
        let reordered: Span = serde_json::from_str(
            r#"{ "content_sha256": "abc123", "byte_end": 9, "file": "a.rs", "byte_start": 2 }"#,
        )
        .unwrap();
        assert_eq!(
            change_digest(&span("a.rs", 2, 9), "x"),
            change_digest(&reordered, "x")
        );
    }

    #[test]
    fn digest_distinguishes_every_field() {
        let base = change_digest(&span("a.rs", 2, 9), "x");
        assert_ne!(base, change_digest(&span("b.rs", 2, 9), "x"));
        assert_ne!(base, change_digest(&span("a.rs", 3, 9), "x"));
        assert_ne!(base, change_digest(&span("a.rs", 2, 8), "x"));
        assert_ne!(base, change_digest(&span("a.rs", 2, 9), "y"));
        let mut drifted = span("a.rs", 2, 9);
        drifted.content_sha256 = "def456".to_string();
        assert_ne!(base, change_digest(&drifted, "x"));
    }

    #[test]
    fn recognize_hits_recorded_and_misses_hand_built() {
        let ledger = ProvenanceLedger::default();
        let s1 = span("a.rs", 0, 4);
        let s2 = span("a.rs", 10, 14);
        let id = ledger.record_changes(
            "lsp.rename",
            AuthorityTier::LspVerified,
            [(&s1, "new_name"), (&s2, "new_name")],
        );
        assert_eq!(id, "led-1");
        // A filtered subset keeps provenance: only s2 consumed.
        assert_eq!(
            ledger.recognize(&s2, "new_name"),
            Some(AuthorityTier::LspVerified)
        );
        assert_eq!(
            ledger.issuance_of(&s2, "new_name"),
            Some(("led-1".to_string(), "lsp.rename"))
        );
        // Hand-built material (same span, different text) is unrecognized.
        assert_eq!(ledger.recognize(&s2, "other"), None);
    }

    #[test]
    fn weakest_link_ordering() {
        assert!(AuthorityTier::SyntaxOnly < AuthorityTier::CompilerSuggested);
        assert!(AuthorityTier::CompilerSuggested < AuthorityTier::LspVerified);
        assert_eq!(
            AuthorityTier::LspVerified.min(AuthorityTier::SyntaxOnly),
            AuthorityTier::SyntaxOnly
        );
        assert_eq!(
            AuthorityTier::CompilerSuggested.min(AuthorityTier::LspVerified),
            AuthorityTier::CompilerSuggested
        );
    }

    #[test]
    fn as_str_round_trips_all_tiers() {
        assert_eq!(AuthorityTier::SyntaxOnly.as_str(), "syntax_only");
        assert_eq!(
            AuthorityTier::CompilerSuggested.as_str(),
            "compiler_suggested"
        );
        assert_eq!(AuthorityTier::LspVerified.as_str(), "lsp_verified");
    }

    #[test]
    fn issuance_ids_are_sequential_per_ledger() {
        let ledger = ProvenanceLedger::default();
        let s = span("a.rs", 0, 1);
        assert_eq!(
            ledger.record_changes("lsp.rename", AuthorityTier::LspVerified, [(&s, "a")]),
            "led-1"
        );
        assert_eq!(
            ledger.record_changes("lsp.rename", AuthorityTier::LspVerified, [(&s, "b")]),
            "led-2"
        );
    }
}
