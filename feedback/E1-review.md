# E1 + D2 fixes review

Commits `3e53118..fc4d6bb` (4 D2 fixes + E1 embedding provider trait + Voyage + Ollama + routing).

## Issues (fix-forward)

1. **No HTTP request timeout on Voyage / Ollama clients.** `reqwest::Client::new()`
   defaults to no timeout. A hung Voyage backend would block the
   embedding task indefinitely. Add `Client::builder().timeout(Duration::from_secs(60))`
   for both providers. Embedding batches can be slow but 60s is a
   reasonable upper bound; add a per-call timeout config knob if
   batches grow.

2. **Rate limit config field exists but isn't enforced.** `VoyageConfig::rate_limit_per_min`
   defaults to 100 but `embed_batch` just calls `client.post(...)`.
   E2 should wire a per-route rate limiter (token bucket) into the
   queue task. Flag for E2 prep.

3. **No retry on transient failures.** `error_for_status()` causes
   any non-2xx to bail immediately. A 503 from Voyage = lost batch.
   E2's queue should wrap with exponential backoff retry. Flag for
   E2 prep.

## Concerns

4. **`reqwest::Client::new()` per provider construction.** No
   connection pool sharing across routes. With 6 buckets and per-
   project overrides, you could end up with N×N clients. Defer
   refactor: share a single `reqwest::Client` instance via the
   `EmbeddingRouter` once E2 wires the persistent queue task.

5. **`bbox_reembed_stub` MCP tool exists but doesn't appear in the
   wired tool registry yet** (only the `reembed_stub` function).
   Confirm in next cycle that it's exposed via MCP `[tool]` macro;
   if not, surface in done note.

6. **`#[allow(dead_code)]` at module top + on Voyage struct.** Same
   pattern as D1/D2: surface gets built ahead of consumers (E2
   wires the queue, E3 wires storage). Track for removal as each
   piece lights up.

## D2 fix observations

7. **D2 fix #1 (shared entity loader)** — `src/entity_loader.rs`
   created with `load` + `compact_label` + `label_from_properties`.
   Both inspect and bundle_evidence use it. Future tools (D3, M3,
   etc.) call the same function. Architectural fix landed cleanly.
   ✓

8. **D2 fix #2 (loaded compact labels)** — `compact_label` now
   consults loaded entity properties first (title/name/qualified_name/
   topic/subject), falls back to provider stub. Edge labels in
   inspect output now show actual titles. ✓

9. **D2 fix #3 (richer inspect text)** — render_text expanded
   from 5 lines to ~100 LoC of structured markdown matching the
   daystrom AgenticTools spike density. Edges, recommended hops,
   coverage all surfaced in text. ✓

10. **D2 fix #4 (clippy cleanup)** — small fix to keep delta
    contained. ✓

## Nits

11. **`Bucket::as_str` / `RoutesConfig::global` / `BucketRoutes::get`
    are three near-identical match expressions.** `Bucket` could
    derive `strum::EnumIter` or implement an `AsRef<str>` to
    consolidate. Subjective.

12. **Voyage default model `voyage-code-3` is hardcoded as a const.**
    Operators wanting `voyage-3` (text-only) override via config.
    Document that voyage-code-3 dim is 1024; voyage-3-large is
    different. Minor.

13. **`from_config` clones the config into the provider struct.**
    Cheap (small struct) but if config grows, consider Arc<Config>.
    Subjective.

14. **`api_key` stored as `Option<String>`** in the provider
    struct. If logged anywhere (debug formatter), the secret leaks.
    Verify `Debug` on `VoyageProvider` doesn't print api_key —
    looking at the struct, `Debug` is NOT derived, so accidental
    `dbg!()` won't expose it. ✓ but worth a comment.
