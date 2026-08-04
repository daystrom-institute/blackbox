//! Syntax-aware source scanning for Clause 2 Proof B (plan section 14.2).
//!
//! The test exemption in section 14.2 is stated in terms of Rust items, and
//! three rounds of line-oriented text processing failed to model them: a
//! test module truncated the rest of the file, a comma-terminated test-only
//! field left the filter in a state it never left, a multiline braced enum
//! variant closed with `},` where an exact-indentation match was expected,
//! and a multiline test function had comma-terminated PARAMETERS that read
//! as the end of the item. Each fix bought one form and missed the next,
//! because delimiter nesting, string literals, member punctuation, and
//! multiline signatures are syntax, and a line filter cannot see syntax.
//!
//! So the exclusion is a parse now. `syn` gives real item boundaries and
//! real `cfg` attributes, and the enclosing item a site sits in comes from
//! the AST rather than from a regex guess at the nearest preceding header.
//!
//! Original source text is preserved rather than reprinted: the patterns
//! match source, and reprinting from tokens would change spacing and
//! punctuation in ways that quietly alter what matches. Spans give byte
//! ranges into the file that was read, so kept regions are sliced out of
//! the exact bytes on disk.

use std::collections::BTreeMap;
use std::path::Path;

use syn::spanned::Spanned;

/// One production line with the item that encloses it.
pub(crate) struct ProductionLine {
    pub(crate) item: String,
    pub(crate) text: String,
}

/// True when any attribute is a `cfg(test)` gate, including the inner
/// (`#![cfg(test)]`) form and `cfg(all(test, ...))` / `cfg(any(test, ...))`
/// nestings. Anything mentioning `test` inside a `cfg` is treated as a test
/// gate: over-excluding shrinks the inventory, which fails the check
/// loudly, whereas under-excluding is the silent direction.
fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let mut found = false;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("test") {
                found = true;
            }
            // Recurse through all(..) / any(..) / not(..).
            let _ = meta.parse_nested_meta(|inner| {
                if inner.path.is_ident("test") {
                    found = true;
                }
                let _ = inner.parse_nested_meta(|deep| {
                    if deep.path.is_ident("test") {
                        found = true;
                    }
                    Ok(())
                });
                Ok(())
            });
            Ok(())
        });
        found
    })
}

/// Byte ranges of every `cfg(test)`-gated item and member in one file.
///
/// Members matter as much as items: a test-only struct field or enum
/// variant is a span too, and the forms that broke the previous filters
/// were all members.
struct TestSpans {
    ranges: Vec<std::ops::Range<usize>>,
}

impl TestSpans {
    fn collect(file: &syn::File) -> Self {
        let mut spans = Self { ranges: Vec::new() };
        spans.walk_items(&file.items);
        spans
    }

    fn push(&mut self, span: proc_macro2::Span) {
        let range = span.byte_range();
        if !range.is_empty() {
            self.ranges.push(range);
        }
    }

