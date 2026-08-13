//! The producer-side policy engine (design section 11).
//!
//! Every rule here is evaluated on ENUMERATION METADATA, before any fetch.
//! That ordering is the whole point: "policy exclusion prevents fetching, not
//! merely indexing" is an acceptance criterion, and a policy that filtered
//! after download would waste vendor quota and bandwidth while still shipping
//! the bytes across the trust boundary.
//!
//! Caps are enforced twice, here and on the server, because authentication
//! proves the configured producer, not the truth of its bytes. The satellite
//! enforces them to avoid wasting quota; the server enforces the same shared
//! policy because it cannot trust that this ran.
//!
//! Silence is not an option anywhere in this file. Every exclusion is counted
//! under a named reason, because a policy quietly dropping half a drive is
//! indistinguishable from a broken connector.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::connector::{RemoteEntry, RemoteEntryKind};

/// Skip reasons. Stable labels, because they are counted per reason and shown
/// to operators on publication status.
pub const REASON_EXCLUDED: &str = "excluded";
pub const REASON_NOT_INCLUDED: &str = "not_included";
pub const REASON_UNSUPPORTED: &str = "unsupported_format";
pub const REASON_OVERSIZE: &str = "oversize";
pub const REASON_NATIVE_EXPORT_DISABLED: &str = "native_export_disabled";
pub const REASON_EXPORT_OVERSIZE: &str = "export_oversize";
pub const REASON_DIRECTORY: &str = "directory";

/// Operator-declared policy for one connector source.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourcePolicy {
    /// When non-empty, an entry must match at least one pattern to be
    /// considered at all. Empty means "everything the other rules allow".
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Per-file ceiling, defaulting to the shared per-extension caps in
    /// `bbox-code-source` so nothing is fetched that the corpus would refuse
    /// to index. An explicit value LOWERS the ceiling; it can never raise it
    /// above what the corpus accepts.
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
    /// Export provider-native documents (Docs, Sheets, Slides) into formats
    /// the chunker registry already claims.
    #[serde(default = "default_native_export")]
    pub native_export: bool,
    /// Per-source total ceiling. Its breach ABORTS the publication loudly
    /// naming the largest offenders rather than truncating silently.
    #[serde(default)]
    pub max_total_bytes: Option<u64>,
}

fn default_native_export() -> bool {
    true
}

impl Default for SourcePolicy {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            max_file_bytes: None,
            native_export: true,
            max_total_bytes: None,
        }
    }
}

/// A compiled policy. Globs are compiled once per publication cycle rather
/// than per entry.
///
/// `Debug` is derived because every field is operator-declared policy -- globs
/// and byte caps -- and none of it is credential material, so rendering it in a
/// panic message or a tracing field leaks nothing. That is the same reasoning
/// the grant table applies in reverse: `ConnectorGrantRuntime` writes `Debug`
/// by hand precisely because it DOES retain bearers, and renders only a token
/// count. Here there is no secret to hide, so the derive is the honest choice
/// and it lets a failed `compile` be asserted on with `unwrap_err`.
#[derive(Debug)]
pub struct CompiledPolicy {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    max_file_bytes: Option<u64>,
    native_export: bool,
    max_total_bytes: Option<u64>,
}

/// What the policy decided about one enumerated entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Fetch it, bounded by this many bytes. The cap is returned rather than
    /// re-derived by the caller so exactly one place decides it.
    Fetch {
        max_bytes: u64,
    },
    /// Export it as this extension, bounded by this many bytes. Export size is
    /// unknown before fetching, so this cap is the one that fires mid-stream.
    Export {
        extension: String,
        max_bytes: u64,
    },
    Skip {
        reason: &'static str,
    },
}

impl CompiledPolicy {
    pub fn compile(policy: &SourcePolicy) -> Result<Self> {
        Ok(Self {
            include: compile_set(&policy.include).context("compiling include globs")?,
            exclude: compile_set(&policy.exclude).context("compiling exclude globs")?,
            max_file_bytes: policy.max_file_bytes,
            native_export: policy.native_export,
            max_total_bytes: policy.max_total_bytes,
        })
    }

