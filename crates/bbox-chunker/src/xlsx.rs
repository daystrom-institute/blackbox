use std::io::Cursor;
use std::path::Path;

use anyhow::Result;
use calamine::{Data, Range, Reader, Sheets, open_workbook_auto_from_rs};

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

/// `.xlsx`/`.xlsm`/`.xlam`/`.xlsb`/`.ods` are all ZIP containers (local file
/// header magic `PK\x03\x04`, per the ZIP spec, always at byte 0 since these
/// producers don't prepend junk the way some PDF producers do); `.xls` is an
/// OLE2 compound file (`D0 CF 11 E0 A1 B1 1A E1`).
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
const OLE2_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Extensions claimed by this chunker. The X-XLSX task scope named `.xlsx`
/// as primary and asked to decide on `.xls`/`.ods` "if calamine makes it
/// free": `calamine::open_workbook_auto_from_rs` auto-detects the concrete
/// format from the bytes themselves (it is not dispatched by extension), so
/// `.xls`, `.xlsb`, `.ods`, and the xlsx siblings `.xlsm`/`.xlam` all ride
/// the exact same parse call as `.xlsx` — there is no extra format-specific
/// code to write, only the extension/magic gate below. Decision: claim the
/// whole family calamine covers rather than `.xlsx` alone.
const ZIP_CONTAINER_EXTENSIONS: &[&str] = &["xlsx", "xlsm", "xlam", "xlsb", "ods"];
const OLE2_CONTAINER_EXTENSIONS: &[&str] = &["xls"];

/// Row cap per sheet chunk (spec: "cap rows per sheet ~200").
const MAX_SHEET_ROWS: usize = 200;

/// Character cap per sheet chunk (spec: "total chars per chunk ~16KB").
/// This sits above `crate::MAX_CHUNK_BYTES` (12KB): a sheet chunk that fills
/// this 16KB budget can still get mechanically re-split into multiple
/// sub-chunks by `bound_chunks` / `split_oversized_chunk`
/// (`crates/bbox-corpus-index/src/index/project_files.rs`), which is
/// existing chunker-agnostic pipeline behavior applied to any oversized
/// chunk from any chunker, not something this chunker needs to prevent.
const MAX_SHEET_CHUNK_CHARS: usize = 16 * 1024;

/// Spreadsheet chunker (X-XLSX,
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
/// One `spreadsheet_sheet` chunk per non-empty sheet: a bounded TSV-ish
/// projection of the sheet's used range, formula text rendered inline next
/// to the value it produces when calamine exposes it cheaply
/// (`Reader::worksheet_formula`). No dependency-graph edges are built from
/// formulas in this pass (`COMPUTED_FROM`/`CELL_REFERENCES` from the design
/// doc are out of scope); `chunk()` returns zero edges like `pdf.rs` and
/// `markdown.rs` do today.
///
/// Corrupt, encrypted, or otherwise unreadable workbooks degrade to zero
/// chunks rather than an error, mirroring `pdf.rs`: `SourceFormatChunker::
/// chunk` returning `Err` aborts the entire background reindex pass, not
/// just this file, so extraction failures are caught and swallowed here.
pub struct XlsxChunker;

impl SourceFormatChunker for XlsxChunker {
    fn format_id(&self) -> &str {
        "xlsx"
    }

    fn claims(&self, path: &Path, sniff: &[u8]) -> bool {
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            return false;
        };
        let ext = ext.to_ascii_lowercase();
        if ZIP_CONTAINER_EXTENSIONS.contains(&ext.as_str()) {
            sniff.starts_with(ZIP_MAGIC)
        } else if OLE2_CONTAINER_EXTENSIONS.contains(&ext.as_str()) {
            sniff.starts_with(OLE2_MAGIC)
        } else {
            false
        }
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        Ok((extract_sheet_chunks(path, bytes), Vec::new()))
    }
}

