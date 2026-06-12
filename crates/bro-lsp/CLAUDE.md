# bro-lsp — the harness-owned language-server pool

Warm LSP sessions for harness-side consumers (cell bindings, diagnostics
baselines). Rust-analyzer only today; jdtls/roslyn are the named growth gate
for Java/C# `lsp_verified` parity. Distinct from the daemon's `bbox-lsp`
pool by *residency*, not by idea: this one lives where the working set
lives, which is what the harness-native container test requires.

## The fail-closed contract (RX-V3 — don't negotiate)

Consumers chose an LSP-backed path specifically for server authority. An
unavailable server (binary missing, init timeout, crash) is an **error**
(`Error::is_lsp_unavailable` is the hook callers route on) — never a silent
downgrade to a syntax-only approximation. If you add a fallback, you have
changed the consumer's semantic claim, not improved availability.

## Warming and retries

rust-analyzer answers requests during indexing with `ContentModified`
(-32801) or retrigger-flagged `ServerCancelled` (-32802). Request methods
retry on exactly those within the configured request timeout — new request
methods should reuse that loop, not surface first-contact warming errors to
callers. Cold index on a real crate is tens of seconds; tests that pay it
get quarantined in `.config/nextest.toml`, and skip entirely when no
rust-analyzer binary resolves (env chain `BRO_LSP_RUST_ANALYZER_BIN` →
`BRO_RUST_ANALYZER_BIN` → `BLACKBOX_RUST_ANALYZER_BIN` → PATH → ~/.cargo/bin).

## Sessions and lifecycle

- Pool keyed by (canonicalized root, language); idle-evicted. The evictor
  task holds a `Weak` — dropping the pool ends it; server children exit on
  stdin EOF rather than kill_on_drop. Deliberate: no zombie reaping logic.
- Document state is a strict version contract (`DocumentAlreadyOpen`,
  `InvalidVersion`, superseded reads). Callers own their open-document
  bookkeeping; the binding-side pattern is lazy re-sync — re-send full text
  with a bumped version when the on-disk content hash drifts from what the
  server was last given (e.g. after an `edits.apply`).
- Positions are LSP-default UTF-16 line/character. Byte↔position conversion
  belongs to CALLERS at their edge (cell bindings convert; this crate speaks
  lsp-types natively). Multibyte is the test for any new conversion code.
