use std::path::Path;

use anyhow::Result;
use scraper::{Html, Node};

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

/// Windowed-fallback target chunk size for heading-less pages, mirroring
/// `text.rs`'s `TARGET_CHUNK_BYTES` house number for windowed text
/// chunking. Section chunks (the heading-boundary path) intentionally do
/// NOT self-cap here, mirroring `markdown.rs`: the framework's
/// `MAX_CHUNK_BYTES` safety net in `bbox-corpus-index`'s `bound_chunks`
/// splits any oversized section after the fact.
const TARGET_CHUNK_BYTES: usize = 1024;

/// Tags whose entire subtree is dropped before extraction: script/style
/// are never readable content, nav/footer/aside are boilerplate chrome,
/// noscript/template hold markup that isn't rendered content.
const SKIP_TAGS: &[&str] = &[
    "script", "style", "nav", "footer", "aside", "noscript", "template", "head",
];

/// h1-h3 are section boundaries. h4-h6 stay inside the enclosing section as
/// ordinary block content, mirroring how `markdown.rs` only splits on `##`
/// (h2) and leaves deeper headings inline in the section body.
const SECTION_HEADING_TAGS: &[&str] = &["h1", "h2", "h3"];

/// Block-level tags that get a paragraph break inserted around them, so
/// extracted text keeps readable paragraph structure. This gives the
/// heading-less windowed fallback real paragraph boundaries to split on,
/// the same way `text.rs`'s paragraph splitter relies on blank lines.
const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "li",
    "tr",
    "br",
    "blockquote",
    "section",
    "article",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "table",
    "pre",
    "hr",
    "header",
    "main",
    "figure",
    "figcaption",
];

/// HTML / web page chunker (X-HTML,
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
/// Extracts readable text (script/style/nav/footer/aside/noscript/template
/// subtrees dropped) and splits it into `web_section` chunks on h1-h3
/// boundaries, falling back to windowed `web_text` chunks for pages with no
/// headings.
///
/// `LINKS_TO_URL`/`EMBEDS_FRAME` edges from the original spec are
/// deliberately not implemented in this pass: no other chunker in this
/// registry currently emits chunk-level edges (`PdfChunker`, `MarkdownChunker`,
/// the config chunkers, and `PlainTextChunker` all return `Vec::new()` for
/// edges — `NEXT_SECTION` is derived later by the indexer itself, not by
/// individual chunkers), so there is no live per-chunker edge channel to
/// plug into yet. `chunk()` returns an empty edge vec to match that
/// established posture rather than inventing a new one.
///
/// Known limitation (not fixable from this module): `Html::parse_document`
/// (html5ever's HTML5 tree-construction algorithm) was measured
/// superlinear in wall time against pathologically deep pure-nesting input
/// (20,000 nested `<div>`s parsed in ~14s on this host; this module's own
/// traversal of that same tree took ~7ms), independent of anything this
/// chunker does. `bbox-corpus-index`'s `MAX_FILE_BYTES` (2MB) bounds file
/// size before a file ever reaches this chunker, but does not bound parse
/// time for an adversarially deep-but-small file within that budget. This
/// is an upstream html5ever/scraper characteristic, not specific to this
/// integration; flagged here rather than worked around.
pub struct HtmlChunker;

impl SourceFormatChunker for HtmlChunker {
    fn format_id(&self) -> &str {
        "html"
    }

    fn claims(&self, path: &Path, _sniff: &[u8]) -> bool {
        matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("html" | "htm" | "xhtml")
        )
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        Ok((extract_chunks(path, bytes), Vec::new()))
    }
}

struct Section {
    heading: Option<String>,
    buffer: String,
}

fn extract_chunks(path: &Path, bytes: &[u8]) -> Vec<Chunk> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "html file is not valid utf-8 (binary masquerading as html); skipping"
            );
            return Vec::new();
        }
    };
    if text.trim().is_empty() {
        return Vec::new();
    }

    // html5ever's HTML5 tree-construction algorithm always produces a tree
    // for any input string (it never rejects or errors on malformed
    // markup), so `Html::parse_document` cannot panic here; extraction
    // below degrades to zero chunks on its own when the result carries no
    // readable text.
    let document = Html::parse_document(text);
    let sections = extract_sections(&document);
    let has_headings = sections.iter().any(|section| section.heading.is_some());

    if has_headings {
        section_chunks(path, sections)
    } else {
        let flat = sections
            .into_iter()
            .map(|section| section.buffer)
            .collect::<Vec<_>>()
            .join("\n\n");
        windowed_chunks(path, &flat)
    }
}