    fn walk_items(&mut self, items: &[syn::Item]) {
        for item in items {
            if item_attrs(item).is_some_and(|attrs| is_cfg_test(attrs)) {
                self.push(item.span());
                continue;
            }
            match item {
                syn::Item::Mod(module) => {
                    if let Some((_, inner)) = &module.content {
                        self.walk_items(inner);
                    }
                }
                syn::Item::Struct(item) => self.walk_fields(&item.fields),
                syn::Item::Union(item) => {
                    self.walk_fields(&syn::Fields::Named(item.fields.clone()))
                }
                syn::Item::Enum(item) => {
                    for variant in &item.variants {
                        if is_cfg_test(&variant.attrs) {
                            self.push(variant.span());
                        } else {
                            self.walk_fields(&variant.fields);
                        }
                    }
                }
                syn::Item::Impl(item) => {
                    for sub in &item.items {
                        let gated = match sub {
                            syn::ImplItem::Fn(f) => is_cfg_test(&f.attrs),
                            syn::ImplItem::Const(c) => is_cfg_test(&c.attrs),
                            syn::ImplItem::Type(t) => is_cfg_test(&t.attrs),
                            _ => false,
                        };
                        if gated {
                            self.push(sub.span());
                        }
                    }
                }
                syn::Item::Trait(item) => {
                    for sub in &item.items {
                        let gated = match sub {
                            syn::TraitItem::Fn(f) => is_cfg_test(&f.attrs),
                            syn::TraitItem::Const(c) => is_cfg_test(&c.attrs),
                            syn::TraitItem::Type(t) => is_cfg_test(&t.attrs),
                            _ => false,
                        };
                        if gated {
                            self.push(sub.span());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_fields(&mut self, fields: &syn::Fields) {
        for field in fields {
            if is_cfg_test(&field.attrs) {
                self.push(field.span());
            }
        }
    }

    fn contains(&self, offset: usize) -> bool {
        self.ranges
            .iter()
            .any(|range| offset >= range.start && offset < range.end)
    }
}

fn item_attrs(item: &syn::Item) -> Option<&Vec<syn::Attribute>> {
    Some(match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => return None,
    })
}

/// The enclosing item label for each byte offset, as the baseline keys it.
///
/// These are real AST items, so `pub(crate) fn x` keys as `fn x` without
/// the truncation guesswork the text scanner needed, and a site inside an
/// impl method keys to that method rather than to the impl header.
fn item_labels(file: &syn::File) -> Vec<(std::ops::Range<usize>, String)> {
    fn label_items(items: &[syn::Item], out: &mut Vec<(std::ops::Range<usize>, String)>) {
        for item in items {
            let range = item.span().byte_range();
            match item {
                syn::Item::Mod(module) => {
                    if let Some((_, inner)) = &module.content {
                        label_items(inner, out);
                    }
                    out.push((range, format!("mod {}", module.ident)));
                }
                syn::Item::Fn(f) => out.push((range, format!("fn {}", f.sig.ident))),
                syn::Item::Struct(s) => out.push((range, format!("struct {}", s.ident))),
                syn::Item::Enum(e) => out.push((range, format!("enum {}", e.ident))),
                syn::Item::Trait(t) => out.push((range, format!("trait {}", t.ident))),
                syn::Item::Union(u) => out.push((range, format!("union {}", u.ident))),
                syn::Item::Type(t) => out.push((range, format!("type {}", t.ident))),
                syn::Item::Const(c) => out.push((range, format!("const {}", c.ident))),
                syn::Item::Static(s) => out.push((range, format!("static {}", s.ident))),
                syn::Item::Impl(imp) => {
                    for sub in &imp.items {
                        if let syn::ImplItem::Fn(f) = sub {
                            out.push((sub.span().byte_range(), format!("fn {}", f.sig.ident)));
                        }
                    }
                    let target = imp
                        .self_ty
                        .as_ref()
                        .span()
                        .source_text()
                        .unwrap_or_else(|| "?".into());
                    out.push((range, format!("impl {target}")));
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    label_items(&file.items, &mut out);
    // Innermost wins: sort by span width so the narrowest enclosing item is
    // found first when an offset falls inside several.
    out.sort_by_key(|(range, _)| range.end - range.start);
    out
}

/// Production lines of one file, with test-gated items and members removed.
///
/// A parse failure is an error rather than a silent skip: a file this proof
/// cannot read is a file it cannot vouch for.
pub(crate) fn production_lines(path: &Path) -> anyhow::Result<Vec<ProductionLine>> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
    let file = syn::parse_file(&source)
        .map_err(|error| anyhow::anyhow!("parsing {}: {error}", path.display()))?;
    // An inner `#![cfg(test)]` makes the whole file test-only.
    if is_cfg_test(&file.attrs) {
        return Ok(Vec::new());
    }
    let spans = TestSpans::collect(&file);
    let labels = item_labels(&file);

    let mut lines = Vec::new();
    let mut offset = 0usize;
    for text in source.split_inclusive('\n') {
        let trimmed = text.trim();
        let keep = !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && !trimmed.starts_with('*')
            && !spans.contains(offset);
        if keep {
            let item = labels
                .iter()
                .find(|(range, _)| offset >= range.start && offset < range.end)
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| "<file scope>".to_string());
            lines.push(ProductionLine {
                item,
                text: text.trim_end().to_string(),
            });
        }
        offset += text.len();
    }
    Ok(lines)
}

/// Per-site counts keyed by (file, enclosing item) for one pattern.
pub(crate) fn scan(
    files: &[String],
    patterns: &[(String, regex::Regex)],
) -> anyhow::Result<BTreeMap<(String, String, String), usize>> {
    let mut sites = BTreeMap::new();
    for file in files {
        let lines = production_lines(Path::new(file))?;
        for line in &lines {
            for (name, pattern) in patterns {
                if pattern.is_match(&line.text) {
                    *sites
                        .entry((name.clone(), file.clone(), line.item.clone()))
                        .or_insert(0usize) += 1;
                }
            }
        }
    }
    Ok(sites)
}

// ── Tracked patterns ────────────────────────────────────────────────────
// Each is a surface the catalog authority replaced, or a way to reach a
// checkout without a lease. The second field is the Phase 6 disposition a
// baseline row inherits when it is first written.
const PATTERNS: &[(&str, &str, &str)] = &[
    (
        "project_record_import",
        r"use .*project_record::\{?[^;]*ProjectRecord",
        "delete with the v1 record type",
    ),
    (
        "canonical_path_read",
        r"\.canonical_path",
        "delete with ProjectRecord",
    ),
    (
        "legacy_publisher",
        r"PublisherRefStore|PublisherAuthorizationCache",
        "delete the legacy publisher store outright",
    ),
    (
        "watcher_selected_carrier",
        r"ArtifactWatchCarrier::selected",
        "delete; catalog watches by attachment id",
    ),
    (
        "repo_io_selected_target",
        r"RepoCarrierTarget::(Selected|Checkout)",
        "delete Selected/Checkout; Attachment remains",
    ),
    (
        // Every DURABLE checkout-path carrier, not only the ones a first
        // pass noticed: `checkout_project_dir` is the catalog's primary
        // carrier, and omitting it left a filesystem open rooted there
        // invisible here AND allowed by clippy in the modules where
        // blocking I/O is deliberately sanctioned.
        "checkout_root_path",
        r"\.checkout_dir|\.checkout_project_dir|\.checkout_root\(\)|\.project_root\(\)",
        "collapse into the lease confined readers",
    ),
    (
        "direct_git_process",
        r#"Command::new\("git"\)"#,
        "route through the verified-commit git wrapper",
    ),
];

const BASELINE: &str = "scripts/catalog-ownership-baseline.txt";

/// Catalog runtime scope: the daemon's own source plus every bbox-* crate
/// the daemon links, DERIVED from the root manifest so it cannot silently
/// stop covering a crate someone adds. The bro-* crates are out by the
/// process boundary: the harness does not link the daemon and reaches no
/// catalog authority.
fn catalog_sources(root: &Path) -> anyhow::Result<Vec<String>> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let crate_dep =
        regex::Regex::new(r#"(?m)^(bbox-[a-z0-9-]+) = \{ path = "crates/(bbox-[a-z0-9-]+)""#)?;
    let mut globs = vec!["src".to_string()];
    let mut seen = std::collections::BTreeSet::new();
    for capture in crate_dep.captures_iter(&manifest) {
        if seen.insert(capture[2].to_string()) {
            globs.push(format!("crates/{}/src", &capture[2]));
        }
    }
    let mut files = Vec::new();
    for dir in globs {
        collect_rs(&root.join(&dir), root, &mut files)?;
    }
    files.sort();
    // A `#[cfg(test)] mod x;` declaration makes the WHOLE of x.rs test-only,
    // and the file itself carries no attribute saying so. Without this the
    // scanner reads such a file as runtime code: it caught its own pattern
    // definitions that way, which is the same class of mistake as scanning a
    // test module, just one indirection out.
    let test_only = test_only_modules(root, &files)?;
    files.retain(|file| !test_only.contains(file));
    Ok(files)
}

/// Files reachable only under `cfg(test)`, via a gated `mod x;` declaration.
fn test_only_modules(
    root: &Path,
    files: &[String],
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let mut gated = std::collections::BTreeSet::new();
    for file in files {
        let source = std::fs::read_to_string(root.join(file))?;
        let Ok(parsed) = syn::parse_file(&source) else {
            continue;
        };
        for item in &parsed.items {
            let syn::Item::Mod(module) = item else {
                continue;
            };
            if module.content.is_some() || !is_cfg_test(&module.attrs) {
                continue;
            }
            let parent = Path::new(file).parent().unwrap_or(Path::new(""));
            let name = module.ident.to_string();
            for candidate in [
                parent.join(format!("{name}.rs")),
                parent.join(&name).join("mod.rs"),
            ] {
                let candidate = candidate.to_string_lossy().replace('\\', "/");
                if files.contains(&candidate) {
                    gated.insert(candidate);
                }
            }
        }
    }
    Ok(gated)
}

fn collect_rs(dir: &Path, root: &Path, out: &mut Vec<String>) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rs(&path, root, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BaselineRow {
    count: usize,
    reason: String,
}

fn load_baseline(root: &Path) -> anyhow::Result<BTreeMap<(String, String, String), BaselineRow>> {
    let text = std::fs::read_to_string(root.join(BASELINE))?;
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        anyhow::ensure!(fields.len() == 5, "malformed baseline row: {line}");
        rows.insert(
            (
                fields[0].to_string(),
                fields[1].to_string(),
                fields[2].to_string(),
            ),
            BaselineRow {
                count: fields[3].parse()?,
                reason: fields[4].to_string(),
            },
        );
    }
    Ok(rows)
}

/// Absolute invariants: these must be ZERO or exact, because nothing
/// legitimate produces them even during the bridge window.
fn absolute_invariants(root: &Path, failures: &mut Vec<String>) -> anyhow::Result<()> {
    // The lower tool-edge carrier must not name ProjectRecord in runtime code.
    let tool_edges = root.join("crates/bbox-corpus-index/src/index/tool_edges.rs");
    for line in production_lines(&tool_edges)? {
        if line.text.contains("ProjectRecord") {
            failures.push(format!(
                "lower tool-edge carrier reintroduced ProjectRecord: {}",
                line.text.trim()
            ));
        }
    }

    // Plan 4.13 forbids new BuiltFromStamp variants.
    let built_from =
        std::fs::read_to_string(root.join("crates/bbox-corpus-core/src/built_from.rs"))?;
    let variants = regex::Regex::new(r"(?m)^\s{4}(Published|CheckoutOverlay)\b")?
        .find_iter(&built_from)
        .count();
    if variants != 2 {
        failures.push(format!(
            "BuiltFromStamp variant set changed: expected 2, found {variants}"
        ));
    }

    // Plan 14.2 and 4.17: no project or attachment fields in checkout
    // observations. The durable key space is pinned field by field.
    let access = std::fs::read_to_string(root.join("crates/bbox-indexing/src/checkout_access.rs"))?;
    for (marker, expected) in [
        (
            "struct CheckoutAccessCounter",
            "kind source_lane outcome count last_sequence last_unix_secs",
        ),
        (
            "struct CheckoutAccessObservationSnapshot",
            "version sequence counters",
        ),
    ] {
        let actual = struct_fields(&access, marker);
        if actual != expected {
            failures.push(format!(
                "checkout observation schema changed for {marker}\n  expected: {expected}\n  actual:   {actual}"
            ));
        }
    }
    Ok(())
}

fn struct_fields(source: &str, marker: &str) -> String {
    let mut fields = Vec::new();
    let mut inside = false;
    for line in source.lines() {
        if line.contains(marker) {
            inside = true;
            continue;
        }
        if inside {
            if line.starts_with('}') {
                break;
            }
            if let Some(rest) = line.strip_prefix("    ") {
                let rest = rest.strip_prefix("pub ").unwrap_or(rest);
                if let Some((name, _)) = rest.split_once(':') {
                    if !name.contains(' ') && !name.starts_with('#') {
                        fields.push(name.to_string());
                    }
                }
            }
        }
    }
    fields.join(" ")
}

pub(crate) struct Report {
    pub(crate) ok: bool,
    pub(crate) rendered: String,
}

/// Run the inventory. With `write_baseline`, the current inventory replaces
/// the baseline, preserving each existing row's Phase 6 reason by key.
pub(crate) fn run(root: &Path, write_baseline: bool) -> anyhow::Result<Report> {
    let patterns: Vec<(String, regex::Regex)> = PATTERNS
        .iter()
        .map(|(name, pattern, _)| Ok((name.to_string(), regex::Regex::new(pattern)?)))
        .collect::<anyhow::Result<_>>()?;
    let files = catalog_sources(root)?;
    let sites = scan(&files, &patterns)?;

    if write_baseline {
        let existing = load_baseline(root).unwrap_or_default();
        let mut out = String::from(
            "# Catalog ownership inventory baseline (plan section 14.2 Proof B).\n\
             # One row per SITE: pattern, file, enclosing item, count, Phase 6 reason.\n\
             # Spans are computed by parsing each file, so test items and members are\n\
             # excluded structurally rather than by matching text. A site absent here\n\
             # fails the check even when the total is unchanged, which is what rejects\n\
             # substituting a prohibited occurrence for an approved one. Every row is\n\
             # Phase 6 deletion inventory; see\n\
             # design/daemon-runtime/durable-project-catalog-phase6-handoff.md.\n\
             # Refresh after a legitimate removal:\n\
             #   scripts/acceptance-catalog-ownership.sh --write-baseline\n",
        );
        for (key, count) in &sites {
            let reason = existing
                .get(key)
                .map(|row| row.reason.clone())
                .unwrap_or_else(|| {
                    PATTERNS
                        .iter()
                        .find(|(name, _, _)| *name == key.0)
                        .map(|(_, _, reason)| reason.to_string())
                        .unwrap_or_default()
                });
            let reason = if reason.is_empty() {
                "NEEDS-REASON".to_string()
            } else {
                reason
            };
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                key.0, key.1, key.2, count, reason
            ));
        }
        std::fs::write(root.join(BASELINE), out)?;
        return Ok(Report {
            ok: true,
            rendered: format!("baseline written: {} sites", sites.len()),
        });
    }

    let mut failures = Vec::new();
    absolute_invariants(root, &mut failures)?;
    let baseline = load_baseline(root)?;

    for (key, count) in &sites {
        match baseline.get(key) {
            None => failures.push(format!("NEW site {} in {} :: {}", key.0, key.1, key.2)),
            Some(row) if *count > row.count => failures.push(format!(
                "{} in {} :: {} grew from {} to {count}",
                key.0, key.1, key.2, row.count
            )),
            Some(row) if *count < row.count => failures.push(format!(
                "{} in {} :: {} shrank from {} to {count}; refresh the baseline",
                key.0, key.1, key.2, row.count
            )),
            Some(_) => {}
        }
    }
    for (key, row) in &baseline {
        if row.reason.is_empty() || row.reason == "NEEDS-REASON" {
            failures.push(format!(
                "{} in {} :: {} has no Phase 6 reason",
                key.0, key.1, key.2
            ));
        }
        if !sites.contains_key(key) {
            failures.push(format!(
                "{} in {} :: {} is gone; refresh the baseline",
                key.0, key.1, key.2
            ));
        }
    }

    let ok = failures.is_empty();
    let rendered = if ok {
        format!(
            "acceptance-catalog-ownership: ok ({} sites, 2 BuiltFromStamp variants, observation schema pinned)",
            baseline.len()
        )
    } else {
        format!(
            "acceptance-catalog-ownership: {} failure(s)\n{}\n\nCatalog runtime paths must reach a checkout only through a capability \
             lease. A NEW or GROWN site means a converted surface gained another way \
             in. A GONE or SHRUNK site is good news that needs the baseline refreshed.",
            failures.len(),
            failures
                .iter()
                .map(|failure| format!("  {failure}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    Ok(Report { ok, rendered })
}