/// Extract one `spreadsheet_sheet` chunk per non-empty sheet. Empty sheets
/// (no used cells) are skipped rather than emitted blank.
///
/// No dedicated sheet-name field exists on `Chunk`; `symbol` (otherwise used
/// for code symbol names, unused by any non-code_block edge derivation, see
/// `crates/bbox-corpus-index/src/index/project_files.rs::build_symbol_table`)
/// carries the sheet name, and `line_start`/`line_end` carry the 1-based
/// sheet ordinal — the same "non-line-oriented source, reuse the position
/// slots" convention `pdf.rs` uses for page number.
fn extract_sheet_chunks(path: &Path, bytes: &[u8]) -> Vec<Chunk> {
    // calamine's zip/OLE2 parsing has not been audited against adversarial
    // inputs; wrap the whole open+iterate pass in catch_unwind on top of its
    // own Result (mirrors pdf.rs's posture) so a corrupt or adversarial
    // workbook can never panic the indexing pass.
    let extraction = std::panic::catch_unwind(|| open_and_extract(bytes));
    let sheets = match extraction {
        Ok(Ok(sheets)) => sheets,
        Ok(Err(err)) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "spreadsheet extraction failed (corrupt, encrypted, or unsupported structure); skipping"
            );
            return Vec::new();
        }
        Err(_) => {
            tracing::warn!(
                path = %path.display(),
                "spreadsheet extraction panicked; skipping"
            );
            return Vec::new();
        }
    };

    let mut chunks = Vec::new();
    let mut byte_offset = 0u64;
    for (idx, (name, text)) in sheets.into_iter().enumerate() {
        let byte_start = byte_offset;
        let byte_end = byte_start + text.len() as u64;
        let mut chunk = placeholder_chunk(
            path,
            "spreadsheet_sheet",
            None,
            text,
            byte_start,
            byte_end,
            chunks.len() as u32,
        );
        let sheet_number = (idx + 1) as u32;
        chunk.line_start = Some(sheet_number);
        chunk.line_end = Some(sheet_number);
        chunk.symbol = Some(name);
        byte_offset = byte_end + 1;
        chunks.push(chunk);
    }
    chunks
}

/// Open the workbook (any of the formats `claims()` admits, auto-detected
/// from the bytes) and render each non-empty sheet to `(sheet_name, text)`.
/// A single unreadable sheet is warned-and-skipped rather than failing the
/// whole workbook, so one corrupt sheet in an otherwise-good file doesn't
/// discard every other sheet's chunks.
fn open_and_extract(bytes: &[u8]) -> Result<Vec<(String, String)>, calamine::Error> {
    let mut workbook: Sheets<Cursor<&[u8]>> = open_workbook_auto_from_rs(Cursor::new(bytes))?;
    let mut out = Vec::new();
    for name in workbook.sheet_names() {
        let values = match workbook.worksheet_range(&name) {
            Ok(range) => range,
            Err(err) => {
                tracing::warn!(
                    sheet = %name,
                    error = %err,
                    "failed to read worksheet range; skipping sheet"
                );
                continue;
            }
        };
        if values.is_empty() {
            continue;
        }
        // Formulas are best-effort enrichment, not a gate: formats/sheets
        // that don't expose them (or error reading them) still render the
        // plain value grid.
        let formulas = workbook.worksheet_formula(&name).ok();
        if let Some(text) = render_sheet(&name, &values, formulas.as_ref()) {
            out.push((name, text));
        }
    }
    Ok(out)
}

