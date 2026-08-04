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

/// Three-valued evaluation of a `cfg` predicate under `test = false`.
///
/// A span is test-only exactly when its predicate CANNOT hold with `test`
/// false. Anything else is reachable in a production build and must be
/// scanned, so the question is not "does `test` appear" but "does the
/// predicate require it". Treating any mention of `test` as a test gate
/// reversed the answer for negated and mixed predicates: `#[cfg(not(test))]`
/// is production-ONLY, and `#[cfg(any(not(unix), test))]` is production on
/// every non-unix target, yet both were being excluded.
///
/// Unknown is the conservative value: every atom other than `test` is
/// unknown here, and an unknown result scans as production. Over-scanning
/// fails loudly at the next baseline diff; under-scanning is the silent
/// direction an ownership gate must not take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Truth {
    False,
    True,
    Unknown,
}

impl Truth {
    fn not(self) -> Self {
        match self {
            Self::False => Self::True,
            Self::True => Self::False,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// Evaluate one `cfg` predicate with `test` bound to false.
fn eval_with_test_false(meta: &syn::Meta) -> Truth {
    match meta {
        syn::Meta::Path(path) => {
            if path.is_ident("test") {
                Truth::False
            } else {
                Truth::Unknown
            }
        }
        syn::Meta::NameValue(_) => Truth::Unknown,
        syn::Meta::List(list) => {
            let nested: Vec<syn::Meta> = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .map(|items| items.into_iter().collect())
                .unwrap_or_default();
            if list.path.is_ident("not") {
                return nested
                    .first()
                    .map(|inner| eval_with_test_false(inner).not())
                    .unwrap_or(Truth::Unknown);
            }
            if list.path.is_ident("all") {
                let mut result = Truth::True;
                for inner in &nested {
                    match eval_with_test_false(inner) {
                        Truth::False => return Truth::False,
                        Truth::Unknown => result = Truth::Unknown,
                        Truth::True => {}
                    }
                }
                return result;
            }
            if list.path.is_ident("any") {
                let mut result = Truth::False;
                for inner in &nested {
                    match eval_with_test_false(inner) {
                        Truth::True => return Truth::True,
                        Truth::Unknown => result = Truth::Unknown,
                        Truth::False => {}
                    }
                }
                return result;
            }
            Truth::Unknown
        }
    }
}

/// True when the attribute set REQUIRES `test`, so the span cannot exist in
/// a production build.
///
/// Multiple `cfg` attributes on one node conjoin, so any one of them
/// forcing the predicate false under `test = false` is enough.
fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let Ok(inner) = attr.parse_args::<syn::Meta>() else {
            return false;
        };
        eval_with_test_false(&inner) == Truth::False
    })
}

/// Byte ranges of every span a production build cannot reach.
///
/// This is a full `syn::visit::Visit` traversal rather than a hand-rolled
/// walk over the item forms someone remembered. The manual version handled
/// items, fields, variants and some impl members, and never descended into
/// FUNCTION BODIES, so a `#[cfg(test)]` statement inside a function was
/// scanned as production while its enclosing function was not. Both
/// directions of that are wrong: test instrumentation gets rejected as a
/// new production site, or a test-only row launders into the Phase 6
/// inventory. Visiting every attribute-bearing node closes the form list
/// by construction instead of by enumeration.
struct TestSpans {
    ranges: Vec<std::ops::Range<usize>>,
}

impl TestSpans {
    fn collect(file: &syn::File) -> Self {
        let mut spans = Self { ranges: Vec::new() };
        syn::visit::Visit::visit_file(&mut spans, file);
        spans
    }

    /// Record the span and stop descending: everything inside an excluded
    /// span is excluded with it.
    fn exclude(&mut self, span: proc_macro2::Span) {
        let range = span.byte_range();
        if !range.is_empty() {
            self.ranges.push(range);
        }
    }

    fn contains(&self, offset: usize) -> bool {
        self.ranges
            .iter()
            .any(|range| offset >= range.start && offset < range.end)
    }
}

