---
title: "Agentic Corpus \u2014 Multimodal Chunkers"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
---

# Agentic Corpus — Multimodal Chunkers

Status: all seven phases have shipped at least a first pass (X-PDF,
X-IPYNB, X-XLSX, X-DOCX-PPTX, X-HTML, X-AV, X-IMG), and X-PDF's
`pdf_figure` visual sidecar has also shipped (easy encodings only:
DCTDecode/JPEG passthrough and unpredicted FlateDecode DeviceGray/RGB;
JPXDecode, CCITTFax, and predictor-compressed streams remain out of
scope). `pdf_table` and per-format edges are the remaining named
deferrals - see each phase's section below.
Related: `design/corpus/agentic-corpus/agentic-corpus.md` §7.3 (chunker registry),
`design/corpus/agentic-corpus/agentic-corpus-impl.md` (X-* markers section),
`design/corpus/agentic-corpus/multimodal-embedding-routing.md` (embedding routing /
model selection — separate concern).

## Thesis

The `S2`/`S3` chunker registry that landed during the agentic-corpus
foundation phases is format-extensible by design — each new chunker
just registers an extractor and emits typed chunks + edges into the
existing EdgeIndex. Seven format chunkers were marked `[marker]` in
the impl skeleton (deferred placeholders, not partially-done work) and
are now spun out here so they can be deliberated independently.

Embedding-side concerns (visual model selection, per-bucket routing
for image/page content) live in `multimodal-embedding-routing.md`.
This doc is about the **chunker phases**: parsing, chunk granularity,
and the edge families each format contributes.

## Phases (deferred; ordering opportunistic)

Original priority from the impl skeleton: PDF first; HTML / AV / IMG
late.

### X-PDF — PDF chunker

Text-first pass **shipped 2026-07-11** (`crates/bbox-chunker/src/pdf.rs`):
`pdf-extract` per-page extraction into `pdf_page` chunks (1-based page
number carried in the position fields), `.pdf` admitted through the
project-file walker with a magic-header claim, docs-bucket routing, and
graceful degradation (encrypted/scanned/corrupt PDFs yield zero chunks and
a warning, never a failed reindex pass).

OCR fallback **shipped 2026-07-11** (same file, `mod ocr`): pages with no
extractable text rasterize through `pdftoppm` (poppler, 200 DPI grayscale)
and recognize through `tesseract`, both as availability-gated shell-outs
(silently disabled when either binary is off PATH; the text-first behavior
is unchanged there). Budgeted per design: per-child kill-on-timeout (60s
per rasterize batch of up to 10 contiguous pages, 30s per page
recognition), a 300s per-document wall-clock budget, and a 100-page OCR
cap. When text extraction fails wholesale (exotic encodings), OCR probes
from page 1 and stops at the first batch that renders nothing. OCR-sourced
chunks stay `pdf_page` (Docs bucket) and carry `symbol = "ocr"` as a
queryable provenance marker. Validated against real scanned documents
(single and multi-page, 1-3s per document). Still deferred: `pdf_table`
(pdf-extract's flat text discards layout; a text-only table detector would
misfire) and per-format edges; `pdf_figure` shipped the same day, below.

