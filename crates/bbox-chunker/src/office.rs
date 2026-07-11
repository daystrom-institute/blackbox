use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::Result;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use regex::Regex;

use super::{Chunk, Edge, MAX_CHUNK_BYTES, SourceFormatChunker, placeholder_chunk};

/// OOXML (.docx/.pptx) containers are zip archives, which begin with the ZIP
/// local file header signature. Encrypted or legacy binary `.doc`/`.ppt`
/// files (OLE2/CFBF compound documents) start with a different magic
/// (`\xD0\xCF\x11\xE0...`) and are legacy formats deliberately out of scope
/// here (see the module doc comment on `DocxChunker`/`PptxChunker`), so a
/// plain byte-0 prefix check (rather than PDF's scan window) is the correct
/// gate: there is no legitimate producer that prepends junk before a zip's
/// local file header the way some PDF producers do before `%PDF-`.
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

/// Text-first Word document chunker (X-DOCX-PPTX,
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
/// Parses `word/document.xml` directly out of the OOXML zip container rather
/// than depending on `docx-rs` (see the dependency comment in `Cargo.toml`).
/// Paragraphs styled `Heading1`/`Heading2`/`Heading3` (matched against the
/// paragraph's `w:pStyle` style id, which is locale-independent, not its
/// display name in `styles.xml`) become section boundaries; documents with no
/// detectable heading styles fall back to windowed chunks sized to the
/// crate's shared `MAX_CHUNK_BYTES`. Encrypted (OLE2-wrapped), corrupt, or
/// non-OOXML zip files degrade to zero chunks rather than an error: like
/// `PdfChunker`, a bubbled `Err` here aborts the entire background reindex
/// pass, not just this file, so extraction failures are caught and swallowed.
pub struct DocxChunker;

/// Text-first PowerPoint chunker (X-DOCX-PPTX, same design doc as
/// `DocxChunker`). One `slide` chunk per non-empty `ppt/slides/slideN.xml`
/// entry, carrying all `a:t` text runs found in that slide. The slide number
/// parsed from the entry's filename is carried in the position fields the
/// same way `PdfChunker` carries page numbers (see its doc comment); slides
/// are ordered by that number, which is filename order rather than a
/// `presentation.xml` `sldIdLst`-resolved display order, since minimal text
/// extraction does not need presentation-level slide reordering.
pub struct PptxChunker;

impl SourceFormatChunker for DocxChunker {
    fn format_id(&self) -> &str {
        "docx"
    }

    fn claims(&self, path: &Path, sniff: &[u8]) -> bool {
        path.extension().and_then(|ext| ext.to_str()) == Some("docx")
            && sniff.starts_with(ZIP_MAGIC)
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        Ok((extract_docx_sections(path, bytes), Vec::new()))
    }
}

impl SourceFormatChunker for PptxChunker {
    fn format_id(&self) -> &str {
        "pptx"
    }

    fn claims(&self, path: &Path, sniff: &[u8]) -> bool {
        path.extension().and_then(|ext| ext.to_str()) == Some("pptx")
            && sniff.starts_with(ZIP_MAGIC)
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        Ok((extract_pptx_slides(path, bytes), Vec::new()))
    }
}

/// A single `w:p` (docx) or `a:p` (pptx) paragraph's flattened text plus its
/// `w:pStyle` style id, if any (pptx paragraphs never set this; the field is
/// unused there).
struct XmlParagraph {
    text: String,
    style: Option<String>,
}

