use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
use tree_sitter::Node;
use tree_sitter_language_pack::{
    ProcessConfig, ProcessResult, StructureItem, StructureKind, process,
};

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

pub struct CodeChunker;

/// One extracted source symbol with enough context for both refactor
/// inventory (qualified + bare names) and code-nav indexing (raw
/// tree-sitter node kind + nearest enclosing symbol kind).
///
/// `kind` is the raw tree-sitter node kind from the AST walk
/// (e.g. `"function_item"`, `"impl_item"`, `"class_declaration"`,
/// `"call"` for Elixir defmodule/def/defp/defmacro). Per the design
/// in `design/archive/code-nav-symbolic-exploration.md`, this must
/// stay raw — any synthesised vocabulary (`impl_method`, etc.) is
/// derived at read time by `refactor_kind_for(language, kind,
/// parent_kind)`. The structure-pack fallback path cannot read
/// tree-sitter kinds and emits the language-pack `StructureKind`
/// label instead (a best-effort approximation; see
/// `structure_kind_label`).
///
/// `parent_kind` is the kind of the **nearest enclosing
/// symbol-producing ancestor** — i.e. the kind of the most recent
/// `SymbolSpec` pushed onto the walk stack. It is **not**
/// `node.parent().kind()`, which would frequently be a wrapper like
/// `declaration_list` / `block` and carry no synthesis signal.
/// `None` at file top level.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolSpec {
    qualified_name: String,
    bare_name: String,
    kind: String,
    parent_kind: Option<String>,
    byte_start: usize,
    byte_end: usize,
}

/// One entry on the walk stack: containing-symbol display name +
/// containing-symbol kind. Display name feeds `qualify`; kind feeds
/// `SymbolSpec.parent_kind`. Co-located in one struct so the two
/// stacks can never desync.
#[derive(Debug, Clone)]
struct SymbolStackFrame {
    display_name: String,
    kind: String,
}

impl SourceFormatChunker for CodeChunker {
    fn format_id(&self) -> &str {
        "code"
    }

    fn claims(&self, path: &Path, _sniff: &[u8]) -> bool {
        language_for_path(path).is_some_and(|language| parser_for_language(language).is_ok())
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        let language = language_for_path(path).context("unsupported code extension")?;
        let source = std::str::from_utf8(bytes)
            .with_context(|| format!("{} is not valid utf-8 code", path.display()))?;
        let config = ProcessConfig::new(language)
            .with_chunking(super::MAX_CHUNK_BYTES)
            .all();
        let processed = process(source, &config).unwrap_or_else(|err| {
            log_language_pack_failure(language, &err);
            ProcessResult::default()
        });

        let mut parser = parser_for_language(language)
            .with_context(|| format!("tree-sitter parser unavailable for {language}"))?;
        let tree = parser
            .parse(source, None)
            .context("tree-sitter parser returned no tree")?;

        let mut specs = ast_symbol_specs(tree.root_node(), source, language);
        if specs.is_empty() {
            specs = structure_symbol_specs(&processed, source);
        }

        let chunks = if specs.is_empty() {
            chunk_from_language_pack(path, language, &processed, source)
        } else {
            chunks_from_symbols(path, language, source, specs)
        };
        Ok((chunks, Vec::new()))
    }
}

fn log_language_pack_failure(language: &'static str, err: &dyn std::fmt::Display) {
    static WARNED_LANGUAGES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let warned = WARNED_LANGUAGES.get_or_init(|| Mutex::new(HashSet::new()));
    let first_failure = warned
        .lock()
        .map(|mut languages| languages.insert(language))
        .unwrap_or(false);
    if first_failure {
        tracing::warn!(
            language,
            error = %err,
            "tree-sitter-language-pack process unavailable; using direct grammar fallback"
        );
    } else {
        tracing::debug!(
            language,
            error = %err,
            "tree-sitter-language-pack process unavailable; using direct grammar fallback"
        );
    }
}

pub(crate) fn ts_language_for_name(language: &str) -> Result<tree_sitter::Language> {
    if let Ok(language) = tree_sitter_language_pack::get_language(language) {
        return Ok(language);
    }
    match language {
        "rust" => Ok(tree_sitter_rust::LANGUAGE.into()),
        "python" => Ok(tree_sitter_python::LANGUAGE.into()),
        "csharp" => Ok(tree_sitter_c_sharp::LANGUAGE.into()),
        "java" => Ok(tree_sitter_java::LANGUAGE.into()),
        "go" => Ok(tree_sitter_go::LANGUAGE.into()),
        "typescript" => Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => Ok(tree_sitter_javascript::LANGUAGE.into()),
        "c" => Ok(tree_sitter_c::LANGUAGE.into()),
        "cpp" => Ok(tree_sitter_cpp::LANGUAGE.into()),
        _ => Err(anyhow!("unsupported language {language}")),
    }
}

