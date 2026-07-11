---
title: "Multimodal and Embedding Routing Design"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
---

# Multimodal and Embedding Routing Design

Date: 2026-07-09 (Voyage 4 alignment pass; supersedes the 2026-06-12
revision in-place)

## Problem

The embedding stack is well-partitioned but **single-model, role-unmarked,
and a generation old**. Re-verified against code and the live daemon on
2026-07-09:

- Every bucket — `code`, `docs`, `knowledge`, `notes`, `threads`,
  `git_message`, `agent_manifest` — embeds with `voyage-code-3`, a
  code-specialized model. The prose corpus has never been on a prose model.
- The provider trait is still `embed_batch(&[String])`
  (`crates/bbox-embed/src/embed/mod.rs`). No `input_type` is sent anywhere:
  stored chunks and live queries reach Voyage role-unmarked.
- Provider config is still hardcoded `ProviderConfigs { voyage, ollama }`,
  not an alias map. Multiple Voyage-backed routes with different models
  cannot be expressed.
- `Route` carries provider/model/dimensions only — no `endpoint_kind`,
  `compatibility_family`, or `output_dtype`. Dimensions are hardcoded
  consts (1024 voyage / 768 ollama); Matryoshka output dims are unused.
- No partition lifecycle tooling (list/prune) exists, which becomes urgent
  the moment any deliberate model migration orphans the current partitions.
- No multimodal code exists anywhere in embed/vectors/chunker.

Two things from the prior revision have shipped since:

- A process-wide query-embedding cache keyed `(provider_id, model, query)`
  (`crates/bbox-embed/src/embed/query_cache.rs`) — already
  compatibility-family-shaped.
- Ranking metrics (MRR / recall@k) and a sweepable rerank cap
  (`crates/bbox-corpus-core/src/search/{rerank,metrics}.rs`) — the eval
  substrate the migration phases below lean on.

Live coverage (2026-07-09, `bbox_embed_status`): knowledge, notes, threads,
and agent_manifest are healthy (99%+); code (32%), docs (11%), and
git_message (87%) report stalled partial coverage with idle queues.
Repair that residue before any model migration so backfill failures and
model-quality changes are not conflated. Transcripts stay guarded behind
the explicit `include_transcripts` opt-in.

This doc supersedes only the embedding-model selection, routing, and
ranking-pipeline parts of `agentic-corpus.md` / `agentic-corpus-impl.md`.
The chunker registry, bucket model, HNSW partitioning, and RRF fusion
strategy stay. Chunker phases live in
`agentic-corpus-multimodal-chunkers.md`.

## External Model Facts

Checked against Voyage documentation on 2026-07-09.

Text embeddings (`/v1/embeddings`):

- **voyage-4 family** — `voyage-4-large`, `voyage-4`, `voyage-4-lite`,
  `voyage-4-nano`. Explicit **shared embedding space across the family**:
  documents embedded with one 4-series model can be searched with queries
  embedded by another. Matryoshka output dims 256/512/1024/2048.
  Quantized output dtypes: `int8`, `uint8`, `binary`, `ubinary` alongside
  float.
- **voyage-4-nano is open-weight** (HuggingFace) — a local/offline fallback
  that stays inside the hosted family's vector space, strictly better for
  this corpus than an unrelated local model (`nomic-embed-text`).
- `voyage-code-3` remains the code-retrieval specialist; no documented
  compatibility with the voyage-4 space. Do not infer compatibility from
  equal dimensions. It supports the same 256/512/1024/2048 dimensions and
  quantized output options.
- Request limits: at most 1,000 inputs per request, with model-specific
  aggregate token caps (1M for `voyage-4-lite`, 320K for `voyage-4`, 120K
  for `voyage-4-large` and `voyage-code-3`). The current queue guard
  (128 inputs / 100 KiB) is a conservative byte heuristic well inside
  those caps, not an exact token check; keep it conservative rather than
  pretending bytes are tokens.

Contextualized chunk embeddings (`voyage-context-4`):

- `voyage-context-4` is now the current recommended contextualized model
  (GA, announced 2026-06-29); it is no longer preview. Voyage documents no
  vector-space compatibility between `voyage-context-3` and `-4`, so the
  version choice is a new partition and full re-embed either way. Adopt
  `-4` directly when Layer 2 lands.