fn extract_docx_sections(path: &Path, bytes: &[u8]) -> Vec<Chunk> {
    // `zip`/`quick-xml` are pure-safe-Rust and are not known to panic on
    // malformed input, but the extraction is wrapped in catch_unwind anyway
    // (matching PdfChunker's posture) so a corrupt/adversarial .docx can
    // never abort the reindex pass, only degrade to zero chunks for this
    // file.
    let extraction = std::panic::catch_unwind(|| read_zip_entry(bytes, "word/document.xml"));
    let xml = match extraction {
        Ok(Some(xml)) => xml,
        Ok(None) => {
            tracing::warn!(
                path = %path.display(),
                "docx text extraction failed (encrypted, corrupt, or missing word/document.xml); skipping"
            );
            return Vec::new();
        }
        Err(_) => {
            tracing::warn!(path = %path.display(), "docx extraction panicked; skipping");
            return Vec::new();
        }
    };

    let paragraphs = match std::panic::catch_unwind(|| extract_paragraphs(&xml)) {
        Ok(paragraphs) => paragraphs,
        Err(_) => {
            tracing::warn!(path = %path.display(), "docx document.xml parse panicked; skipping");
            return Vec::new();
        }
    };

    let mut chunks = Vec::new();
    let mut byte_offset = 0u64;
    for section in docx_sections(&paragraphs) {
        let content = section.trim();
        if content.is_empty() {
            continue;
        }
        let byte_start = byte_offset;
        let byte_end = byte_start + content.len() as u64;
        // Text-first, like pdf_page: no language tag, so bucket routing
        // (bbox-embed enqueue_project_file) falls through to Docs rather
        // than Code.
        chunks.push(placeholder_chunk(
            path,
            "office_section",
            None,
            content.to_string(),
            byte_start,
            byte_end,
            chunks.len() as u32,
        ));
        byte_offset = byte_end + 1;
    }
    chunks
}

fn extract_pptx_slides(path: &Path, bytes: &[u8]) -> Vec<Chunk> {
    let extraction = std::panic::catch_unwind(|| list_slide_xml(bytes));
    let mut slides = match extraction {
        Ok(slides) => slides,
        Err(_) => {
            tracing::warn!(path = %path.display(), "pptx extraction panicked; skipping");
            return Vec::new();
        }
    };
    slides.sort_by_key(|(number, _)| *number);

    let mut chunks = Vec::new();
    let mut byte_offset = 0u64;
    for (slide_number, xml) in slides {
        let paragraphs = match std::panic::catch_unwind(|| extract_paragraphs(&xml)) {
            Ok(paragraphs) => paragraphs,
            Err(_) => {
                tracing::warn!(
                    path = %path.display(),
                    slide = slide_number,
                    "pptx slide XML parse panicked; skipping slide"
                );
                continue;
            }
        };
        let content = paragraphs
            .iter()
            .map(|paragraph| paragraph.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if content.is_empty() {
            continue;
        }
        let byte_start = byte_offset;
        let byte_end = byte_start + content.len() as u64;
        let mut chunk = placeholder_chunk(
            path,
            "slide",
            None,
            content,
            byte_start,
            byte_end,
            chunks.len() as u32,
        );
        chunk.line_start = Some(slide_number);
        chunk.line_end = Some(slide_number);
        byte_offset = byte_end + 1;
        chunks.push(chunk);
    }
    chunks
}

/// Reads one named entry out of a zip archive. `None` covers every failure
/// mode uniformly (not a zip at all, entry missing, decompression error) so
/// callers get a single degrade-to-empty path; the caller logs the reason
/// class since this function only distinguishes success/absence.
fn read_zip_entry(bytes: &[u8], entry_name: &str) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut file = archive.by_name(entry_name).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Lists every `ppt/slides/slideN.xml` entry's raw bytes, tagged with its
/// parsed slide number. Returns an empty vec for anything that isn't a
/// readable zip (encrypted/corrupt) or has no matching entries (a zip file
/// that isn't a PPTX, e.g. a plain zip archive with a `.pptx` extension).
fn list_slide_xml(bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Vec::new();
    };
    let slide_path = Regex::new(r"^ppt/slides/slide(\d+)\.xml$").expect("valid slide path regex");
    let mut matches: Vec<(u32, String)> = archive
        .file_names()
        .filter_map(|name| {
            let captures = slide_path.captures(name)?;
            let number: u32 = captures[1].parse().ok()?;
            Some((number, name.to_string()))
        })
        .collect();
    matches.sort_by_key(|(number, _)| *number);

    let mut slides = Vec::new();
    for (number, name) in matches {
        let Ok(mut file) = archive.by_name(&name) else {
            continue;
        };
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_ok() {
            slides.push((number, buf));
        }
    }
    slides
}

