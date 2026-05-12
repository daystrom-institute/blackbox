# Multimodal and Embedding Routing Design

Date: 2026-05-07

## Problem

The original agentic-corpus design correctly made embeddings bucket-routed, but
the concrete implementation now lags the model landscape and the intended
future multimodal path:

- `src/embed/voyage.rs` defaults to `voyage-code-3` for the single `voyage`
  provider slot.
- `EmbeddingRouter` can route buckets to provider ids (`voyage`, `ollama`),
  but cannot express multiple Voyage-backed routes with different models.
- Query embedding uses the same `embed_batch` path as document embedding, so
  Voyage `input_type` is omitted for both stored chunks and live queries.
- The design open question still names `voyage-multimodal-3`; current Voyage
  multimodal guidance points at `voyage-multimodal-3.5`.
- Existing vector partitions are model-scoped, but the search layer needs a
  stricter rule: only embed a query with the model family that produced the
  partition being searched. Same dimensions do not imply shared vector space.
- The current config schema has hardcoded `voyage` and `ollama` provider
  fields. A multi-Voyage design therefore requires a real provider-map
  migration, not just new route names in TOML.

This doc supersedes only the embedding-model selection and multimodal-routing
parts of `design/archive/agentic-corpus.md` and `design/archive/agentic-corpus-impl.md`. The
chunker registry, bucket model, HNSW partitioning, and RRF fusion strategy stay.

## Current Baseline

Implemented buckets:

- `knowledge`
- `code`
- `docs`
- `transcripts`
- `git_message`
- `notes`
- `threads`
- `agent_manifest`

Implemented providers:

- `voyage`: hardcoded 1024 dimensions, default model `voyage-code-3`
- `ollama`: default model `nomic-embed-text`

Implemented partition identity:

- `Route::vector_route_id()` includes provider id, model, dimensions, and a
  short hash. This is correct and must remain the isolation boundary.

Observed gap in `blackbox-dev` on 2026-05-07:

- `code`, `docs`, and `git_message` are mostly/fully indexed under
  `voyage-code-3`.
- `knowledge`, `notes`, and `threads` have source counts but zero indexed
  vectors. That coverage issue is independent of model choice and should be
  fixed before judging retrieval quality for those buckets.

## External Model Facts

These facts come from Voyage documentation and release notes, checked on
2026-05-07.

- Voyage text embeddings are served at `/v1/embeddings`; recommended text
  models include `voyage-4-large`, `voyage-4`, `voyage-4-lite`,
  `voyage-3-large`, `voyage-3.5`, `voyage-3.5-lite`, `voyage-code-3`,
  `voyage-finance-2`, and `voyage-law-2`.
- For retrieval/search, Voyage recommends setting `input_type` to `query` for
  query embeddings and `document` for stored corpus embeddings. Embeddings
  generated with and without `input_type` are compatible inside the same model.
- `voyage-4-large`, `voyage-4`, `voyage-4-lite`, `voyage-3-large`,
  `voyage-3.5`, `voyage-3.5-lite`, and `voyage-code-3` support 2048, 1024,
  512, and 256 output dimensions.
- The Voyage 4 text family has an explicit shared embedding-space guarantee:
  documents embedded with one 4-series model can be searched with queries
  embedded by another 4-series model.
- No source found states that `voyage-code-3` vectors are compatible with
  `voyage-4` or with `voyage-multimodal-3.5`. Do not infer compatibility from
  equal dimensions.
- `voyage-multimodal-3.5` embeds interleaved text, images, and videos through
  the multimodal endpoint. It supports `input_type=query|document`, videos,
  and the same 256/512/1024/2048 dimension choices. It is not just a text model
  replacement; it is a separate route family for visual-native content.

Primary references:

- https://docs.voyageai.com/docs/embeddings
- https://docs.voyageai.com/reference/embeddings-api
- https://docs.voyageai.com/docs/multimodal-embeddings
- https://blog.voyageai.com/2026/01/15/voyage-4/
- https://blog.voyageai.com/2026/01/15/voyage-multimodal-3-5/

## Design Principles