pub(crate) fn parser_for_language(language: &str) -> Result<tree_sitter::Parser> {
    let ts_language = ts_language_for_name(language)?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&ts_language)
        .map_err(|err| anyhow!("failed to set {language} parser: {err}"))?;
    Ok(parser)
}

pub fn language_for_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => Some("rust"),
        Some("py") => Some("python"),
        Some("cs") => Some("csharp"),
        Some("java") => Some("java"),
        Some("go") => Some("go"),
        Some("ts" | "tsx") => Some("typescript"),
        Some("js" | "jsx" | "mjs" | "cjs") => Some("javascript"),
        Some("c" | "h") => Some("c"),
        Some("cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx") => Some("cpp"),
        Some("erl" | "hrl") => Some("erlang"),
        Some("ex" | "exs") => Some("elixir"),
        Some("rb") => Some("ruby"),
        Some("ml" | "mli") => Some("ocaml"),
        Some("hs") => Some("haskell"),
        Some("swift") => Some("swift"),
        Some("kt") => Some("kotlin"),
        Some("scala") => Some("scala"),
        Some("lua") => Some("lua"),
        Some("sh" | "bash") => Some("bash"),
        Some("json") => Some("json"),
        Some("yaml" | "yml") => Some("yaml"),
        Some("toml") => Some("toml"),
        Some("html" | "htm") => Some("html"),
        Some("css") => Some("css"),
        Some("sql") => Some("sql"),
        _ => None,
    }
}

fn chunks_from_symbols(
    path: &Path,
    language: &str,
    source: &str,
    specs: Vec<SymbolSpec>,
) -> Vec<Chunk> {
    let mut seen = HashSet::new();
    let mut specs: Vec<_> = specs
        .into_iter()
        .filter(|spec| spec.byte_start < spec.byte_end && spec.byte_end <= source.len())
        .filter(|spec| seen.insert((spec.qualified_name.clone(), spec.byte_start, spec.byte_end)))
        .collect();
    specs.sort_by_key(|spec| (spec.byte_start, spec.byte_end));
    specs
        .into_iter()
        .enumerate()
        .map(|(idx, spec)| {
            let mut chunk = placeholder_chunk(
                path,
                "code_block",
                Some(language),
                source[spec.byte_start..spec.byte_end].to_string(),
                spec.byte_start as u64,
                spec.byte_end as u64,
                idx as u32,
            );
            chunk.symbol = Some(spec.qualified_name);
            chunk.symbol_exact = Some(spec.bare_name);
            chunk.symbol_kind = Some(spec.kind);
            chunk.parent_kind = spec.parent_kind;
            chunk.line_start = Some(byte_to_line_1based(source, spec.byte_start));
            chunk.line_end = Some(byte_to_line_1based(source, spec.byte_end));
            chunk
        })
        .collect()
}

/// Convert a byte offset in `source` to a 1-based line number.
/// `byte` is clamped to `source.len()`. Used by `chunks_from_symbols`
/// so indexed records can return line ranges without re-opening the
/// source file at read time.
fn byte_to_line_1based(source: &str, byte: usize) -> u32 {
    let clamped = byte.min(source.len());
    let lines_before = source[..clamped].bytes().filter(|b| *b == b'\n').count();
    (lines_before as u32) + 1
}

fn chunk_from_language_pack(
    path: &Path,
    language: &str,
    processed: &ProcessResult,
    source: &str,
) -> Vec<Chunk> {
    if !processed.chunks.is_empty() {
        return processed
            .chunks
            .iter()
            .enumerate()
            .map(|(idx, code_chunk)| {
                placeholder_chunk(
                    path,
                    "code_block",
                    Some(language),
                    code_chunk.content.clone(),
                    code_chunk.start_byte as u64,
                    code_chunk.end_byte as u64,
                    idx as u32,
                )
            })
            .collect();
    }

    vec![placeholder_chunk(
        path,
        "code_block",
        Some(language),
        source.to_string(),
        0,
        source.len() as u64,
        0,
    )]
}

