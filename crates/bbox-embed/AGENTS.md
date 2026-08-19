# bbox-embed — embedding providers, routes, queue, query cache

- Query-time embedding is a BLOCKING HTTP round-trip per route, driven to
  completion with `block_in_place` — callers must already be on the blocking
  pool / sync context. Retry and backoff belong to the background queue
  worker only; a query-path embed failure degrades that route for that
  search and must not retry inline.
- `embed::query_cache` is THE process-wide query-vector memo, keyed by the
  exact query encoder `(provider, query_model, dim, dtype, query)` — the
  vector is identical for the same key regardless of bucket, so one cache
  serves every surface. Compatibility families decide which partitions a
  cached vector may SEARCH, never cache identity: two same-family query
  models still produce different vectors. Hybrid search and agent search
  both go through it. **Do not grow bespoke per-surface caches again** —
  the pre-2026-06 state was a private `AGENT_QUERY_EMBED_CACHE` next to an
  uncached hybrid path that re-embedded every search per route
  (gap-172e52e4). Failed embeds are never cached.
- `EmbeddingRouter::load_default()` re-parses `embed.toml` on every call.
  Fine per search; do not put it inside per-item loops.
- A route's `vector_route_id` hashes provider alias + document_model + dims
  (+ dtype when non-float) — identical config on two hosts yields the same
  partition route id. Useful when scripting cross-host partition
  maintenance; don't assume it differentiates hosts. Float dtype is
  deliberately EXCLUDED from the hash so pre-dtype partitions keep their
  ids; never "simplify" it in — that orphans every deployed corpus.
- Providers are config aliases (`[embed.providers.<alias>]` with a `type`
  discriminator), not singletons; legacy `voyage`/`ollama` tables without
  `type` still parse, and built-ins (`voyage`, `voyage_code`,
  `voyage_text`, `ollama`) synthesize when unconfigured. Route validation
  (asymmetric document/query model family agreement, dtype support) lives
  in `EmbeddingRouter::route()` — construction paths reuse it.
- Every embed call carries an `EmbedInputType` role: queue workers send
  `Document`, live retrieval sends `Query`. Voyage vectors differ by role,
  so a new provider/call site must never default one role for both.
- Queue worker retry state is PER BATCH (`RetryBatch` queue in
  `queue.rs`), never a single worker-level slot: a wave dispatches
  `WORKER_CONCURRENCY` batches in parallel, and a shared slot let a second
  failing batch-mate overwrite the first (and a succeeding batch-mate clear
  it), stranding items in the `pending` identity set with their
  `queue_depth` accounted and no path to persist, drop, or re-enqueue until
  a daemon restart. Every admitted batch must end in exactly one of
  `mark_success`, `mark_dropped`, or `mark_poison_dropped`, each paired
  with `release_pending`. Provider 429s carry `RateLimitedBatchError`
  (attach it via `classify_provider_http_failure`) and get the longer
  `MAX_RATE_LIMIT_RETRIES` budget with a per-minute-window floor; the
  generic 1s/2s/4s ladder cannot outlast a TPM window.