1. Model compatibility is explicit, not dimension-derived.
2. Text-first chunkers must not block on multimodal embeddings.
3. Visual-native retrieval is a new route family, not a global replacement.
4. Route identity must include every parameter that changes vector space:
   provider, endpoint kind, model, output dimension, dtype, and compatibility
   family.
5. Query embedding must be route-local. A partition produced by a given route
   is searched with that route's query encoder, except when an explicit
   compatibility-family rule allows asymmetric retrieval.
6. Ranking fusion remains cross-route RRF. Raw cosine scores across unrelated
   model families are not directly comparable.
7. Corpus export policy remains bucket-scoped. Adding multimodal must make
   image/video/PDF pixel export explicit.

## Recommended Routing

Default hosted configuration should split Voyage into named routes. This
requires replacing the current hardcoded `ProviderConfigs { voyage, ollama }`
shape with a map keyed by provider alias:

```toml
[embed.providers.voyage_code]
type = "voyage_text"
api_key_env = "VOYAGE_API_KEY"
model = "voyage-code-3"
output_dimension = 1024

[embed.providers.voyage_text]
type = "voyage_text"
api_key_env = "VOYAGE_API_KEY"
model = "voyage-4"
output_dimension = 1024

[embed.providers.voyage_text_large]
type = "voyage_text"
api_key_env = "VOYAGE_API_KEY"
model = "voyage-4-large"
output_dimension = 1024

[embed.providers.voyage_visual]
type = "voyage_multimodal"
api_key_env = "VOYAGE_API_KEY"
model = "voyage-multimodal-3.5"
output_dimension = 1024

[embed.providers.ollama]
type = "ollama"
endpoint = "http://localhost:11434"
model = "nomic-embed-text"

[embed.routes]
code = "voyage_code"
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

- Existing configs with `[embed.providers.voyage]` continue to parse.
- During load, legacy `voyage` is treated as a normal provider alias with
  `type = "voyage_text"` and whatever model it configured.
- If no config exists, defaults synthesize `voyage_code`, `voyage_text`, and
  `ollama` aliases internally.
- Unknown route provider ids remain errors; silently falling back to `voyage`
  would hide typos and could search the wrong vector space.

Rationale:

- `code` keeps the code-specialized model until a local eval proves
  `voyage-4-large` or `voyage-4` is better for this repo's code queries.
- `knowledge`, `notes`, `threads`, `agent_manifest`, `git_message`, and most
  `docs` move to the Voyage 4 family because these are semantic text and
  operational prose, not code corpora.
- `voyage_text_large` is available for one-time document indexing if we want
  asymmetric Voyage 4 retrieval later: embed stored text with
  `voyage-4-large`, query with `voyage-4` or `voyage-4-lite`.
- `voyage_visual` is opt-in and used only for chunks that preserve meaningful
  visual evidence.

## Compatibility Families

Add an explicit compatibility family to route metadata:

```rust
enum EmbedEndpointKind {
    Text,
    Multimodal,
    Ollama,
}

struct Route {
    bucket: Bucket,
    project_id: Option<String>,
    provider_id: String,
    endpoint_kind: EmbedEndpointKind,
    model: String,
    dimensions: usize,
    output_dtype: OutputDType,
    compatibility_family: String,
}
```

Examples:

- `voyage-4-large`, `voyage-4`, `voyage-4-lite`, and `voyage-4-nano` at the
  same output dimension: `compatibility_family = "voyage-4:1024:float"`.
- `voyage-code-3` at 1024 float:
  `compatibility_family = "voyage-code-3:1024:float"`.
- `voyage-multimodal-3.5` at 1024 float:
  `compatibility_family = "voyage-multimodal-3.5:1024:float"`.
- `nomic-embed-text` at 768 float:
  `compatibility_family = "ollama:nomic-embed-text:768:float"`.

`vector_route_id()` should continue to identify exact partitions. Search may
reuse a query vector across partitions only when compatibility families match.

Compatibility family must be derived by code from provider type, model,
dimension, and dtype. It must not be an operator-supplied string in config.
Provider implementations own the derivation table:

- Voyage 4 text models map to `voyage-4:<dim>:<dtype>`.
- `voyage-code-3` maps to `voyage-code-3:<dim>:<dtype>`.
- `voyage-multimodal-3.5` maps to
  `voyage-multimodal-3.5:<dim>:<dtype>`.
- Unknown future models default to exact-model families until explicitly
  classified.

The route loader should reject a family if provider-derived dimensions do not
match the configured output dimension. This makes family/dimension mismatches
fail before indexing, rather than surfacing later as search degradation.

## Provider Interface

The current trait conflates document and query embeddings:

```rust
async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
```

Replace or extend it with:

```rust
enum EmbedInputType {
    Query,
    Document,
}