- Each chunk is encoded in the context of the other chunks of the same
  document; chunk-level vectors, document-level semantics. 32K-token
  context per chunk.
- API takes document-grouped input (`List[List[str]]`); queries stay flat
  `List[str]` with `input_type="query"`. Same flexible dims
  (256/512/1024/2048) and output formats.
- Request caps: 1,000 inputs, 120K aggregate tokens, 16K chunks.
- Optional provider auto-chunking exists (`enable_auto_chunking`, off by
  default; `chunk_size` defaults to 512 tokens and caps at 32K;
  `chunk_overlap` defaults to 0 and must be smaller than chunk size).
  Blackbox's default remains locally chunked document groups, which keep
  stable entity refs, source anchors, and graph edges; see Layer 2.
- Own vector space: its own compatibility family, not interchangeable
  with voyage-4 standard embeddings.

Rerankers (`/v1/rerank`):

- `rerank-2.5` (quality, instruction-following) and `rerank-2.5-lite`
  (latency/cost) — cross-encoders, 32K combined query+doc tokens, query ≤
  8K tokens, ≤ 1,000 documents per call, ≤ 600K total tokens.

Multimodal (`/v1/multimodalembeddings`):

- `voyage-multimodal-3.5`: interleaved text / image / video parts
  (URL or base64, not mixed per request), `input_type=query|document`,
  output dims 256/512/**1024 default**/2048. Video is 3.5-only.
- Limits: image ≤ 20 MB and ≤ 16M pixels; video ≤ 20 MB; ≤ 32K tokens per
  input, ≤ 320K per batch, ≤ 1,000 inputs per request (560 px per image
  token, 1120 px per video token).
- `truncation` defaults to true, and when truncation lands inside an image
  the entire image is silently discarded. Visual routes must set
  `truncation=false` and preflight the limits locally; a dropped image is
  not an acceptable silent degradation for visual retrieval.
- The multimodal guide documents flexible dimensions but, unlike the text
  and contextualized docs, does not document quantized output dtypes.
  Treat multimodal quantization as unverified until probed.
- **No documented compatibility with the voyage-4 text space.** Multimodal
  is a separate route family, not a text-model replacement.

Primary references:

- https://docs.voyageai.com/docs/embeddings
- https://docs.voyageai.com/docs/contextualized-chunk-embeddings
- https://blog.voyageai.com/2026/06/29/voyage-context-4/
- https://docs.voyageai.com/docs/reranker
- https://docs.voyageai.com/docs/multimodal-embeddings
- https://docs.voyageai.com/reference/multimodal-embeddings-api

## Design Principles

1. Model compatibility is explicit, not dimension-derived.
2. Compatibility family includes **dtype**. Binary-quantized voyage-4
   vectors are not comparable with float voyage-4 vectors at the same
   dimension. Family = provider type + model family + dim + dtype.
3. Text-first chunkers must not block on multimodal embeddings.
4. Visual-native retrieval is a new route family, not a global replacement.
5. Query embedding is route-local, except where an explicit
   family rule allows **asymmetric retrieval** (voyage-4: embed documents
   with `voyage-4-large`, queries with `voyage-4-lite`/`-nano`).
6. Contextualized document encoding is the preferred default for chunked
   prose and code corpora once the queue can batch per-document; standard
   embeddings remain for buckets whose units are not document-grouped.
7. Ranking is a pipeline: BM25 + vector → RRF fusion → cross-encoder
   rerank of the fused top-k → heuristic adjustments (type/temporal)
   last. Raw cosine scores across unrelated families are never merged
   before rank normalization.
8. Corpus export policy remains bucket-scoped; multimodal makes
   image/video/PDF pixel export explicit and opt-in.

## Layered Target Architecture

Dependency-ordered. Layer 0 gates everything.

### Layer 0 — Routing substrate (unchanged prerequisite)

Provider alias map with a `type` discriminator, `input_type` on the
provider trait, and `endpoint_kind` + `compatibility_family` +
`output_dtype` in route metadata. Detailed below in Recommended Routing /
Compatibility Families / Provider Interface.

### Layer 1 — Re-route the prose corpus + asymmetric retrieval