    /// Decide one entry from its enumeration metadata alone.
    ///
    /// `logical_path` is the DERIVED path, not the raw remote name: globs are
    /// matched against the same string the manifest will carry and the corpus
    /// will index, so an operator's exclude pattern means what they read on a
    /// status page rather than what a vendor happened to name a folder.
    pub fn decide(&self, entry: &RemoteEntry, logical_path: &str) -> PolicyDecision {
        if entry.kind == RemoteEntryKind::Directory {
            return PolicyDecision::Skip {
                reason: REASON_DIRECTORY,
            };
        }
        if let Some(exclude) = &self.exclude
            && exclude.is_match(logical_path)
        {
            return PolicyDecision::Skip {
                reason: REASON_EXCLUDED,
            };
        }
        if let Some(include) = &self.include
            && !include.is_match(logical_path)
        {
            return PolicyDecision::Skip {
                reason: REASON_NOT_INCLUDED,
            };
        }
        if entry.kind == RemoteEntryKind::NativeDocument && !self.native_export {
            return PolicyDecision::Skip {
                reason: REASON_NATIVE_EXPORT_DISABLED,
            };
        }
        // The corpus ceiling for the path AS IT WILL BE INDEXED. A format the
        // chunker registry does not claim is refused here, so the bytes are
        // never fetched at all.
        let Some(corpus_cap) =
            bbox_code_source::max_bytes_for_path(std::path::Path::new(logical_path))
        else {
            return PolicyDecision::Skip {
                reason: REASON_UNSUPPORTED,
            };
        };
        // An operator cap LOWERS the ceiling and can never raise it above what
        // the corpus would accept: the server enforces the corpus cap anyway,
        // so a higher local value would only produce a rejected manifest.
        let cap = self
            .max_file_bytes
            .map_or(corpus_cap, |c| c.min(corpus_cap));
        if entry.kind == RemoteEntryKind::NativeDocument {
            let extension = export_extension(entry);
            return PolicyDecision::Export {
                extension: extension.to_string(),
                max_bytes: cap,
            };
        }
        // Size IS known for an ordinary file, so an oversized one is refused
        // before a byte moves.
        if let Some(size) = entry.size
            && size > cap
        {
            return PolicyDecision::Skip {
                reason: REASON_OVERSIZE,
            };
        }
        PolicyDecision::Fetch { max_bytes: cap }
    }

    pub fn max_total_bytes(&self) -> Option<u64> {
        self.max_total_bytes
    }

    /// Check the per-source total against the cap, ABORTING LOUDLY and naming
    /// the biggest offenders.
    ///
    /// Truncating to fit would publish a manifest that silently omitted the
    /// largest documents in the scope, and the operator's only signal would be
    /// a search that never finds them. Refusing with the offenders named is
    /// the whole value of this rule.
    pub fn check_total(&self, sized: &[(String, u64)]) -> Result<()> {
        let Some(cap) = self.max_total_bytes else {
            return Ok(());
        };
        let total = sized
            .iter()
            .try_fold(0_u64, |sum, (_, size)| sum.checked_add(*size))
            .context("connector source byte total overflowed")?;
        if total <= cap {
            return Ok(());
        }
        let mut ranked: Vec<&(String, u64)> = sized.iter().collect();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        let offenders = ranked
            .iter()
            .take(10)
            .map(|(path, size)| format!("{path} ({size} bytes)"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "connector source exceeds its {cap}-byte max_total_bytes with {total} bytes; \
             largest entries: {offenders}"
        );
    }
}

fn compile_set(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid glob {pattern:?}"))?);
    }
    Ok(Some(builder.build()?))
}

/// The export map (design section 7): provider-native kinds onto formats the
/// chunker registry ALREADY claims.
///
/// This static mapping is the entire extent of the satellite's knowledge of
/// the corpus. It does not sniff formats, model chunks, or know the registry;
/// it asks the vendor to render a document and receives ordinary bytes. That
/// is what keeps exactly one chunker version in the system, on the corpus
/// host, so a satellite deploy can never skew against the index.
pub fn export_extension(entry: &RemoteEntry) -> &'static str {
    match entry.media_type.as_deref() {
        Some("application/vnd.bbox-fixture.document") => "docx",
        Some("application/vnd.bbox-fixture.spreadsheet") => "xlsx",
        Some("application/vnd.bbox-fixture.presentation") => "pptx",
        Some("application/vnd.google-apps.document") => "docx",
        Some("application/vnd.google-apps.spreadsheet") => "xlsx",
        Some("application/vnd.google-apps.presentation") => "pptx",
        // Drawings and everything else a store calls native render to PDF,
        // which the registry also claims.
        _ => "pdf",
    }
}