fn structure_symbol_specs(processed: &ProcessResult, source: &str) -> Vec<SymbolSpec> {
    let mut out = Vec::new();
    for item in &processed.structure {
        collect_structure_item(item, source, &mut Vec::new(), &mut out);
    }
    out
}

fn collect_structure_item(
    item: &StructureItem,
    source: &str,
    stack: &mut Vec<SymbolStackFrame>,
    out: &mut Vec<SymbolSpec>,
) {
    let Some(bare_name) = item.name.as_ref().filter(|name| !name.is_empty()) else {
        return;
    };
    let qualified_name = qualify(stack, bare_name);
    let kind = structure_kind_label(&item.kind);
    let parent_kind = stack.last().map(|frame| frame.kind.clone());
    out.push(SymbolSpec {
        qualified_name: qualified_name.clone(),
        bare_name: bare_name.clone(),
        kind: kind.clone(),
        parent_kind,
        byte_start: item.span.start_byte.min(source.len()),
        byte_end: item.span.end_byte.min(source.len()),
    });
    stack.push(SymbolStackFrame {
        display_name: bare_name.clone(),
        kind,
    });
    for child in &item.children {
        collect_structure_item(child, source, stack, out);
    }
    stack.pop();
}

/// Map the `tree_sitter_language_pack` abstract `StructureKind` to a
/// lowercase string label.
///
/// **This deviates from the design's "must be the tree-sitter node
/// kind" rule** because the structure-pack fallback path runs when
/// the AST walker (`ast_symbol_specs`) produces no symbols — typically
/// for languages outside the curated AST grammar set. We have no
/// `node.kind()` to surface; the language-pack's abstract categories
/// are the most precise label we can attach. Documented as a
/// best-effort fallback in
/// `design/archive/code-nav-symbolic-exploration-impl.md` (CN-D1).
fn structure_kind_label(kind: &StructureKind) -> String {
    match kind {
        StructureKind::Function => "function".to_string(),
        StructureKind::Method => "method".to_string(),
        StructureKind::Class => "class".to_string(),
        StructureKind::Struct => "struct".to_string(),
        StructureKind::Interface => "interface".to_string(),
        StructureKind::Enum => "enum".to_string(),
        StructureKind::Module => "module".to_string(),
        StructureKind::Trait => "trait".to_string(),
        StructureKind::Impl => "impl".to_string(),
        StructureKind::Namespace => "namespace".to_string(),
        StructureKind::Other(other) if !other.is_empty() => other.clone(),
        StructureKind::Other(_) => "unknown".to_string(),
    }
}

fn ast_symbol_specs(root: Node<'_>, source: &str, language: &str) -> Vec<SymbolSpec> {
    let mut out = Vec::new();
    collect_ast_symbols(root, source, language, &mut Vec::new(), &mut out);
    out
}

fn collect_ast_symbols(
    node: Node<'_>,
    source: &str,
    language: &str,
    stack: &mut Vec<SymbolStackFrame>,
    out: &mut Vec<SymbolSpec>,
) {
    let symbol = symbol_name(node, source, language);
    if let Some((bare_name, display_name)) = symbol {
        let qualified_name = qualify(stack, &display_name);
        let kind = node.kind().to_string();
        let parent_kind = stack.last().map(|frame| frame.kind.clone());
        out.push(SymbolSpec {
            qualified_name: qualified_name.clone(),
            bare_name,
            kind: kind.clone(),
            parent_kind,
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
        });
        stack.push(SymbolStackFrame { display_name, kind });
        walk_children(node, source, language, stack, out);
        stack.pop();
    } else {
        walk_children(node, source, language, stack, out);
    }
}

fn walk_children(
    node: Node<'_>,
    source: &str,
    language: &str,
    stack: &mut Vec<SymbolStackFrame>,
    out: &mut Vec<SymbolSpec>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_ast_symbols(child, source, language, stack, out);
    }
}

fn symbol_name(node: Node<'_>, source: &str, language: &str) -> Option<(String, String)> {
    let kind = node.kind();
    // Elixir uses `call` for defmodule/def/defp/defmacro — every other call
    // is also `call`, so we filter by inspecting the head identifier.
    if language == "elixir" && kind == "call" {
        return elixir_call_symbol(node, source);
    }
    if language == "rust" && kind == "impl_item" {
        let display = impl_header(node, source)?;
        let bare = display
            .split_whitespace()
            .last()
            .unwrap_or(display.as_str())
            .trim_matches(|ch: char| !is_ident_char(ch))
            .to_string();
        return Some((bare, display));
    }
    if !is_symbol_node(kind) {
        return None;
    }
    let bare = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, source))
        .or_else(|| fallback_name(kind, node, source))?;
    Some((bare.clone(), bare))
}