`pdf_figure` **shipped 2026-07-11** (`crates/bbox-chunker/src/pdf_figure.rs`,
a separate module - `PdfChunker::chunk` calls it and appends the result,
the only coupling to the text-extraction path in `pdf.rs`): embedded
raster XObjects extracted via `lopdf`'s page/XObject reading API
(`Document::get_page_images`), one `pdf_figure` chunk per extracted image
(page number in the position fields, same convention as `pdf_page`),
routed into the visual payload sidecar and the `voyage_visual` embedding
route (see `multimodal-embedding-routing.md` Layer 4). `lopdf` added zero
new dependency-tree weight: `pdf-extract` already resolves it (confirmed
via `git diff Cargo.lock` showing only the new sidecar crate as an
addition), and `default-features = false` keeps its `embed_image`
feature (which would pull in the `image` crate) off - only the
image-*reading* API is used, never the XObject-*creation* helpers. Only
the "easy encodings" are handled, matching the multimodal design's
pixel-export/effort bar: `DCTDecode` (baseline JPEG) passes through
unchanged; `FlateDecode` with 8-bit `DeviceGray`/`DeviceRGB` and no
PNG/TIFF predictor decompresses to raw scanlines and is re-encoded into a
minimal hand-built PNG (signature + IHDR + one IDAT + IEND, via `flate2`
and `crc32fast` - both already transitively present, so likewise zero new
dependency weight). Everything else (JPXDecode/JPEG2000, CCITTFax,
indexed/ICC/CMYK color spaces, 16-bit/1-bit components, predictor-
compressed Flate streams) is skipped rather than mis-decoded. `FIGURE_OF`
edges remain unbuilt (chunkers emit no edges today, matching every other
phase in this doc).

Original scope:

- `pdf-extract` for text PDFs; `tesseract` shell-out for scanned PDFs.
- Chunk types: `pdf_page`, `pdf_figure`, `pdf_table`.
- Edges: `ON_PAGE`, `FIGURE_OF`, `TABLE_OF`, `CITATION_TO`.
- Text-first ship is acceptable per `multimodal-embedding-routing.md`
  ("X-PDF can ship text-first without selecting a visual embedding
  model").

### X-IPYNB — Jupyter notebook chunker

Shipped 2026-07-11 (`crates/bbox-chunker/src/ipynb.rs`): one
`notebook_cell` chunk per non-empty cell (nbformat v4), cell index in the
position fields, kernel language on code cells (which routes them to the
Code bucket), text/plain and stream outputs appended truncated (2KB cap),
binary outputs skipped. Cell-adjacency is covered by the indexer's
derived `NEXT_SECTION` edges; `OUTPUT_OF`/`IMPORTS_FROM_CELL` remain
unbuilt (chunkers emit no edges today).

Original scope:

- Cell-level chunks with cell index + outputs.
- Edges: `NEXT_CELL`, `OUTPUT_OF`, `IMPORTS_FROM_CELL`.

### X-XLSX — Spreadsheet chunker

Shipped 2026-07-11 (`crates/bbox-chunker/src/xlsx.rs`): one
`spreadsheet_sheet` chunk per non-empty sheet across the whole calamine
family (.xlsx/.xlsm/.xlam/.xlsb/.xls/.ods), bounded TSV projection (200
rows / 16KB with truncation markers), formulas rendered inline as
`value [=FORMULA]`, sheet name in `symbol`. Formula dependency edges
remain unbuilt.

Original scope:

- `calamine` crate. Sheet-level + cell-range chunks.
- Edges: `IN_SHEET`, `COMPUTED_FROM` (formula deps), `CELL_REFERENCES`.

### X-DOCX-PPTX — Office documents chunker

Text-first pass **shipped 2026-07-11** (`crates/bbox-chunker/src/office.rs`):
`.docx` parsed into `office_section` chunks (split on `Heading1`-`Heading3`
styled paragraphs when present, else windowed at `MAX_CHUNK_BYTES`), `.pptx`
parsed into one `slide` chunk per non-empty slide (slide number carried in the
position fields the same way `pdf_page` carries page numbers). Both formats
admitted through the project-file walker with a ZIP-local-file-header magic
claim and a binary-gate exemption (they're zip containers, same story as
`.pdf`), and graceful degradation (encrypted/OLE2, corrupt, or non-OOXML zip
files yield zero chunks and a warning, never a failed reindex pass). Parsed
directly (the `zip` crate + `quick-xml`) rather than `docx-rs` / a pptx crate:
those are thin, sparsely maintained wrappers over the same "unzip + read
w:t/a:t runs" primitive this needs. Deliberately deferred from this pass: the
`IN_SECTION`/`ON_SLIDE`/`IN_DECK` edges below (the chunker registry emits zero
edges today across every format; `NEXT_SECTION` is derived generically by the
indexer over consecutive chunks regardless of `chunk_kind`), and legacy binary
`.doc`/`.ppt` (OLE2/CFBF, a different format entirely, out of scope).

Original scope:

- `docx-rs` / `pptx` parser.
- Edges: `IN_SECTION`, `ON_SLIDE`, `IN_DECK`.

### X-HTML — HTML / web archive chunker

Shipped 2026-07-11 (`crates/bbox-chunker/src/html.rs`): `web_section`
chunks split on h1-h3 (windowed `web_text` fallback), script/style/nav/
footer/aside/head stripped, scraper-based. Registered AHEAD of the code
chunker: .html previously fell through to tree-sitter code chunking (a
live misroute this phase fixed, with a precedence regression test).
Link/frame edges remain unbuilt.

Original scope:

- `scraper` crate.
- Edges: `LINKS_TO_URL`, `EMBEDS_FRAME`.

### X-AV — Audio/video transcript chunker

Shipped 2026-07-11 (`crates/bbox-chunker/src/av_transcript.rs`): .vtt and
.srt transcript files parse into merged `transcript_segment` chunks
(1KB windows, floor-second start/end in the position fields, speaker
voice tags kept as `Speaker:` prefixes, inline markup stripped, malformed
cues skipped individually). Running whisper is out of scope by design;
this chunker consumes external transcript files.

Original scope:

- Time-segmented chunks from external transcript producers (whisper).
- Edges: `AT_TIMESTAMP`, `IN_RECORDING`.

### X-IMG — Standalone image chunker

Shipped 2026-07-11 (`crates/bbox-chunker/src/ximg.rs`): claims
`.png`/`.jpg`/`.jpeg`/`.gif`/`.webp` by extension + a bounded magic-byte
scan (PNG/JPEG/GIF/WebP header signatures), admitted through the
project-file walker with a binary-gate exemption and a dedicated size cap
(`MAX_IMAGE_FILE_BYTES`, 20 MB - matched to the multimodal provider's own
per-image byte limit so an oversized file never reaches the payload
store). Emits exactly one `image` chunk whose only lexical content is the
file name stem; VLM caption extraction stayed out of scope as designed:
the file stem is the chunk's entire text. Raw bytes go into the
content-hash-addressed visual payload sidecar (`bbox-visual-store`, new
crate, outside tantivy - see `multimodal-embedding-routing.md`
"Multimodal Chunk Model" below), and the resulting `VisualPayloadRef`
rides the chunk two ways: directly on a new `Chunk::visual_payload` field
(consumed by the index-time enqueue path) and encoded into `Chunk::symbol`
(consumed by the backfill/reembed path, which reconstructs a `Chunk` from
stored tantivy fields rather than re-chunking the file - `symbol` is an
existing plain-text field, so this needed no schema bump). `DEPICTS`/
`CAPTIONED_AS` edges remain unbuilt, matching every other chunker phase.

Original scope:

- Embed the image directly with `voyage-multimodal-3.5`; the model
  selection is resolved in `multimodal-embedding-routing.md` (Layer 4).
  VLM caption extraction is optional lexical enrichment, not a gate.
- Edges: `DEPICTS`, `CAPTIONED_AS`.

## Picking the next one

The chunker registry contract is stable; each phase is additive and
none block the others. Open questions when picking:

- Does the format unlock a high-value corpus already on this host
  (PDF research papers, Jupyter notebooks from a data project)?
- Does the format need a new embedding route, or can it ride existing
  text routes (text-first PDF, HTML extracted text → existing `docs`
  bucket)?
- Does the format require a heavyweight external dep (tesseract, VLM
  for X-IMG, whisper for X-AV)? Cheaper phases first when the corpus
  doesn't already demand them.