/// Bounded per-reason counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkipCounters {
    counts: BTreeMap<String, u64>,
}

impl SkipCounters {
    pub fn record(&mut self, reason: &str) {
        *self.counts.entry(reason.to_string()).or_insert(0) += 1;
    }

    pub fn into_map(self) -> BTreeMap<String, u64> {
        self.counts
    }

    pub fn get(&self, reason: &str) -> u64 {
        self.counts.get(reason).copied().unwrap_or(0)
    }

    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(size: u64) -> RemoteEntry {
        RemoteEntry {
            remote_id: "remote-1".into(),
            name_path: vec!["notes.md".into()],
            kind: RemoteEntryKind::File,
            remote_version: "v1".into(),
            size: Some(size),
            media_type: Some("text/markdown".into()),
            remote_url: None,
            deleted: false,
        }
    }

    fn native(media_type: &str) -> RemoteEntry {
        RemoteEntry {
            remote_id: "remote-2".into(),
            name_path: vec!["Runbook".into()],
            kind: RemoteEntryKind::NativeDocument,
            remote_version: "v1".into(),
            size: None,
            media_type: Some(media_type.into()),
            remote_url: None,
            deleted: false,
        }
    }

    #[test]
    fn exclusion_is_decided_before_any_fetch() {
        let policy = CompiledPolicy::compile(&SourcePolicy {
            exclude: vec!["**/Archive/**".into()],
            ..SourcePolicy::default()
        })
        .unwrap();
        assert_eq!(
            policy.decide(&file(10), "Ops/Archive/old.md"),
            PolicyDecision::Skip {
                reason: REASON_EXCLUDED
            }
        );
        assert!(matches!(
            policy.decide(&file(10), "Ops/current.md"),
            PolicyDecision::Fetch { .. }
        ));
    }

    #[test]
    fn an_include_list_is_an_allowlist_when_present() {
        let policy = CompiledPolicy::compile(&SourcePolicy {
            include: vec!["**/*.md".into()],
            ..SourcePolicy::default()
        })
        .unwrap();
        assert!(matches!(
            policy.decide(&file(10), "Ops/current.md"),
            PolicyDecision::Fetch { .. }
        ));
        assert_eq!(
            policy.decide(&file(10), "Ops/current.txt"),
            PolicyDecision::Skip {
                reason: REASON_NOT_INCLUDED
            }
        );
        // An empty include list admits everything the other rules allow.
        let open = CompiledPolicy::compile(&SourcePolicy::default()).unwrap();
        assert!(matches!(
            open.decide(&file(10), "Ops/current.txt"),
            PolicyDecision::Fetch { .. }
        ));
    }

    #[test]
    fn caps_lower_but_never_raise_the_corpus_ceiling() {
        let strict = CompiledPolicy::compile(&SourcePolicy {
            max_file_bytes: Some(64),
            ..SourcePolicy::default()
        })
        .unwrap();
        assert_eq!(
            strict.decide(&file(65), "Ops/current.md"),
            PolicyDecision::Skip {
                reason: REASON_OVERSIZE
            }
        );
        assert_eq!(
            strict.decide(&file(64), "Ops/current.md"),
            PolicyDecision::Fetch { max_bytes: 64 }
        );

        // A local cap ABOVE the corpus ceiling cannot raise it: the server
        // enforces the corpus cap anyway, so a higher local value would only
        // ever produce a rejected manifest.
        let permissive = CompiledPolicy::compile(&SourcePolicy {
            max_file_bytes: Some(u64::MAX),
            ..SourcePolicy::default()
        })
        .unwrap();
        assert_eq!(
            permissive.decide(&file(1), "Ops/current.md"),
            PolicyDecision::Fetch {
                max_bytes: bbox_code_source::MAX_TEXT_FILE_BYTES
            }
        );
        assert_eq!(
            permissive.decide(&file(bbox_code_source::MAX_TEXT_FILE_BYTES + 1), "a.md"),
            PolicyDecision::Skip {
                reason: REASON_OVERSIZE
            }
        );
    }