enum EmbedInput {
    Text(String),
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
    ) -> Result<Vec<Vec<f32>>>;

    fn dimensions(&self) -> usize;
    fn model_name(&self) -> &str;
    fn endpoint_kind(&self) -> EmbedEndpointKind;
    fn compatibility_family(&self) -> &str;
}
```

Rules:

- Queue workers call with `input_type = Document`.
- Hybrid search calls with `input_type = Query`.
- Agent lookup and other semantic lookup helpers call with
  `input_type = Query`.
- Text providers reject non-text `EmbedInput` with a typed error.
- Multimodal providers accept text-only inputs too, but text-only buckets should
  not route there by default because that makes every query pay the multimodal
  route cost and changes the compatibility family.

The query/document split is a correctness fix, not an optimization. The current
implementation sends neither value, so Voyage embeds stored corpus chunks and
live search queries as unspecified text. Voyage documents that retrieval inputs
should be marked by role; keeping the field unset leaves relevance on the table
and makes future eval results harder to interpret.

Implementation call sites that must change:

- `src/embed/queue.rs` workers: `provider.embed_batch(..., Document)`
- `src/mcp_tools/hybrid_search.rs::embed_with_provider`:
  `provider.embed_batch(..., Query)`
- `src/main.rs` agent-query helper paths that embed live lookup text:
  `provider.embed_batch(..., Query)`
- provider tests/mocks in `src/embed/voyage.rs`, `src/embed/ollama.rs`, and
  `src/embed/queue.rs`

## Multimodal Chunk Model

Keep text-first chunks in the existing `project_file` path:

- `pdf_page` text
- `pdf_table` extracted text or markdown table
- `spreadsheet_sheet` summary text
- `spreadsheet_cell_range` formula/value text
- `notebook_cell`
- `slide` text
- `web_section`
- `transcript_segment`

Add visual payload sidecars only for chunks where pixels are semantically
load-bearing:

- `pdf_figure`
- `spreadsheet_chart`
- `slide_image`
- `image_caption`
- `video_segment`

Do not store raw image/video bytes in Tantivy. Store a sidecar object with:

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

The chunk's text `content` remains searchable by BM25 and text embeddings. The
visual route embeds `MultimodalPart::ImageBytes` or `VideoBytes` plus optional
context text such as caption, page heading, neighboring OCR, or filename.

## Search Semantics

For a text query:

1. Run BM25 as today.
2. Resolve current route metadata for every configured bucket.
3. Group active vector partitions by compatibility family.
4. For each family, embed the query with that family's configured query model.
5. Search exact partitions in that family.
6. Fuse BM25 and vector rank lists via RRF.

Partitions that have no current configured route metadata are skipped by
default and reported under `degraded.skipped_partitions`. This is the safe
behavior for orphaned partitions after a model migration. An operator-only
diagnostic mode can search orphaned partitions later, but normal retrieval
should not guess which query encoder to use.

For a visual query in the future:

1. Accept an explicit query payload (`text`, `image`, or interleaved parts).
2. Search only compatible multimodal families unless the caller asks for
   text-only fallback.
3. Fuse with BM25 only when the query has text.

Never:

- Search a `voyage-code-3` partition with a `voyage-4` query vector.
- Search a `voyage-multimodal-3.5` partition with a `voyage-code-3` query
  vector.
- Merge cosine scores from unrelated route families before rank normalization.

## Migration Plan

Phase 1: Text routing correction

- Add named provider config with `type`.
- Replace hardcoded `ProviderConfigs { voyage, ollama }` with a provider map
  keyed by alias plus a typed provider enum.
- Keep legacy `[embed.providers.voyage]` readable as alias `voyage`.
- Add `output_dimension`, `output_dtype`, `endpoint_kind`, and
  `compatibility_family` to route metadata.
- Send `input_type=document` for every queue/stored-corpus embedding path.
- Send `input_type=query` for every live retrieval embedding path, including
  `bbox_hybrid_search`, agent lookup, and any future visual query endpoint.
- Add a mock Voyage test that asserts document paths and query paths serialize
  different `input_type` values.
- Add route-loader tests for:
  - two Voyage aliases with different models
  - unknown route alias rejection
  - legacy `[embed.providers.voyage]` parsing
  - provider-derived compatibility family values
- Update defaults: `code = voyage_code`; prose buckets route to `voyage_text`.
- Add `bbox_reembed` guidance because changing model/dim/family creates new
  partitions; old partitions remain until pruned.

Phase 2: Backfill existing non-code buckets

- Fix the observed zero-coverage state for `knowledge`, `notes`, and `threads`.
- Add a regression test that source_count > 0 for those buckets can produce
  indexed_count > 0 under a mock provider.
- Run `bbox_reembed` for each prose bucket after route migration.

Phase 3: Multimodal provider support

- Add `VoyageMultimodalProvider` against `/v1/multimodalembeddings`.
- Add payload-size guards matching provider limits:
  - image <= 20 MB and <= 16M pixels
  - video <= 20 MB
  - per-input <= 32K tokens by provider accounting
- Add export-policy docs for pixel/video data leaving the host.

Phase 4: First visual chunker

- Implement `X-PDF` as text-first first: `pdf_page` and `pdf_table`.
- Add `pdf_figure` visual sidecar support only after the multimodal provider
  path exists.
- Gate OCR/tesseract shell-outs behind availability checks and timeouts.

Phase 5: Visual eval

- Add an eval query set for figure/table/chart retrieval before making
  multimodal default for any visual kind.
- Compare:
  - text-only extraction through `voyage-4`
  - visual sidecar through `voyage-multimodal-3.5`
  - optional local/open baseline if available

Phase 6: Partition lifecycle

- Add `bbox_embed_partitions(action="list|prune")` or equivalent CLI/MCP
  surface before encouraging broad route churn.
- `list` reports exact route id, provider, endpoint kind, model, dim, dtype,
  compatibility family, active_count, last_write, and whether any configured
  bucket currently maps to it.
- `prune` is dry-run by default and only deletes partitions that are both:
  - unmapped by current route config
  - older than an operator-supplied age threshold
- Never auto-prune as part of `bbox_reembed`; reembed proves replacement
  vectors exist, but deletion is a separate destructive lifecycle operation.

## Acceptance Criteria

- Config can route `code` and `knowledge` to two different Voyage models.
- Provider config is a typed alias map, while legacy `[embed.providers.voyage]`
  still parses.
- Route status reports provider id, endpoint kind, model, dim, dtype, and
  compatibility family.
- Stored embeddings use `input_type=document`; query embeddings use
  `input_type=query`.
- Hybrid search embeds once per compatibility family, not once per arbitrary
  route string.
- Equal-dimension incompatible partitions are not searched with the same query
  vector.
- Existing legacy config remains readable.
- A route/model change creates a new partition and does not corrupt old vectors.
- Orphaned partitions are visible and skipped by normal search unless their
  compatibility family is still configured.
- Partition pruning has an explicit dry-run and age threshold.
- Visual payloads are stored outside Tantivy and are content-hash addressed.
- Multimodal export is documented separately from text export.
- `X-PDF` can ship text-first without selecting a visual embedding model.

## Open Questions

- Should prose defaults use `voyage-4` or `voyage-4-large` for stored corpus?
  `voyage-4-large` maximizes document quality, but `voyage-4` is a cheaper
  balanced default. The route model should allow either without code changes.
- Should Voyage 4 asymmetric retrieval be represented as separate
  document/query provider ids or as one provider with `document_model` and
  `query_model` fields?
- Do we keep `docs` entirely prose-routed, or split code-adjacent docs from
  general markdown using chunk metadata? Current extension-based bucket
  selection cannot answer that.
- What is the right local/offline multimodal fallback, if any?