/// Streams paragraphs (`w:p` in docx, `a:p` in pptx) out of an OOXML XML
/// part, matching on local element names so the `w:`/`a:` namespace prefixes
/// (fixed by convention but not guaranteed by the XML spec) don't need to be
/// resolved. Malformed XML stops the stream and returns whatever paragraphs
/// were already collected, rather than erroring.
fn extract_paragraphs(xml: &[u8]) -> Vec<XmlParagraph> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut paragraphs = Vec::new();

    let mut in_paragraph = false;
    let mut in_text_run = false;
    let mut current_text = String::new();
    let mut current_style: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"p" => {
                    in_paragraph = true;
                    current_text.clear();
                    current_style = None;
                }
                b"t" => in_text_run = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => match local_name(e.name().as_ref()) {
                b"pStyle" => current_style = attr_value(&e, b"val"),
                b"tab" if in_paragraph => current_text.push('\t'),
                b"br" | b"cr" if in_paragraph => current_text.push('\n'),
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if in_text_run {
                    current_text.push_str(&decode_text(&e));
                }
            }
            Ok(Event::End(e)) => match local_name(e.name().as_ref()) {
                b"t" => in_text_run = false,
                b"p" => {
                    if in_paragraph {
                        paragraphs.push(XmlParagraph {
                            text: std::mem::take(&mut current_text),
                            style: current_style.take(),
                        });
                    }
                    in_paragraph = false;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    paragraphs
}

fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().rposition(|&byte| byte == b':') {
        Some(idx) => &qname[idx + 1..],
        None => qname,
    }
}

fn attr_value(start: &BytesStart, local: &[u8]) -> Option<String> {
    start.attributes().flatten().find_map(|attr| {
        if local_name(attr.key.as_ref()) == local {
            attr.unescape_value().ok().map(|value| value.into_owned())
        } else {
            None
        }
    })
}

fn decode_text(event: &BytesText) -> String {
    let Ok(raw) = event.decode() else {
        return String::new();
    };
    match unescape(&raw) {
        Ok(text) => text.into_owned(),
        Err(_) => raw.into_owned(),
    }
}

fn is_heading_style(style: &str) -> bool {
    style.eq_ignore_ascii_case("Heading1")
        || style.eq_ignore_ascii_case("Heading2")
        || style.eq_ignore_ascii_case("Heading3")
}

/// Splits paragraphs into `office_section` content blocks. When at least one
/// heading-styled paragraph is present, sections run from each heading
/// (inclusive) to the next heading (exclusive), mirroring
/// `markdown::h2_sections`; any paragraphs before the first heading form a
/// leading, unstyled section. When no heading styles are detectable at all,
/// falls back to windowed accumulation at `MAX_CHUNK_BYTES`.
fn docx_sections(paragraphs: &[XmlParagraph]) -> Vec<String> {
    let mut starts: Vec<usize> = paragraphs
        .iter()
        .enumerate()
        .filter(|(_, paragraph)| paragraph.style.as_deref().is_some_and(is_heading_style))
        .map(|(idx, _)| idx)
        .collect();

    if starts.is_empty() {
        return windowed_sections(paragraphs);
    }
    if starts.first().copied() != Some(0) {
        starts.insert(0, 0);
    }

    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = starts.get(i + 1).copied().unwrap_or(paragraphs.len());
            paragraphs[start..end]
                .iter()
                .map(|paragraph| paragraph.text.trim())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .collect()
}