`code` stays on `voyage-code-3` until the eval suite says otherwise.
All prose buckets (`knowledge`, `notes`, `threads`, `docs`,
`git_message`, `agent_manifest`, `transcripts`) move to the voyage-4
family. The family's shared-space guarantee enables asymmetric retrieval:
document embeddings on `voyage-4-large` (one-time indexing cost), query
embeddings on `voyage-4-lite` or local `voyage-4-nano` (per-search
latency/cost). Expressed as `document_model` / `query_model` within one
provider alias — both must derive the same compatibility family or the
route loader rejects the config. Note the asymmetry Voyage actually
guarantees: family compatibility permits cross-model search, it does not
make two query models' vectors identical. The query cache key therefore
stays exact, `(provider_id, query_model, dim, dtype, query)`; the family
governs which partitions a cached vector may search, never cache identity.

### Layer 2 — Contextualized chunk embeddings

`voyage-context-4` (GA since 2026-06-29) becomes the document encoder for
buckets whose chunks are document-grouped — `code`, `docs`, and
transcript sessions are the strongest fits; chunk-in-document context is
the known weakness of independent chunk embedding on exactly this corpus.
`code` moves only on an eval win against `voyage-code-3`; Voyage's
published aggregates do not establish that specialized comparison.

Queue implication (the real work): the embed queue currently batches flat
text across documents. Contextualized routes need a **document-grouped
batch boundary** — requests carry a document grouping key and the worker
assembles `Vec<Vec<String>>` per document, never splitting one document
across batches (subject to the per-batch token budget; oversized documents
fall back to windowed grouping). `EmbedInput` anticipates this with a
`DocumentChunks` variant from day one so Layer 0's shapes don't churn.

Provider auto-chunking is not the ingestion default. Local semantic chunks
keep stable entity refs, line/page/cell metadata, AST boundaries, and graph
edges; send them as ordered document groups. Auto-chunking is at most an
eval alternative for source kinds with a canonical whole-document text
projection, and only if the returned chunk text and settings can be
persisted alongside the vectors. It is never a chunker for raw binary
formats.

Queries against context partitions are flat text with `input_type=query` —
no query-side grouping, so hybrid search needs no structural change beyond
query-encoder-aware routing.

### Layer 3 — Cross-encoder rerank stage

The current "rerank" is a heuristic feature multiplier (type × temporal
decay, capped — `bbox-corpus-core/src/search/rerank.rs`). Insert a real
rerank stage: after RRF fusion, send the fused top-k (text content, k
≈ 50–100) to `rerank-2.5-lite` with the query; re-order by relevance
score; apply heuristic adjustments after (or fold them into the rerank
score as a small multiplier under the existing cap machinery). Opt-in per
call (`rerank="model"|"heuristic"|"none"`), heuristic remains the default
until the eval suite shows the win and the latency budget is accepted.
This is the designated instrument for the "learned ranking" ambition —
a hosted cross-encoder is cheaper and better calibrated than per-turn LLM
scoring (gap-85c45849), and the metrics substrate (MRR/recall@k) makes it
A/B-able from day one. Degradation rule: rerank API failure falls back to
the heuristic path and reports `degraded.rerank_unavailable`.

### Layer 4 — Multimodal route family (opt-in)

`voyage-multimodal-3.5` at 1024d as its own compatibility family. Visual
payload sidecars outside Tantivy, pixel-export policy, PDF text-first.
This resolves the X-IMG model-selection blocker (gap-d5bd0c66): the
selection is `voyage-multimodal-3.5`, 1024 float, family
`voyage-multimodal-3.5:1024:float`, no text-space sharing. Details in
Multimodal Chunk Model below; chunker phases in
`agentic-corpus-multimodal-chunkers.md`.

### Layer 5 — Partition lifecycle (before the migrations, not after)

Layers 1–2 are deliberate model migrations that orphan every current
partition. `bbox_embed_partitions(action="list|prune")` (or CLI
equivalent) lands first:

- `list` reports exact route id, provider, endpoint kind, model, dim,
  dtype, compatibility family, active_count, last_write, and whether any
  configured bucket currently maps to it.
- `prune` is dry-run by default and only deletes partitions that are both
  unmapped by current route config and older than an operator-supplied age
  threshold.
- Never auto-prune as part of `bbox_reembed`.

### Sequencing

Layer 0 → Layer 5 → Layer 1 → Layer 3 (independent of 1–2; may move
earlier) → Layer 2 → Layer 4 (demand-driven per the chunker doc's
"picking the next one" criteria).