/// Attributes carried by an expression, for the statement positions where
/// Rust accepts `cfg` on an expression.
fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
    macro_rules! arms {
        ($($variant:ident),* $(,)?) => {
            match expr {
                $(syn::Expr::$variant(inner) => &inner.attrs,)*
                _ => &[],
            }
        };
    }
    arms!(
        Array, Assign, Async, Await, Binary, Block, Break, Call, Cast, Closure, Const, Continue,
        Field, ForLoop, Group, If, Index, Infer, Let, Lit, Loop, Macro, Match, MethodCall, Paren,
        Path, Range, RawAddr, Reference, Repeat, Return, Struct, Try, TryBlock, Tuple, Unary,
        Unsafe, While, Yield,
    )
}

impl<'ast> syn::visit::Visit<'ast> for TestSpans {
    fn visit_item(&mut self, node: &'ast syn::Item) {
        if item_attrs(node).is_some_and(|attrs| is_cfg_test(attrs)) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_item(self, node);
    }

    fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
        let attrs: &[syn::Attribute] = match node {
            syn::ImplItem::Const(inner) => &inner.attrs,
            syn::ImplItem::Fn(inner) => &inner.attrs,
            syn::ImplItem::Type(inner) => &inner.attrs,
            syn::ImplItem::Macro(inner) => &inner.attrs,
            _ => &[],
        };
        if is_cfg_test(attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_impl_item(self, node);
    }

    fn visit_trait_item(&mut self, node: &'ast syn::TraitItem) {
        let attrs: &[syn::Attribute] = match node {
            syn::TraitItem::Const(inner) => &inner.attrs,
            syn::TraitItem::Fn(inner) => &inner.attrs,
            syn::TraitItem::Type(inner) => &inner.attrs,
            syn::TraitItem::Macro(inner) => &inner.attrs,
            _ => &[],
        };
        if is_cfg_test(attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_trait_item(self, node);
    }

    fn visit_foreign_item(&mut self, node: &'ast syn::ForeignItem) {
        let attrs: &[syn::Attribute] = match node {
            syn::ForeignItem::Fn(inner) => &inner.attrs,
            syn::ForeignItem::Static(inner) => &inner.attrs,
            syn::ForeignItem::Type(inner) => &inner.attrs,
            syn::ForeignItem::Macro(inner) => &inner.attrs,
            _ => &[],
        };
        if is_cfg_test(attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_foreign_item(self, node);
    }

    fn visit_field(&mut self, node: &'ast syn::Field) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_field(self, node);
    }

    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_variant(self, node);
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_arm(self, node);
    }

    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        let attrs: &[syn::Attribute] = match node {
            syn::Stmt::Local(local) => &local.attrs,
            syn::Stmt::Macro(mac) => &mac.attrs,
            // The expression arm is handled by `visit_expr` below, which
            // sees it at any depth rather than only as the statement's
            // outer expression.
            syn::Stmt::Expr(_, _) | syn::Stmt::Item(_) => &[],
        };
        if is_cfg_test(attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_stmt(self, node);
    }

    /// Every expression, at every depth.
    ///
    /// Checking attributes only on a statement's OUTER expression missed a
    /// gated block used as an array, tuple, call or tuple-struct operand.
    /// Hooking the expression node itself is the by-construction form of
    /// the same rule: enumerating operand contexts is the enumeration that
    /// keeps coming up one short.
    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        if is_cfg_test(expr_attrs(node)) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_expr(self, node);
    }

