use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
use tree_sitter::Node;
use tree_sitter_language_pack::{ProcessConfig, ProcessResult, StructureItem, get_parser, process};

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

pub struct CodeChunker;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolSpec {
    qualified_name: String,
    bare_name: String,
    byte_start: usize,
    byte_end: usize,
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

fn parser_for_language(language: &str) -> Result<tree_sitter::Parser> {
    if let Ok(parser) = get_parser(language) {
        return Ok(parser);
    }
    let mut parser = tree_sitter::Parser::new();
    match language {
        "rust" => parser.set_language(&tree_sitter_rust::LANGUAGE.into()),
        "python" => parser.set_language(&tree_sitter_python::LANGUAGE.into()),
        "csharp" => parser.set_language(&tree_sitter_c_sharp::LANGUAGE.into()),
        "java" => parser.set_language(&tree_sitter_java::LANGUAGE.into()),
        "go" => parser.set_language(&tree_sitter_go::LANGUAGE.into()),
        "typescript" => parser.set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => parser.set_language(&tree_sitter_javascript::LANGUAGE.into()),
        "c" => parser.set_language(&tree_sitter_c::LANGUAGE.into()),
        "cpp" => parser.set_language(&tree_sitter_cpp::LANGUAGE.into()),
        _ => return Err(anyhow!("unsupported language {language}")),
    }
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
            chunk
        })
        .collect()
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
    parents: &mut Vec<String>,
    out: &mut Vec<SymbolSpec>,
) {
    let Some(bare_name) = item.name.as_ref().filter(|name| !name.is_empty()) else {
        return;
    };
    let qualified_name = qualify(parents, bare_name);
    out.push(SymbolSpec {
        qualified_name: qualified_name.clone(),
        bare_name: bare_name.clone(),
        byte_start: item.span.start_byte.min(source.len()),
        byte_end: item.span.end_byte.min(source.len()),
    });
    parents.push(bare_name.clone());
    for child in &item.children {
        collect_structure_item(child, source, parents, out);
    }
    parents.pop();
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
    parents: &mut Vec<String>,
    out: &mut Vec<SymbolSpec>,
) {
    let symbol = symbol_name(node, source, language);
    if let Some((bare_name, display_name)) = symbol {
        let qualified_name = qualify(parents, &display_name);
        out.push(SymbolSpec {
            qualified_name: qualified_name.clone(),
            bare_name,
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
        });
        parents.push(display_name);
        walk_children(node, source, language, parents, out);
        parents.pop();
    } else {
        walk_children(node, source, language, parents, out);
    }
}

fn walk_children(
    node: Node<'_>,
    source: &str,
    language: &str,
    parents: &mut Vec<String>,
    out: &mut Vec<SymbolSpec>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_ast_symbols(child, source, language, parents, out);
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

fn is_symbol_node(kind: &str) -> bool {
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

fn qualify(parents: &[String], name: &str) -> String {
    if parents.is_empty() {
        name.to_string()
    } else {
        format!("{}::{name}", parents.join("::"))
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