pub(crate) fn is_symbol_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_definition"
            | "method_declaration"
            | "method_spec"
            | "class_definition"
            | "class_declaration"
            | "struct_item"
            | "struct_specifier"
            | "field_declaration"
            | "enum_item"
            | "enum_declaration"
            | "trait_item"
            | "interface_declaration"
            | "interface_type"
            | "mod_item"
            | "source_file"
            | "package_declaration"
            | "type_declaration"
            | "type_spec"
    )
}

/// Extract a symbol name from an elixir `call` node when the call head is
/// a definition keyword (defmodule, def, defp, defmacro, defmacrop,
/// defguard, defguardp, defstruct, defprotocol, defimpl, defexception).
/// Other `call` nodes (regular function invocations) return None so we
/// don't pollute the symbol table with every `Foo.bar(x)` call site.
fn elixir_call_symbol(node: Node<'_>, source: &str) -> Option<(String, String)> {
    let mut cursor = node.walk();
    let mut children = node.named_children(&mut cursor);
    let head = children.next()?;
    let head_text = node_text(head, source)?;
    let is_def = matches!(
        head_text.as_str(),
        "defmodule"
            | "def"
            | "defp"
            | "defmacro"
            | "defmacrop"
            | "defguard"
            | "defguardp"
            | "defstruct"
            | "defprotocol"
            | "defimpl"
            | "defexception"
    );
    if !is_def {
        return None;
    }
    // Next child is usually an `arguments` node; the first identifier within
    // it is the symbol name. For `defmodule Witness.Authority do ...` the
    // arg is `Witness.Authority` (alias). For `def start_link(opts) do` the
    // arg is `start_link(opts)` (call); we want the first identifier.
    let args = children.next()?;
    let args_text = node_text(args, source)?;
    let bare = args_text
        .split(|c: char| !is_ident_char(c) && c != '.')
        .find(|s| !s.is_empty())?
        .to_string();
    if bare.is_empty() {
        return None;
    }
    Some((bare.clone(), bare))
}

fn fallback_name(kind: &str, node: Node<'_>, source: &str) -> Option<String> {
    let text = node_text(node, source)?;
    match kind {
        "field_declaration" => text
            .split(':')
            .next()
            .and_then(|left| left.split_whitespace().last())
            .map(clean_ident),
        "source_file" | "package_declaration" => None,
        _ => first_identifier_after_keyword(&text),
    }
    .filter(|name| !name.is_empty())
}

fn first_identifier_after_keyword(text: &str) -> Option<String> {
    let keywords = [
        "fn",
        "function",
        "class",
        "struct",
        "enum",
        "trait",
        "interface",
        "mod",
        "type",
    ];
    let mut prev_was_keyword = false;
    for token in text.split(|ch: char| !is_ident_char(ch)) {
        if token.is_empty() {
            continue;
        }
        if prev_was_keyword {
            return Some(token.to_string());
        }
        prev_was_keyword = keywords.contains(&token);
    }
    None
}