## Recommended Routing

Replace hardcoded `ProviderConfigs { voyage, ollama }` with a map keyed by
provider alias:

```toml
[embed.providers.voyage_code]
type = "voyage_text"
api_key_env = "VOYAGE_API_KEY"
model = "voyage-code-3"
output_dimension = 1024

[embed.providers.voyage_text]
type = "voyage_text"
api_key_env = "VOYAGE_API_KEY"
document_model = "voyage-4-large"   # one-time indexing cost
query_model = "voyage-4-lite"       # per-search cost; same family enforced
output_dimension = 1024

[embed.providers.voyage_context]
type = "voyage_context"
api_key_env = "VOYAGE_API_KEY"
model = "voyage-context-4"
output_dimension = 1024

[embed.providers.voyage_visual]
type = "voyage_multimodal"
api_key_env = "VOYAGE_API_KEY"
model = "voyage-multimodal-3.5"
output_dimension = 1024

[embed.providers.local_nano]
type = "local_voyage4"              # open-weight voyage-4-nano; same family
model = "voyage-4-nano"
output_dimension = 1024

[embed.providers.ollama]
type = "ollama"
endpoint = "http://localhost:11434"
model = "nomic-embed-text"

[embed.rerank]
provider = "voyage"
model = "rerank-2.5-lite"
api_key_env = "VOYAGE_API_KEY"
top_k = 64

[embed.routes]
code = "voyage_code"            # → voyage_context after Layer 2 eval
docs = "voyage_text"
knowledge = "voyage_text"
notes = "voyage_text"
threads = "voyage_text"
agent_manifest = "voyage_text"
git_message = "voyage_text"
transcripts = "voyage_text"

[embed.routes.visual]
pdf_figure = "voyage_visual"
spreadsheet_chart = "voyage_visual"
slide_image = "voyage_visual"
image_caption = "voyage_visual"
video_segment = "voyage_visual"
```

Backward compatibility:

- Legacy `[embed.providers.voyage]` parses as alias `voyage` with
  `type = "voyage_text"` and its configured model.
- Absent config synthesizes `voyage_code`, `voyage_text`, and `ollama`.
- Unknown route provider ids remain errors; silent fallback would search
  the wrong vector space.

## Compatibility Families

Route metadata grows explicit family identity:

```rust
enum EmbedEndpointKind {
    Text,
    ContextualizedText,
    Multimodal,
    Ollama,
}

struct Route {
    bucket: Bucket,
    project_id: Option<String>,
    provider_id: String,
    endpoint_kind: EmbedEndpointKind,
    document_model: String,
    query_model: String,          // == document_model unless asymmetric
    dimensions: usize,
    output_dtype: OutputDType,    // float | int8 | uint8 | binary | ubinary
    compatibility_family: String,
}
```

Derivation is code-owned (never operator-supplied):

- voyage-4 family (incl. nano, local or hosted) → `voyage-4:<dim>:<dtype>`
- `voyage-code-3` → `voyage-code-3:<dim>:<dtype>`
- `voyage-context-4` → `voyage-context-4:<dim>:<dtype>` (each exact
  contextual model is its own family; no documented cross-version space)
- `voyage-multimodal-3.5` → `voyage-multimodal-3.5:<dim>:<dtype>`
- `nomic-embed-text` → `ollama:nomic-embed-text:768:float`
- Unknown future models default to exact-model families until classified.

Rules:

- `vector_route_id()` keeps identifying exact partitions; it must grow
  dtype. A query vector is reused across partitions only when families
  match exactly.
- Asymmetric `document_model`/`query_model` pairs must derive the same
  family or the route loader rejects the config at load, not at search.
- The loader rejects dimension/family mismatches before indexing.

## Provider Interface

Replace the conflated trait:

```rust
enum EmbedInputType { Query, Document }

enum EmbedInput {
    Text(String),
    DocumentChunks(Vec<String>),      // contextualized routes
    Multimodal(Vec<MultimodalPart>),
}

enum MultimodalPart {
    Text(String),
    ImageBytes { mime: String, bytes: Vec<u8> },
    VideoBytes { mime: String, bytes: Vec<u8> },
}

trait EmbeddingProvider {
    async fn embed_batch(
        &self,
        inputs: &[EmbedInput],
        input_type: EmbedInputType,
    ) -> Result<Vec<EmbedOutput>>;   // DocumentChunks yields one vector per chunk

    fn dimensions(&self) -> usize;
    fn document_model(&self) -> &str;
    fn query_model(&self) -> &str;
    fn endpoint_kind(&self) -> EmbedEndpointKind;
    fn compatibility_family(&self) -> &str;
}
```