    /// Function and closure parameters, including `self`.
    fn visit_fn_arg(&mut self, node: &'ast syn::FnArg) {
        let attrs: &[syn::Attribute] = match node {
            syn::FnArg::Receiver(receiver) => &receiver.attrs,
            syn::FnArg::Typed(typed) => &typed.attrs,
        };
        if is_cfg_test(attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_fn_arg(self, node);
    }

    fn visit_receiver(&mut self, node: &'ast syn::Receiver) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_receiver(self, node);
    }

    /// Function-pointer parameters, e.g. `fn(#[cfg(test)] u8)`.
    fn visit_bare_fn_arg(&mut self, node: &'ast syn::BareFnArg) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_bare_fn_arg(self, node);
    }

    /// Struct-expression fields. `FieldValue` is the initializer side and a
    /// different node from the declaration-side `Field` above.
    fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_field_value(self, node);
    }

    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        if is_cfg_test(pat_attrs(node)) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_pat(self, node);
    }

    /// Variadic markers in `extern "C"` signatures, both the item and the
    /// function-pointer form.
    fn visit_variadic(&mut self, node: &'ast syn::Variadic) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_variadic(self, node);
    }

    fn visit_bare_variadic(&mut self, node: &'ast syn::BareVariadic) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_bare_variadic(self, node);
    }

    /// Struct-pattern fields, the destructuring counterpart to FieldValue.
    fn visit_field_pat(&mut self, node: &'ast syn::FieldPat) {
        if is_cfg_test(&node.attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_field_pat(self, node);
    }

    fn visit_generic_param(&mut self, node: &'ast syn::GenericParam) {
        let attrs: &[syn::Attribute] = match node {
            syn::GenericParam::Lifetime(param) => &param.attrs,
            syn::GenericParam::Type(param) => &param.attrs,
            syn::GenericParam::Const(param) => &param.attrs,
        };
        if is_cfg_test(attrs) {
            self.exclude(node.span());
            return;
        }
        syn::visit::visit_generic_param(self, node);
    }
}