/// Walk the parsed document in document order with an explicit stack
/// (never recursion), so pathologically deep or huge nesting can't blow the
/// call stack. Skip-tag subtrees are dropped entirely; h1-h3 elements open
/// a new section carrying their own text as the section identity (the
/// heading-text analog of how `markdown.rs` carries `## heading` as the
/// leading line of a `doc_section` chunk); every other text node is
/// appended to the current section's buffer, with a paragraph break
/// inserted around block-level tags.
fn extract_sections(document: &Html) -> Vec<Section> {
    let mut sections = vec![Section {
        heading: None,
        buffer: String::new(),
    }];

    let mut roots: Vec<_> = document.tree.root().children().collect();
    roots.reverse();
    let mut stack = roots;

    while let Some(node) = stack.pop() {
        match node.value() {
            Node::Element(element) => {
                let tag = element.name();
                if SKIP_TAGS.contains(&tag) {
                    continue;
                }
                if SECTION_HEADING_TAGS.contains(&tag) {
                    let heading_text = normalize_whitespace(&collect_text(node));
                    if !heading_text.is_empty() {
                        sections.push(Section {
                            heading: Some(heading_text.clone()),
                            buffer: heading_text,
                        });
                    }
                    // The heading's own text is already captured above;
                    // don't descend into it again as ordinary content.
                    continue;
                }
                if BLOCK_TAGS.contains(&tag) {
                    push_paragraph_break(current_buffer_mut(&mut sections));
                }
                let mut children: Vec<_> = node.children().collect();
                children.reverse();
                stack.extend(children);
            }
            Node::Text(text) => {
                let normalized = normalize_whitespace(text);
                if !normalized.is_empty() {
                    append_inline_text(current_buffer_mut(&mut sections), &normalized);
                }
            }
            _ => {}
        }
    }

    sections
}

/// `sections` is seeded with one entry before traversal starts and only
/// ever grows (a heading push), so `last_mut()` is always `Some`.
fn current_buffer_mut(sections: &mut [Section]) -> &mut String {
    &mut sections
        .last_mut()
        .expect("sections always has at least the seeded leading entry")
        .buffer
}

/// Iteratively collect all readable text under `node` (used for a single
/// heading element's own text), still skipping `SKIP_TAGS` subtrees and
/// still stack-based rather than recursive for the same huge/malformed-input
/// safety reason as `extract_sections`.
fn collect_text(node: ego_tree::NodeRef<'_, Node>) -> String {
    let mut buffer = String::new();
    let mut children: Vec<_> = node.children().collect();
    children.reverse();
    let mut stack = children;

    while let Some(node) = stack.pop() {
        match node.value() {
            Node::Element(element) => {
                if SKIP_TAGS.contains(&element.name()) {
                    continue;
                }
                let mut children: Vec<_> = node.children().collect();
                children.reverse();
                stack.extend(children);
            }
            Node::Text(text) => {
                let normalized = normalize_whitespace(text);
                if !normalized.is_empty() {
                    append_inline_text(&mut buffer, &normalized);
                }
            }
            _ => {}
        }
    }

    buffer
}

fn append_inline_text(buffer: &mut String, normalized: &str) {
    if !buffer.is_empty() && !buffer.ends_with('\n') && !buffer.ends_with(' ') {
        buffer.push(' ');
    }
    buffer.push_str(normalized);
}

fn push_paragraph_break(buffer: &mut String) {
    if buffer.is_empty() || buffer.ends_with("\n\n") {
        return;
    }
    if buffer.ends_with('\n') {
        buffer.push('\n');
    } else {
        buffer.push_str("\n\n");
    }
}

/// Collapse any run of whitespace (spaces, tabs, newlines from the raw
/// markup's own formatting) to a single space and trim the ends. HTML
/// whitespace is not significant outside `pre`/textual-formatting
/// intent, so this avoids carrying the source file's indentation into the
/// extracted content.
fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn section_chunks(path: &Path, sections: Vec<Section>) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut byte_offset = 0u64;
    for section in sections {
        let content = section.buffer.trim();
        if content.is_empty() {
            continue;
        }
        let start = byte_offset;
        let end = start + content.len() as u64;
        chunks.push(placeholder_chunk(
            path,
            "web_section",
            None,
            content.to_string(),
            start,
            end,
            chunks.len() as u32,
        ));
        byte_offset = end + 1;
    }
    chunks
}