Rules:

- Queue workers send `Document`; hybrid search and agent/semantic lookup
  helpers send `Query`. The query/document split is a correctness fix —
  today neither value is sent.
- Text providers reject non-text inputs with a typed error; contextualized
  providers reject `Multimodal`; multimodal providers accept text but
  text-only buckets must not route there by default.
- Asymmetric providers select `document_model` vs `query_model` from
  `input_type` internally.

Call sites that must change: `embed/queue.rs` workers (`Document`),
`mcp_tools/hybrid_search.rs` + `query_cache.rs` (`Query`,
family-scoped cache key), agent-lookup embed paths (`Query`), and the
provider mocks/tests.

## Multimodal Chunk Model

Unchanged from the prior revision. Text-first chunks ride the existing
`project_file` path (`pdf_page`, `pdf_table`, `spreadsheet_sheet`,
`notebook_cell`, `slide`, `web_section`, `transcript_segment`, …).
Visual payload sidecars only where pixels are semantically load-bearing
(`pdf_figure`, `spreadsheet_chart`, `slide_image`, `image_caption`,
`video_segment`).

No raw image/video bytes in Tantivy; a content-hash-addressed sidecar:

```rust
struct VisualPayloadRef {
    source_file: PathBuf,
    source_hash: String,
    chunk_entity_id: String,
    mime: String,
    byte_range: Option<(u64, u64)>,
    page: Option<u32>,
    bbox: Option<[f32; 4]>,
    timestamp_ms: Option<u64>,
    frame_idx: Option<u32>,
    extracted_path: PathBuf,
    pixel_hash: String,
}
```

Sidecar anchoring should use the `file:` virtual entity once it lands
(gap-ab3ef97f) rather than the chunk[0]-as-file proxy. Payload guards
match current provider limits (image ≤ 20 MB / ≤ 16M px, video ≤ 20 MB,
≤ 32K tokens per input by provider accounting).

## Search Semantics

For a text query:

1. Run BM25 as today.
2. Resolve route metadata for every configured bucket; reject query
   encoders whose family does not match the target partition.
3. Embed the query once per exact query encoder (provider, query_model,
   dim, dtype); cache key `(provider_id, query_model, dim, dtype, query)`.
   Compatibility decides which partitions a vector may search, not whether
   two encoders share a cache entry.
4. Search compatible partitions; fuse BM25 + vector lists via RRF.
5. Optional model rerank: send fused top-k to `rerank-2.5-lite`, re-order
   by relevance score. On API failure fall back to heuristic rerank and
   report `degraded.rerank_unavailable`.
6. Apply heuristic type/temporal adjustments under the existing cap.

Partitions with no current route metadata are skipped and reported under
`degraded.skipped_partitions` — never guess a query encoder for orphaned
partitions.

For a future visual query: accept explicit parts (`text`/`image`/
interleaved), search only compatible multimodal families unless text-only
fallback is requested, fuse with BM25 only when the query has text.

Never:

- Search a `voyage-code-3` partition with a voyage-4 query vector (or any
  cross-family pairing — equal dims prove nothing).
- Merge cosine scores across families before rank normalization.

## Migration Plan

Phase 1 — Routing substrate (Layer 0) — **shipped 2026-07-10**
(`crates/bbox-embed`): typed provider alias map with legacy parsing,
role-marked `embed_batch(&[EmbedInput], EmbedInputType)`, route metadata
with endpoint kind / dtype / derived compatibility family, asymmetric
document/query models with load-time family enforcement, encoder-exact
query-cache key, and status reporting. Float partition ids intentionally
unchanged (dtype joins the hash only when non-float) so no partition
orphaned. Non-float `output_dtype` config is rejected until quantized
response decoding exists; the family/partition machinery already honors
dtype. Original scope for the phase:

- Provider alias map with `type`; legacy `[embed.providers.voyage]` still
  parses.
