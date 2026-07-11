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

Status: proposed (deferred from agentic-corpus-impl X-* markers).
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
a warning, never a failed reindex pass). Deliberately deferred from that
pass: `pdf_table` (pdf-extract's flat text discards layout; a text-only
table detector would misfire), OCR shell-outs, `pdf_figure` + edges.

Original scope:

- `pdf-extract` for text PDFs; `tesseract` shell-out for scanned PDFs.
- Chunk types: `pdf_page`, `pdf_figure`, `pdf_table`.
- Edges: `ON_PAGE`, `FIGURE_OF`, `TABLE_OF`, `CITATION_TO`.
- Text-first ship is acceptable per `multimodal-embedding-routing.md`
  ("X-PDF can ship text-first without selecting a visual embedding
  model").

### X-IPYNB — Jupyter notebook chunker

- Cell-level chunks with cell index + outputs.
- Edges: `NEXT_CELL`, `OUTPUT_OF`, `IMPORTS_FROM_CELL`.

### X-XLSX — Spreadsheet chunker

- `calamine` crate. Sheet-level + cell-range chunks.
- Edges: `IN_SHEET`, `COMPUTED_FROM` (formula deps), `CELL_REFERENCES`.

### X-DOCX-PPTX — Office documents chunker

- `docx-rs` / `pptx` parser.
- Edges: `IN_SECTION`, `ON_SLIDE`, `IN_DECK`.

### X-HTML — HTML / web archive chunker

- `scraper` crate.
- Edges: `LINKS_TO_URL`, `EMBEDS_FRAME`.

### X-AV — Audio/video transcript chunker

- Time-segmented chunks from external transcript producers (whisper).
- Edges: `AT_TIMESTAMP`, `IN_RECORDING`.

### X-IMG — Standalone image chunker

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