/// Windowed fallback for heading-less pages: accumulate paragraph-separated
/// text (paragraphs delimited by the blank lines `extract_sections`
/// inserted around block-level tags) into ~`TARGET_CHUNK_BYTES` windows,
/// mirroring `text.rs`'s `PlainTextChunker` windowing.
fn windowed_chunks(path: &Path, flat: &str) -> Vec<Chunk> {
    let paragraphs: Vec<&str> = flat
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .collect();
    if paragraphs.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut byte_offset = 0u64;
    let mut current_start = byte_offset;

    for paragraph in paragraphs {
        if !current.is_empty() && current.len() + paragraph.len() + 2 > TARGET_CHUNK_BYTES {
            let end = current_start + current.len() as u64;
            chunks.push(placeholder_chunk(
                path,
                "web_text",
                None,
                current.trim().to_string(),
                current_start,
                end,
                chunks.len() as u32,
            ));
            byte_offset = end + 1;
            current.clear();
            current_start = byte_offset;
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }

    if !current.trim().is_empty() {
        let end = current_start + current.len() as u64;
        chunks.push(placeholder_chunk(
            path,
            "web_text",
            None,
            current.trim().to_string(),
            current_start,
            end,
            chunks.len() as u32,
        ));
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_html_htm_and_xhtml_extensions() {
        assert!(HtmlChunker.claims(Path::new("page.html"), b""));
        assert!(HtmlChunker.claims(Path::new("page.htm"), b""));
        assert!(HtmlChunker.claims(Path::new("page.xhtml"), b""));
        assert!(!HtmlChunker.claims(Path::new("page.txt"), b""));
    }

    #[test]
    fn sectioned_page_splits_on_headings_and_drops_nav_and_script() {
        let input = r#"
            <html>
              <head><title>Doc</title></head>
              <body>
                <nav>Home | About | Contact</nav>
                <script>trackPageView();</script>
                <style>.hero { color: red; }</style>
                <h1>Getting Started</h1>
                <p>Welcome to the guide.</p>
                <h2>Installation</h2>
                <p>Run the installer.</p>
                <h2>Usage</h2>
                <p>Call the API.</p>
                <footer>Copyright 2026</footer>
              </body>
            </html>
        "#;
        let (chunks, edges) = HtmlChunker
            .chunk(Path::new("guide.html"), input.as_bytes())
            .expect("chunk() must not error on well-formed html");
        assert!(edges.is_empty());
        assert_eq!(chunks.len(), 3, "chunks: {chunks:?}");

        for chunk in &chunks {
            assert_eq!(chunk.chunk_kind, "web_section");
        }
        assert!(chunks[0].content.starts_with("Getting Started"));
        assert!(chunks[0].content.contains("Welcome to the guide"));
        assert!(chunks[1].content.starts_with("Installation"));
        assert!(chunks[1].content.contains("Run the installer"));
        assert!(chunks[2].content.starts_with("Usage"));
        assert!(chunks[2].content.contains("Call the API"));

        let full = chunks
            .iter()
            .map(|chunk| chunk.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !full.contains("Home | About | Contact"),
            "nav content leaked into chunks: {full}"
        );
        assert!(
            !full.contains("trackPageView"),
            "script content leaked into chunks: {full}"
        );
        assert!(
            !full.contains("color: red"),
            "style content leaked into chunks: {full}"
        );
        assert!(
            !full.contains("Copyright 2026"),
            "footer content leaked into chunks: {full}"
        );
    }

    #[test]
    fn heading_less_page_falls_back_to_windowed_chunks() {
        let long_paragraph = "word ".repeat(400); // ~2000 bytes, forces a window split
        let input =
            format!("<html><body><p>{long_paragraph}</p><p>{long_paragraph}</p></body></html>");
        let (chunks, edges) = HtmlChunker
            .chunk(Path::new("notes.html"), input.as_bytes())
            .expect("chunk() must not error on well-formed html");
        assert!(edges.is_empty());
        assert!(
            chunks.len() >= 2,
            "expected multiple windowed chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            assert_eq!(chunk.chunk_kind, "web_text");
            assert!(!chunk.content.is_empty());
        }
    }

    #[test]
    fn invalid_utf8_bytes_produce_no_chunks_and_do_not_panic() {
        let garbage: &[u8] = &[
            0xFF, 0xFE, 0x00, 0x01, 0x02, 0xC0, 0x80, 0xF5, 0x80, 0x80, 0x80,
        ];
        let (chunks, edges) = HtmlChunker
            .chunk(Path::new("blob.html"), garbage)
            .expect("chunk() must never return Err for invalid utf-8 input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn empty_byte_stream_produces_no_chunks() {
        let (chunks, edges) = HtmlChunker
            .chunk(Path::new("empty.html"), b"")
            .expect("chunk() must never return Err for empty input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn deeply_nested_markup_does_not_panic() {
        // A pathologically deep tag stack is the shape most likely to blow a
        // recursive tree walker's call stack; extract_sections/collect_text
        // are stack-based (not recursive) specifically to survive this.
        // Depth is kept moderate (not the ~100k+ that would actually stress
        // a stack) because `Html::parse_document` itself (html5ever's tree
        // construction, not this module's traversal) has been measured
        // superlinear at extreme depth — see html.rs's module doc comment.
        let depth = 3_000;
        let mut input = String::from("<html><body>");
        input.push_str(&"<div>".repeat(depth));
        input.push_str("<h1>Deep</h1>deep text");
        input.push_str(&"</div>".repeat(depth));
        input.push_str("</body></html>");

        let (chunks, edges) = HtmlChunker
            .chunk(Path::new("deep.html"), input.as_bytes())
            .expect("chunk() must never return Err on deeply nested markup");
        assert!(edges.is_empty());
        assert!(!chunks.is_empty());
    }
}