/// Attributes carried by a pattern node.
fn pat_attrs(pat: &syn::Pat) -> &[syn::Attribute] {
    macro_rules! arms {
        ($($variant:ident),* $(,)?) => {
            match pat {
                $(syn::Pat::$variant(inner) => &inner.attrs,)*
                _ => &[],
            }
        };
    }
    arms!(
        Const,
        Ident,
        Lit,
        Macro,
        Or,
        Paren,
        Path,
        Range,
        Reference,
        Rest,
        Slice,
        Struct,
        Tuple,
        TupleStruct,
        Type,
        Wild,
    )
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

// ── Exhaustiveness of the attribute-bearing node set ─────────────────────
//
// The visitor claims to cover every node syn exposes attributes on. That
// claim was wrong four times over on inspection, so it is a machine check
// now rather than a comment.
//
// It is two-sided, and neither side alone is enough:
//
//   REMOVED or RENAMED nodes fail at COMPILE time. `expr_attrs` and
//   `pat_attrs` name every variant explicitly and each visitor override
//   names its node type, so a type syn drops or renames stops building.
//
//   ADDED nodes fail at TEST time, through the version pin below. syn's
//   `Expr` and `Pat` are `#[non_exhaustive]`, so an exhaustive match is
//   impossible and the wildcard arm cannot be removed; a new variant would
//   otherwise fall silently into `_ => &[]`. There is no runtime reflection
//   over a crate's type surface, so the tripwire is the version itself: the
//   covered inventory was audited against syn 2.0.117, and any other
//   version fails until someone re-audits and re-pins.
//
// The failure mode this replaces is the one that produced this round: a
// reviewer diffing the visitor against the AST by hand and finding four
// nodes short.

/// syn version the covered inventory below was audited against.
const AUDITED_SYN_VERSION: &str = "2.0.117";

/// Every node type in the audited syn version that carries `attrs`, each
/// with the hook that excludes it. Kept as data so the audit is reviewable
/// as a list rather than by reading the visitor.
const COVERED_ATTRIBUTE_NODES: &[(&str, &str)] = &[
    ("syn::Item", "visit_item"),
    ("syn::ImplItem", "visit_impl_item"),
    ("syn::TraitItem", "visit_trait_item"),
    ("syn::ForeignItem", "visit_foreign_item"),
    ("syn::Field", "visit_field"),
    ("syn::FieldValue", "visit_field_value"),
    ("syn::FieldPat", "visit_field_pat"),
    ("syn::Variant", "visit_variant"),
    ("syn::Arm", "visit_arm"),
    ("syn::Stmt", "visit_stmt"),
    ("syn::Expr", "visit_expr"),
    ("syn::FnArg", "visit_fn_arg"),
    ("syn::Receiver", "visit_receiver"),
    ("syn::BareFnArg", "visit_bare_fn_arg"),
    ("syn::Variadic", "visit_variadic"),
    ("syn::BareVariadic", "visit_bare_variadic"),
    ("syn::GenericParam", "visit_generic_param"),
    // syn::Pat is deliberately NOT listed. `visit_pat` exists as defensive
    // depth, but no source syn parses puts an attribute on a standalone
    // pattern: the reachable pattern attributes arrive through PatType
    // inside FnArg, which is its own row. An unexercisable row would be a
    // coverage claim no test could bind, which is the shape of claim this
    // inventory exists to retire.
    (
        "syn::File",
        "visit_file (inner attributes, whole-file gate)",
    ),
];

/// Fail when the audited syn version is not the one actually resolved.
///
/// This is the only mechanism that can catch a NEWLY ADDED attribute node,
/// because the non_exhaustive wildcard swallows unknown variants silently.
pub(crate) fn assert_covered_node_inventory(root: &Path) -> anyhow::Result<()> {
    let lock = std::fs::read_to_string(root.join("Cargo.lock"))?;
    let resolved = lock
        .split("[[package]]")
        .find(|block| block.contains("name = \"syn\""))
        .and_then(|block| {
            block
                .lines()
                .find_map(|line| line.trim().strip_prefix("version = "))
        })
        .map(|version| version.trim_matches('"').to_string())
        .ok_or_else(|| anyhow::anyhow!("syn not found in Cargo.lock"))?;
    anyhow::ensure!(
        resolved == AUDITED_SYN_VERSION,
        "the catalog ownership scanner covers the attribute-bearing nodes of syn \
         {AUDITED_SYN_VERSION}, but {resolved} is resolved. A newer syn may expose \
         attributes on nodes the visitor does not hook, and the non_exhaustive \
         wildcard would swallow them silently. Re-audit syn's attribute-bearing \
         node set against COVERED_ATTRIBUTE_NODES, add any missing visitor \
         override, then re-pin AUDITED_SYN_VERSION."
    );
    anyhow::ensure!(
        COVERED_ATTRIBUTE_NODES.len() == 18,
        "the covered node inventory changed without its audit count"
    );
    Ok(())
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    /// One synthetic source exercising one hook.
    ///
    /// `CANARY` marks an occurrence inside the `cfg(test)` span and must
    /// never survive; `PRODUCTION` marks one outside it and must, except
    /// for the whole-file gate where nothing survives.
    struct Case {
        node: &'static str,
        source: &'static str,
        production_survives: bool,
        /// Text that must not survive the scan. Usually the CANARY token,
        /// but a variadic has no name to carry one, so for those the
        /// excluded SPAN itself is the assertion.
        canary: &'static str,
    }

    const CASES: &[Case] = &[
        Case {
            node: "syn::Item",
            source: "#[cfg(test)]\nfn g() { CANARY; }\nfn p() { PRODUCTION; }\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::ImplItem",
            source: "struct S;\nimpl S {\n#[cfg(test)]\nfn g() { CANARY; }\nfn p() { PRODUCTION; }\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::TraitItem",
            source: "trait T {\n#[cfg(test)]\nfn g() { CANARY; }\nfn p() { PRODUCTION; }\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::ForeignItem",
            source: "extern \"C\" {\n#[cfg(test)]\nfn CANARY();\nfn PRODUCTION();\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::Field",
            source: "struct S {\n#[cfg(test)]\na: CANARY,\nb: PRODUCTION,\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::FieldValue",
            source: "fn f() -> S {\nS {\n#[cfg(test)]\na: CANARY,\nb: PRODUCTION,\n}\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::FieldPat",
            source: "fn f(s: S) {\nlet S {\n#[cfg(test)]\na: CANARY,\nb: PRODUCTION,\n} = s;\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::Variant",
            source: "enum E {\n#[cfg(test)]\nA(CANARY),\nB(PRODUCTION),\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::Arm",
            source: "fn f(x: u8) {\nmatch x {\n#[cfg(test)]\n0 => CANARY,\n_ => PRODUCTION,\n}\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::Stmt",
            source: "fn f() {\n#[cfg(test)]\nlet _ = CANARY;\nlet _ = PRODUCTION;\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        // The raw-address expression from the round-7 finding, in the
        // statement position where an expression attribute is stable.
        Case {
            node: "syn::Expr",
            source: "fn f() {\n#[cfg(test)]\n&raw const CANARY;\nPRODUCTION;\n}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::FnArg",
            source: "fn f(\n#[cfg(test)]\na: CANARY,\nb: PRODUCTION,\n) {}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::Receiver",
            source: "impl S {\nfn f(\n#[cfg(test)]\n&self,\n) {}\n}\nfn p() { PRODUCTION; }\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::BareFnArg",
            source: "type F = fn(\n#[cfg(test)]\nCANARY,\nPRODUCTION,\n);\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::Variadic",
            source: "extern \"C\" {\nfn f(\na: PRODUCTION,\n#[cfg(test)]\n...\n);\n}\n",
            production_survives: true,
            canary: "...",
        },
        Case {
            node: "syn::BareVariadic",
            source: "type F = unsafe extern \"C\" fn(\na: PRODUCTION,\n#[cfg(test)]\n...\n);\n",
            production_survives: true,
            canary: "...",
        },
        Case {
            node: "syn::GenericParam",
            source: "fn f<\n#[cfg(test)]\nCANARY,\nPRODUCTION,\n>() {}\n",
            production_survives: true,
            canary: "CANARY",
        },
        Case {
            node: "syn::File",
            source: "#![cfg(test)]\nfn p() { PRODUCTION; }\n",
            production_survives: false,
            canary: "CANARY",
        },
    ];

    fn scan_source(source: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("case.rs");
        std::fs::write(&path, source).unwrap();
        production_lines(&path)
            .unwrap_or_else(|error| panic!("parsing case source: {error}\n{source}"))
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    /// Every inventory row is exercised by a case, and every case is an
    /// inventory row.
    ///
    /// This is what binds the inventory to the hooks. Row count alone
    /// proved nothing: deleting a hook left the count intact and every
    /// gate green, which is the silent regression the inventory exists to
    /// prevent.
    #[test]
    fn every_covered_node_is_exercised_by_a_case() {
        let covered: std::collections::BTreeSet<&str> = COVERED_ATTRIBUTE_NODES
            .iter()
            .map(|(node, _)| *node)
            .collect();
        let exercised: std::collections::BTreeSet<&str> =
            CASES.iter().map(|case| case.node).collect();
        assert_eq!(
            covered, exercised,
            "every attribute-bearing node the scanner claims to cover must have a \
             case that fails when its hook is removed"
        );
    }

    /// Deleting any hook makes its case leak, which reds this.
    #[test]
    fn gated_spans_are_excluded_and_production_survives() {
        for case in CASES {
            let lines = scan_source(case.source);
            let joined = lines.join("\n");
            assert!(
                !joined.contains(case.canary),
                "{}: a cfg(test) span survived the scan\n{joined}",
                case.node
            );
            assert_eq!(
                joined.contains("PRODUCTION"),
                case.production_survives,
                "{}: production reachability is wrong\n{joined}",
                case.node
            );
        }
    }

    /// The round-7 finding end to end, through `scan` rather than through
    /// `production_lines`: the gated raw-address occurrence contributes no
    /// site, and the production occurrence after it is the only one.
    #[test]
    fn a_gated_raw_address_statement_reports_only_the_production_site() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw_addr.rs");
        std::fs::write(
            &path,
            "fn f(p: &P) -> usize {\n\
             #[cfg(test)]\n\
             (&raw const p.canonical_path);\n\
             p.checkout_project_dir.len()\n\
             }\n",
        )
        .unwrap();
        let patterns = vec![
            (
                "canonical_path_read".to_string(),
                regex::Regex::new(r"\.canonical_path").unwrap(),
            ),
            (
                "checkout_root_path".to_string(),
                regex::Regex::new(r"\.checkout_project_dir").unwrap(),
            ),
        ];
        let files = vec![path.to_string_lossy().into_owned()];
        let sites = scan(&files, &patterns).unwrap();
        let names: Vec<&str> = sites.keys().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, vec!["checkout_root_path"], "sites: {sites:?}");
    }
}