- `input_type` on the trait; `Document` on all queue paths, `Query` on all
  live retrieval paths.
- `output_dimension`, `output_dtype`, `endpoint_kind`,
  `compatibility_family` in route metadata; dtype into
  `vector_route_id()`.
- Tests: document vs query serialize different `input_type`; two Voyage
  aliases with different models; unknown alias rejection; legacy parsing;
  derived family values; asymmetric pair family mismatch rejected.

Phase 2 — Partition lifecycle (Layer 5): `bbox_embed_partitions`
list/prune as specified above, before any deliberate migration.

Phase 3 — Prose re-route + asymmetric retrieval (Layer 1):

- Prose buckets → `voyage_text` (voyage-4 family); `code` stays on
  `voyage_code` pending eval.
- `bbox_reembed` per bucket; old partitions pruned via Phase 2 tooling
  after verification.
- Repair the stalled partial coverage on `code`, `docs`, and `git_message`
  (observed 2026-07-09) before re-routing so the backfill and the
  migration aren't conflated.

Phase 4 — Model rerank stage (Layer 3): `[embed.rerank]` config, opt-in
param, eval A/B against heuristic-only using the metrics substrate; ship
as default only on a measured win.

Phase 5 — Contextualized embeddings (Layer 2): document-grouped queue
batching, `DocumentChunks` input, `voyage_context` route for `code`/`docs`
behind an eval comparison vs their Layer-1 routes.

Phase 6 — Multimodal provider + first visual chunker (Layer 4):

- `VoyageMultimodalProvider` against `/v1/multimodalembeddings` with the
  payload guards above; export-policy docs for pixels leaving the host.
- `X-PDF` text-first (`pdf_page`, `pdf_table`); `pdf_figure` sidecar only
  after the provider path exists; OCR shell-outs gated behind
  availability checks and timeouts.
- Visual eval (figure/table/chart query set) before multimodal becomes
  default for any visual kind: text-only extraction via voyage-4 vs
  visual sidecar via voyage-multimodal-3.5.

## Acceptance Criteria

- Config routes `code` and `knowledge` to different Voyage models; provider
  config is a typed alias map; legacy config still parses.
- Stored embeddings send `input_type=document`; queries send
  `input_type=query`.
- Route status reports provider id, endpoint kind, models (document/query),
  dim, dtype, and compatibility family.
- Hybrid search embeds once per exact query encoder; the query cache key
  includes the query model, dim, and dtype. Family compatibility alone
  never aliases two query models in the cache.
- An asymmetric route (document `voyage-4-large`, query `voyage-4-lite`)
  works end-to-end; a cross-family asymmetric pair is rejected at config
  load.
- Equal-dimension incompatible partitions are never searched with the same
  query vector; a dtype change alone forces a new family and partition.
- Orphaned partitions are visible (`list`), skipped by search, and
  prunable only via explicit dry-run-default tooling with an age threshold.
- Model rerank is opt-in, A/B-measurable via MRR/recall@k, and degrades to
  heuristic rerank on API failure with an explicit degraded marker.
- Contextualized routes never split one document's chunks across batches.
- Visual payloads live outside Tantivy, content-hash addressed; multimodal
  export is documented separately from text export.
- `X-PDF` ships text-first without a visual embedding model.

## Open Questions

- Quantization posture: at ~60K indexed vectors (2026-07-09), float at
  1024d is cheap; int8/binary buys little until the corpus grows 10–50×.
  Defer a quantized family until partition size or memory pressure says
  otherwise? (Multimodal quantized output is additionally undocumented;
  see External Model Facts.)
- Contextualized chunking parameters: if Layer 2's eval ever exercises
  auto-chunking, chunk size and overlap are recipe inputs to sweep, not
  constants (overlap tokens are billed again).
- Local query encoding via open-weight `voyage-4-nano`: worth the
  inference dependency for offline/latency wins, or keep queries hosted on
  `voyage-4-lite`?
- Do we keep `docs` entirely prose-routed, or split code-adjacent docs
  from general markdown via chunk metadata? Extension-based bucket
  selection still cannot answer this.
- Per-project route keys (`per_project` map) should resolve through the
  shared project resolver from
  `design/corpus/agentic-corpus/project-taxonomy-standardization.md`
  (logical project ids/aliases, not paths) — coordinate when that
  resolver lands.