fn windowed_sections(paragraphs: &[XmlParagraph]) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    for paragraph in paragraphs {
        let text = paragraph.text.trim();
        if text.is_empty() {
            continue;
        }
        if !current.is_empty() && current.len() + text.len() + 2 > MAX_CHUNK_BYTES {
            sections.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(text);
    }
    if !current.is_empty() {
        sections.push(current);
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    /// Builds a minimal valid docx byte-for-byte in the test itself: a zip
    /// archive with a single `word/document.xml` entry. Real docx files also
    /// carry `[Content_Types].xml` and `_rels` parts, but this chunker never
    /// reads them, so the fixture omits them.
    ///
    /// `paragraphs` is `(text, style)` pairs; `style` becomes the
    /// paragraph's `w:pStyle` value (e.g. `Some("Heading1")`), or `None` for
    /// an unstyled paragraph.
    fn build_docx(paragraphs: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut body = String::new();
        body.push_str(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
"#,
        );
        for (text, style) in paragraphs {
            body.push_str("<w:p>");
            if let Some(style) = style {
                body.push_str(&format!(r#"<w:pPr><w:pStyle w:val="{style}"/></w:pPr>"#));
            }
            body.push_str(&format!(
                r#"<w:r><w:t xml:space="preserve">{}</w:t></w:r>"#,
                xml_escape(text)
            ));
            body.push_str("</w:p>\n");
        }
        body.push_str("</w:body>\n</w:document>");
        build_zip(&[("word/document.xml", &body)])
    }

    /// Builds a minimal valid pptx: one `ppt/slides/slideN.xml` entry per
    /// slide, each carrying the given `a:t` text runs (already own
    /// paragraphs). An empty `&[]` slide has no text runs at all, exercising
    /// the "skip empty slides" path.
    fn build_pptx(slides: &[&[&str]]) -> Vec<u8> {
        let mut entries: Vec<(String, String)> = Vec::new();
        for (idx, runs) in slides.iter().enumerate() {
            let slide_number = idx + 1;
            let mut xml = String::new();
            xml.push_str(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
"#,
            );
            for run in *runs {
                xml.push_str(&format!(
                    r#"<p:sp><p:txBody><a:p><a:r><a:t>{}</a:t></a:r></a:p></p:txBody></p:sp>"#,
                    xml_escape(run)
                ));
            }
            xml.push_str("</p:spTree></p:cSld>\n</p:sld>");
            entries.push((format!("ppt/slides/slide{slide_number}.xml"), xml));
        }
        let refs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(name, xml)| (name.as_str(), xml.as_str()))
            .collect();
        build_zip(&refs)
    }

    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn build_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, content) in entries {
            writer.start_file(*name, options).expect("start zip entry");
            std::io::Write::write_all(&mut writer, content.as_bytes()).expect("write zip entry");
        }
        writer.finish().expect("finish zip archive").into_inner()
    }

    #[test]
    fn docx_claims_extension_with_zip_magic() {
        let bytes = build_docx(&[("hello", None)]);
        assert!(DocxChunker.claims(Path::new("doc.docx"), &bytes));
        assert!(!DocxChunker.claims(Path::new("doc.txt"), &bytes));
        assert!(!DocxChunker.claims(Path::new("doc.docx"), b"not a zip"));
    }

    #[test]
    fn pptx_claims_extension_with_zip_magic() {
        let bytes = build_pptx(&[&["hello"]]);
        assert!(PptxChunker.claims(Path::new("deck.pptx"), &bytes));
        assert!(!PptxChunker.claims(Path::new("deck.txt"), &bytes));
        assert!(!PptxChunker.claims(Path::new("deck.pptx"), b"not a zip"));
    }

    #[test]
    fn heading_styled_docx_produces_sectioned_chunks() {
        let bytes = build_docx(&[
            ("Intro line", None),
            ("Section One", Some("Heading1")),
            ("Body of section one.", None),
            ("Section Two", Some("Heading2")),
            ("Body of section two.", None),
        ]);
        let (chunks, edges) = DocxChunker
            .chunk(Path::new("doc.docx"), &bytes)
            .expect("well-formed fixture docx must not error");
        assert!(edges.is_empty());
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chunk_kind == "office_section")
        );
        assert!(chunks[0].content.contains("Intro line"));
        assert!(!chunks[0].content.contains("Section One"));
        assert!(chunks[1].content.contains("Section One"));
        assert!(chunks[1].content.contains("Body of section one."));
        assert!(chunks[2].content.contains("Section Two"));
        assert!(chunks[2].content.contains("Body of section two."));
        assert!(chunks[0].language.is_none());
    }

    #[test]
    fn unstyled_docx_falls_back_to_a_single_windowed_section() {
        let bytes = build_docx(&[("Paragraph one.", None), ("Paragraph two.", None)]);
        let (chunks, _) = DocxChunker
            .chunk(Path::new("doc.docx"), &bytes)
            .expect("well-formed fixture docx must not error");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_kind, "office_section");
        assert!(chunks[0].content.contains("Paragraph one."));
        assert!(chunks[0].content.contains("Paragraph two."));
    }

    #[test]
    fn two_slide_pptx_with_one_empty_produces_one_slide_chunk() {
        let bytes = build_pptx(&[&["Slide one title", "Slide one body"], &[]]);
        let (chunks, edges) = PptxChunker
            .chunk(Path::new("deck.pptx"), &bytes)
            .expect("well-formed fixture pptx must not error");
        assert!(edges.is_empty());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_kind, "slide");
        assert_eq!(chunks[0].line_start, Some(1));
        assert_eq!(chunks[0].line_end, Some(1));
        assert!(chunks[0].content.contains("Slide one title"));
        assert!(chunks[0].content.contains("Slide one body"));
        assert!(chunks[0].language.is_none());
    }

    #[test]
    fn garbage_bytes_produce_no_chunks_and_do_not_panic() {
        let garbage = b"this is not a docx or pptx, just bytes \x00\x01\x02 garbage";
        let (docx_chunks, docx_edges) = DocxChunker
            .chunk(Path::new("doc.docx"), garbage)
            .expect("chunk() must never return Err for malformed input");
        assert!(docx_chunks.is_empty());
        assert!(docx_edges.is_empty());

        let (pptx_chunks, pptx_edges) = PptxChunker
            .chunk(Path::new("deck.pptx"), garbage)
            .expect("chunk() must never return Err for malformed input");
        assert!(pptx_chunks.is_empty());
        assert!(pptx_edges.is_empty());
    }

    #[test]
    fn zip_that_is_not_ooxml_produces_no_chunks() {
        // A structurally valid zip archive that simply isn't a docx/pptx
        // (no word/document.xml, no ppt/slides/*.xml).
        let bytes = build_zip(&[("readme.txt", "just a plain zip, not office")]);
        let (docx_chunks, _) = DocxChunker
            .chunk(Path::new("doc.docx"), &bytes)
            .expect("valid-but-foreign zip must not error");
        assert!(docx_chunks.is_empty());

        let (pptx_chunks, _) = PptxChunker
            .chunk(Path::new("deck.pptx"), &bytes)
            .expect("valid-but-foreign zip must not error");
        assert!(pptx_chunks.is_empty());
    }

    #[test]
    fn empty_byte_stream_produces_no_chunks_and_does_not_panic() {
        let (docx_chunks, _) = DocxChunker
            .chunk(Path::new("doc.docx"), b"")
            .expect("chunk() must never return Err for empty input");
        assert!(docx_chunks.is_empty());

        let (pptx_chunks, _) = PptxChunker
            .chunk(Path::new("deck.pptx"), b"")
            .expect("chunk() must never return Err for empty input");
        assert!(pptx_chunks.is_empty());
    }
}