    #[test]
    fn a_format_the_corpus_cannot_chunk_is_never_fetched() {
        let policy = CompiledPolicy::compile(&SourcePolicy::default()).unwrap();
        assert_eq!(
            policy.decide(&file(10), "Ops/archive.bin"),
            PolicyDecision::Skip {
                reason: REASON_UNSUPPORTED
            }
        );
    }

    #[test]
    fn native_documents_map_onto_formats_the_registry_already_claims() {
        let policy = CompiledPolicy::compile(&SourcePolicy::default()).unwrap();
        for (media_type, expected) in [
            ("application/vnd.bbox-fixture.document", "docx"),
            ("application/vnd.bbox-fixture.spreadsheet", "xlsx"),
            ("application/vnd.bbox-fixture.presentation", "pptx"),
            ("application/vnd.bbox-fixture.drawing", "pdf"),
        ] {
            let entry = native(media_type);
            // The decision is made against the EXPORTED path, which is what
            // the corpus will index.
            let logical = format!("Ops/Runbook.{expected}");
            match policy.decide(&entry, &logical) {
                PolicyDecision::Export { extension, .. } => assert_eq!(extension, expected),
                other => panic!("{media_type} must export, got {other:?}"),
            }
        }
    }

    #[test]
    fn native_export_can_be_switched_off_and_is_then_counted() {
        let policy = CompiledPolicy::compile(&SourcePolicy {
            native_export: false,
            ..SourcePolicy::default()
        })
        .unwrap();
        assert_eq!(
            policy.decide(
                &native("application/vnd.bbox-fixture.document"),
                "Ops/R.docx"
            ),
            PolicyDecision::Skip {
                reason: REASON_NATIVE_EXPORT_DISABLED
            }
        );
    }

    #[test]
    fn a_total_byte_breach_aborts_loudly_naming_the_biggest_offenders() {
        let policy = CompiledPolicy::compile(&SourcePolicy {
            max_total_bytes: Some(100),
            ..SourcePolicy::default()
        })
        .unwrap();
        policy
            .check_total(&[("a.md".into(), 40), ("b.md".into(), 60)])
            .expect("exactly at the cap is fine");

        let error = policy
            .check_total(&[
                ("small.md".into(), 5),
                ("huge.md".into(), 900),
                ("medium.md".into(), 100),
            ])
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("max_total_bytes"), "{rendered}");
        // Truncating to fit would publish a manifest silently missing the
        // largest documents, so the refusal must name them.
        assert!(rendered.contains("huge.md (900 bytes)"), "{rendered}");
        assert!(rendered.contains("medium.md (100 bytes)"), "{rendered}");
        let huge_at = rendered.find("huge.md").unwrap();
        let medium_at = rendered.find("medium.md").unwrap();
        assert!(huge_at < medium_at, "offenders are ranked largest first");
    }

    #[test]
    fn directories_are_skipped_without_a_fetch() {
        let policy = CompiledPolicy::compile(&SourcePolicy::default()).unwrap();
        let mut dir = file(0);
        dir.kind = RemoteEntryKind::Directory;
        assert_eq!(
            policy.decide(&dir, "Ops"),
            PolicyDecision::Skip {
                reason: REASON_DIRECTORY
            }
        );
    }

    #[test]
    fn an_invalid_glob_refuses_at_compile_rather_than_silently_matching_nothing() {
        let error = CompiledPolicy::compile(&SourcePolicy {
            exclude: vec!["[unterminated".into()],
            ..SourcePolicy::default()
        })
        .unwrap_err();
        assert!(error.to_string().contains("exclude"));
    }

    #[test]
    fn skip_counters_are_per_reason() {
        let mut counters = SkipCounters::default();
        counters.record(REASON_OVERSIZE);
        counters.record(REASON_OVERSIZE);
        counters.record(REASON_EXCLUDED);
        assert_eq!(counters.get(REASON_OVERSIZE), 2);
        assert_eq!(counters.get(REASON_EXCLUDED), 1);
        assert_eq!(counters.total(), 3);
    }
}