/// Render a sheet's used range to a bounded TSV-ish text block: a `# Sheet:
/// <name>` header line followed by tab-joined rows, each cell rendered via
/// `Data`'s `Display` impl with the cell's formula (when present) appended
/// as `[=FORMULA]`. Rows beyond `MAX_SHEET_ROWS`, or content beyond
/// `MAX_SHEET_CHUNK_CHARS`, stop early behind an explicit truncation marker
/// line. Returns `None` for a used range that is entirely blank cells
/// (calamine's "used range" can be wider than the cells that actually carry
/// content), so such sheets are skipped like a genuinely empty sheet.
fn render_sheet(
    name: &str,
    values: &Range<Data>,
    formulas: Option<&Range<String>>,
) -> Option<String> {
    let value_start = values.start()?;
    let formula_start = formulas.and_then(Range::start);

    let mut out = String::new();
    out.push_str("# Sheet: ");
    out.push_str(name);
    out.push('\n');

    let total_rows = values.height();
    let mut rows_emitted = 0usize;
    let mut any_content = false;
    let mut truncated = false;

    for (row_idx, row) in values.rows().enumerate() {
        if row_idx >= MAX_SHEET_ROWS {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(row.len());
        for (col_idx, cell) in row.iter().enumerate() {
            let mut text = cell.to_string();
            if let Some(formula) =
                formula_at(formulas, formula_start, value_start, row_idx, col_idx)
            {
                text = if text.is_empty() {
                    format!("[={formula}]")
                } else {
                    format!("{text} [={formula}]")
                };
            }
            cells.push(text);
        }
        let line = cells.join("\t");
        if !line.trim().is_empty() {
            any_content = true;
        }
        if out.len() + line.len() + 1 > MAX_SHEET_CHUNK_CHARS {
            truncated = true;
            break;
        }
        out.push_str(&line);
        out.push('\n');
        rows_emitted += 1;
    }

    if !any_content {
        return None;
    }
    if truncated {
        out.push_str(&format!(
            "... [truncated: showing {rows_emitted} of {total_rows} rows]\n"
        ));
    }
    Some(out)
}

/// Look up the formula text (if any) for the value-range cell at relative
/// `(row_idx, col_idx)`, translating through absolute sheet coordinates
/// since the value range and the formula range are independently sparse and
/// so generally don't share the same top-left origin.
fn formula_at(
    formulas: Option<&Range<String>>,
    formula_start: Option<(u32, u32)>,
    value_start: (u32, u32),
    row_idx: usize,
    col_idx: usize,
) -> Option<String> {
    let formulas = formulas?;
    let (formula_row0, formula_col0) = formula_start?;
    let abs_row = value_start.0 as usize + row_idx;
    let abs_col = value_start.1 as usize + col_idx;
    let rel_row = abs_row.checked_sub(formula_row0 as usize)?;
    let rel_col = abs_col.checked_sub(formula_col0 as usize)?;
    let formula = formulas.get((rel_row, rel_col))?;
    if formula.is_empty() {
        None
    } else {
        Some(formula.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;

    /// Assemble a minimal, spec-valid `.xlsx` (a ZIP of the handful of XML
    /// parts OOXML requires) directly in the test, byte for byte, so there
    /// is no checked-in binary fixture to go stale and no build-time
    /// dependency beyond the `zip` crate calamine itself already pulls in.
    /// `sheets` is `(name, rows)` where each row is a list of either plain
    /// values or `("=FORMULA", "cached-value")` pairs rendered as an
    /// OOXML `<f>`/`<v>` cell.
    enum SheetCell {
        Value(&'static str),
        Formula {
            formula: &'static str,
            cached: &'static str,
        },
    }

    fn build_xlsx(sheets: &[(&str, Vec<Vec<SheetCell>>)]) -> Vec<u8> {
        fn col_letter(mut idx: usize) -> String {
            let mut letters = Vec::new();
            idx += 1;
            while idx > 0 {
                let rem = (idx - 1) % 26;
                letters.push((b'A' + rem as u8) as char);
                idx = (idx - 1) / 26;
            }
            letters.iter().rev().collect()
        }

        fn sheet_xml(rows: &[Vec<SheetCell>]) -> String {
            let mut body = String::new();
            for (row_idx, row) in rows.iter().enumerate() {
                let r = row_idx + 1;
                body.push_str(&format!("<row r=\"{r}\">"));
                for (col_idx, cell) in row.iter().enumerate() {
                    let coord = format!("{}{r}", col_letter(col_idx));
                    match cell {
                        SheetCell::Value(v) => {
                            if let Ok(_n) = v.parse::<f64>() {
                                body.push_str(&format!("<c r=\"{coord}\"><v>{v}</v></c>"));
                            } else {
                                body.push_str(&format!(
                                    "<c r=\"{coord}\" t=\"str\"><v>{v}</v></c>"
                                ));
                            }
                        }
                        SheetCell::Formula { formula, cached } => {
                            body.push_str(&format!(
                                "<c r=\"{coord}\"><f>{formula}</f><v>{cached}</v></c>"
                            ));
                        }
                    }
                }
                body.push_str("</row>");
            }
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
                 <worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
                 <sheetData>{body}</sheetData></worksheet>"
            )
        }

        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();

            zip.start_file("[Content_Types].xml", opts).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
"#,
            )
            .unwrap();
            for (idx, _) in sheets.iter().enumerate() {
                zip.write_all(format!(
                    "<Override PartName=\"/xl/worksheets/sheet{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\n",
                    idx + 1
                ).as_bytes()).unwrap();
            }
            zip.write_all(b"</Types>").unwrap();

            zip.start_file("_rels/.rels", opts).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
            )
            .unwrap();

            zip.start_file("xl/_rels/workbook.xml.rels", opts).unwrap();
            let mut rels = String::from(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
                 <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
            );
            for (idx, _) in sheets.iter().enumerate() {
                rels.push_str(&format!(
                    "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{}.xml\"/>",
                    idx + 1,
                    idx + 1
                ));
            }
            rels.push_str("</Relationships>");
            zip.write_all(rels.as_bytes()).unwrap();

            zip.start_file("xl/workbook.xml", opts).unwrap();
            let mut wb = String::from(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
                 <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
                 xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>",
            );
            for (idx, (name, _)) in sheets.iter().enumerate() {
                wb.push_str(&format!(
                    "<sheet name=\"{name}\" sheetId=\"{}\" r:id=\"rId{}\"/>",
                    idx + 1,
                    idx + 1
                ));
            }
            wb.push_str("</sheets></workbook>");
            zip.write_all(wb.as_bytes()).unwrap();

            for (idx, (_, rows)) in sheets.iter().enumerate() {
                zip.start_file(format!("xl/worksheets/sheet{}.xml", idx + 1), opts)
                    .unwrap();
                zip.write_all(sheet_xml(rows).as_bytes()).unwrap();
            }

            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    fn text_row(values: &[&'static str]) -> Vec<SheetCell> {
        values.iter().map(|v| SheetCell::Value(v)).collect()
    }

    #[test]
    fn claims_xlsx_extension_with_zip_magic() {
        let bytes = build_xlsx(&[("Sheet1", vec![text_row(&["hello"])])]);
        assert!(XlsxChunker.claims(Path::new("book.xlsx"), &bytes));
        assert!(!XlsxChunker.claims(Path::new("book.txt"), &bytes));
        assert!(!XlsxChunker.claims(Path::new("book.xlsx"), b"not a zip"));
    }

    #[test]
    fn does_not_claim_unsupported_extension() {
        let bytes = build_xlsx(&[("Sheet1", vec![text_row(&["hello"])])]);
        assert!(!XlsxChunker.claims(Path::new("book.docx"), &bytes));
    }

    #[test]
    fn two_sheets_produce_two_chunks_with_names_and_kind() {
        let bytes = build_xlsx(&[
            (
                "Revenue",
                vec![text_row(&["Q1", "100"]), text_row(&["Q2", "200"])],
            ),
            ("Notes", vec![text_row(&["free text"])]),
        ]);
        let (chunks, edges) = XlsxChunker
            .chunk(Path::new("book.xlsx"), &bytes)
            .expect("well-formed fixture workbook must not error");
        assert!(edges.is_empty());
        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].chunk_kind, "spreadsheet_sheet");
        assert_eq!(chunks[0].symbol.as_deref(), Some("Revenue"));
        assert_eq!(chunks[0].line_start, Some(1));
        assert_eq!(chunks[0].line_end, Some(1));
        assert!(chunks[0].content.contains("Q1"));
        assert!(chunks[0].content.contains("100"));
        assert!(chunks[0].language.is_none());

        assert_eq!(chunks[1].chunk_kind, "spreadsheet_sheet");
        assert_eq!(chunks[1].symbol.as_deref(), Some("Notes"));
        assert_eq!(chunks[1].line_start, Some(2));
        assert_eq!(chunks[1].line_end, Some(2));
        assert!(chunks[1].content.contains("free text"));
    }

    #[test]
    fn formula_is_rendered_inline_next_to_cached_value() {
        let bytes = build_xlsx(&[(
            "Sheet1",
            vec![
                text_row(&["10"]),
                text_row(&["20"]),
                vec![SheetCell::Formula {
                    formula: "SUM(A1:A2)",
                    cached: "30",
                }],
            ],
        )]);
        let (chunks, _) = XlsxChunker
            .chunk(Path::new("book.xlsx"), &bytes)
            .expect("well-formed fixture workbook must not error");
        assert_eq!(chunks.len(), 1);
        assert!(
            chunks[0].content.contains("30 [=SUM(A1:A2)]"),
            "content was: {}",
            chunks[0].content
        );
    }

    #[test]
    fn garbage_byte_stream_produces_no_chunks_and_does_not_panic() {
        let garbage = b"this is not a workbook, just some random bytes \x00\x01\x02 garbage";
        let (chunks, edges) = XlsxChunker
            .chunk(Path::new("book.xlsx"), garbage)
            .expect("chunk() must never return Err for malformed input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn empty_byte_stream_produces_no_chunks_and_does_not_panic() {
        let (chunks, edges) = XlsxChunker
            .chunk(Path::new("book.xlsx"), b"")
            .expect("chunk() must never return Err for empty input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn empty_sheet_produces_no_chunk() {
        let bytes = build_xlsx(&[("Empty", vec![])]);
        let (chunks, edges) = XlsxChunker
            .chunk(Path::new("book.xlsx"), &bytes)
            .expect("well-formed fixture workbook must not error");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn row_cap_truncation_marker_appears_for_oversized_sheet() {
        let rows: Vec<Vec<SheetCell>> = (0..(MAX_SHEET_ROWS + 25))
            .map(|i| {
                text_row(&["x"])
                    .into_iter()
                    .chain(std::iter::once(SheetCell::Value(if i % 2 == 0 {
                        "even"
                    } else {
                        "odd"
                    })))
                    .collect()
            })
            .collect();
        let bytes = build_xlsx(&[("Big", rows)]);
        let (chunks, _) = XlsxChunker
            .chunk(Path::new("book.xlsx"), &bytes)
            .expect("well-formed fixture workbook must not error");
        assert_eq!(chunks.len(), 1);
        assert!(
            chunks[0].content.contains(&format!(
                "truncated: showing {MAX_SHEET_ROWS} of {}",
                MAX_SHEET_ROWS + 25
            )),
            "content tail was: {}",
            &chunks[0].content[chunks[0].content.len().saturating_sub(200)..]
        );
    }
}