fn impl_header(node: Node<'_>, source: &str) -> Option<String> {
    let text = node_text(node, source)?;
    let header = text.split('{').next()?.trim();
    Some(header.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn node_text(node: Node<'_>, source: &str) -> Option<String> {
    source
        .get(node.start_byte()..node.end_byte())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn qualify(stack: &[SymbolStackFrame], name: &str) -> String {
    if stack.is_empty() {
        name.to_string()
    } else {
        let joined = stack
            .iter()
            .map(|frame| frame.display_name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        format!("{joined}::{name}")
    }
}

fn clean_ident(raw: &str) -> String {
    raw.trim_matches(|ch: char| !is_ident_char(ch)).to_string()
}

fn is_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CN-D2 contract: chunks built from a SymbolSpec must carry the
    /// new symbol_kind / parent_kind / line_start / line_end fields.
    /// This is what unblocks the indexed code_symbols lane in CN-T2 —
    /// without these on the Chunk record, CN-D3 has nothing to project
    /// into tantivy.
    #[test]
    fn rust_impl_method_chunks_carry_kind_and_line_metadata() {
        let source = b"struct S;\nimpl S {\n    fn run(&self) {}\n}\n";
        let (chunks, _) = CodeChunker.chunk(Path::new("t.rs"), source).unwrap();

        let struct_chunk = chunks
            .iter()
            .find(|c| {
                c.symbol_exact.as_deref() == Some("S")
                    && c.symbol_kind.as_deref() == Some("struct_item")
            })
            .expect("struct chunk present with symbol_kind=struct_item");
        assert_eq!(struct_chunk.parent_kind, None);
        assert_eq!(struct_chunk.line_start, Some(1));
        assert_eq!(struct_chunk.line_end, Some(1));

        let method_chunk = chunks
            .iter()
            .find(|c| c.symbol_exact.as_deref() == Some("run"))
            .expect("method chunk present");
        assert_eq!(method_chunk.symbol_kind.as_deref(), Some("function_item"));
        assert_eq!(method_chunk.parent_kind.as_deref(), Some("impl_item"));
        assert_eq!(method_chunk.line_start, Some(3));
        assert_eq!(method_chunk.line_end, Some(3));
    }

    /// byte_to_line_1based is the helper CN-D3 reads to populate
    /// tantivy line fields without re-opening the source file. Lock
    /// its behaviour explicitly: byte 0 -> line 1, byte after a
    /// newline -> next line.
    #[test]
    fn byte_to_line_handles_multiline_source() {
        let source = "alpha\nbeta\ngamma";
        assert_eq!(byte_to_line_1based(source, 0), 1); // alpha
        assert_eq!(byte_to_line_1based(source, 5), 1); // last byte of line 1
        assert_eq!(byte_to_line_1based(source, 6), 2); // beta
        assert_eq!(byte_to_line_1based(source, 11), 3); // gamma
        // Past EOF clamps to line 3.
        assert_eq!(byte_to_line_1based(source, 999), 3);
    }

    #[test]
    fn rust_impl_display_chunks_with_symbol_metadata() {
        let source = br#"
pub enum EntityRef { Knowledge { id: String } }

impl Display for EntityRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "entity")
    }
}
"#;
        let (chunks, _edges) = CodeChunker
            .chunk(Path::new("src/entity_ref.rs"), source)
            .unwrap();
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.content.contains("impl Display for EntityRef"))
        );
        assert!(chunks.iter().any(|chunk| chunk.symbol.is_some()));
    }

    /// Helper: parse `source` with `language`, then run the AST symbol
    /// walker and return the produced SymbolSpecs.
    fn ast_specs_for(language: &str, source: &str) -> Vec<SymbolSpec> {
        let mut parser = parser_for_language(language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        ast_symbol_specs(tree.root_node(), source, language)
    }

    /// Rust impl methods are the load-bearing case for CN-T2's indexed
    /// vs live equivalence: a `function_item` whose enclosing symbol is
    /// an `impl_item` is what `refactor::status` synthesises as
    /// `impl_method`. The indexed lane only has access to the raw
    /// tree-sitter kinds, so `parent_kind` must carry `impl_item` here
    /// or the synthesis derivation cannot reproduce.
    #[test]
    fn symbolspec_kind_and_parent_kind_for_rust_impl_method() {
        let source = "struct S; impl S { fn run(&self) {} fn stop(&self) {} }";
        let specs = ast_specs_for("rust", source);

        let struct_spec = specs.iter().find(|s| s.bare_name == "S").unwrap();
        assert_eq!(struct_spec.kind, "struct_item");
        assert_eq!(struct_spec.parent_kind, None);

        let run = specs.iter().find(|s| s.bare_name == "run").unwrap();
        assert_eq!(run.kind, "function_item");
        assert_eq!(run.parent_kind.as_deref(), Some("impl_item"));

        let stop = specs.iter().find(|s| s.bare_name == "stop").unwrap();
        assert_eq!(stop.kind, "function_item");
        assert_eq!(stop.parent_kind.as_deref(), Some("impl_item"));
    }

    /// Top-level Rust functions must have `parent_kind = None` so the
    /// indexed lane does not accidentally synthesise them as
    /// `impl_method`.
    #[test]
    fn symbolspec_top_level_rust_function_has_no_parent_kind() {
        let source = "fn standalone() {}";
        let specs = ast_specs_for("rust", source);
        let spec = specs.iter().find(|s| s.bare_name == "standalone").unwrap();
        assert_eq!(spec.kind, "function_item");
        assert_eq!(spec.parent_kind, None);
    }

    /// Java method declarations live under `class_body` (the immediate
    /// AST parent), but `parent_kind` tracks the nearest *enclosing
    /// symbol* — the class declaration — not the wrapper node.
    #[test]
    fn symbolspec_java_method_parent_kind_is_class_declaration() {
        let source = "class Test { void run() {} void stop() {} }";
        let specs = ast_specs_for("java", source);
        let class = specs.iter().find(|s| s.bare_name == "Test").unwrap();
        assert_eq!(class.kind, "class_declaration");
        assert_eq!(class.parent_kind, None);
        let run = specs.iter().find(|s| s.bare_name == "run").unwrap();
        assert_eq!(run.kind, "method_declaration");
        assert_eq!(run.parent_kind.as_deref(), Some("class_declaration"));
    }

    /// Python functions inside classes get `parent_kind =
    /// class_definition`. Methods at module top level get `None`.
    #[test]
    fn symbolspec_python_method_vs_top_level() {
        let source = "class C:\n    def m(self):\n        pass\n\ndef top():\n    pass\n";
        let specs = ast_specs_for("python", source);
        let class = specs.iter().find(|s| s.bare_name == "C").unwrap();
        assert_eq!(class.kind, "class_definition");
        assert_eq!(class.parent_kind, None);
        let method = specs.iter().find(|s| s.bare_name == "m").unwrap();
        assert_eq!(method.kind, "function_definition");
        assert_eq!(method.parent_kind.as_deref(), Some("class_definition"));
        let top = specs.iter().find(|s| s.bare_name == "top").unwrap();
        assert_eq!(top.kind, "function_definition");
        assert_eq!(top.parent_kind, None);
    }

    /// TypeScript class method: `method_definition` enclosed by
    /// `class_declaration`. Top-level function: `function_declaration`
    /// with `parent_kind = None`.
    #[test]
    fn symbolspec_typescript_class_method_vs_top_level() {
        let source = "class C { run() {} }\nfunction top() {}\n";
        let specs = ast_specs_for("typescript", source);
        let class = specs.iter().find(|s| s.bare_name == "C").unwrap();
        assert_eq!(class.kind, "class_declaration");
        assert_eq!(class.parent_kind, None);
        let method = specs.iter().find(|s| s.bare_name == "run").unwrap();
        assert_eq!(method.kind, "method_definition");
        assert_eq!(method.parent_kind.as_deref(), Some("class_declaration"));
        let top = specs.iter().find(|s| s.bare_name == "top").unwrap();
        assert_eq!(top.kind, "function_declaration");
        assert_eq!(top.parent_kind, None);
    }

    /// Elixir keeps `kind = "call"` (the raw tree-sitter node kind for
    /// defmodule / def / defp / defmacro). The promotion to a more
    /// specific marker is deferred to a separate `symbol_kind_display`
    /// surface (out of scope for CN-D1). Per the design doc, raw node
    /// kind is the contract.
    #[test]
    fn symbolspec_elixir_keeps_raw_call_kind() {
        let source = "defmodule M do\n  def run, do: :ok\nend\n";
        let specs = ast_specs_for("elixir", source);
        let module = specs.iter().find(|s| s.bare_name == "M").unwrap();
        assert_eq!(module.kind, "call");
        assert_eq!(module.parent_kind, None);
        let func = specs.iter().find(|s| s.bare_name == "run").unwrap();
        assert_eq!(func.kind, "call");
        assert_eq!(func.parent_kind.as_deref(), Some("call"));
    }

    #[test]
    fn language_for_path_recognizes_pack_languages() {
        for (path, language) in [
            ("src/foo.erl", "erlang"),
            ("lib/foo.ex", "elixir"),
            ("lib/foo.rb", "ruby"),
            ("src/foo.ml", "ocaml"),
            ("src/foo.hs", "haskell"),
            ("src/foo.swift", "swift"),
            ("src/foo.kt", "kotlin"),
            ("src/foo.scala", "scala"),
            ("src/foo.lua", "lua"),
            ("scripts/foo.sh", "bash"),
            ("data/foo.json", "json"),
            ("data/foo.yaml", "yaml"),
            ("config/foo.toml", "toml"),
            ("web/foo.html", "html"),
            ("web/foo.css", "css"),
            ("db/foo.sql", "sql"),
        ] {
            assert_eq!(language_for_path(Path::new(path)), Some(language));
        }
    }
}
