---
title: "Operator Web UI"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - surfaces
  - web-ui
brief: "A read-only operator web UI rendered by blackboxd itself on a loopback-only listener behind an identity proxy: threads, bros, gaps, projects, system state, and redacted configuration, with live roster and tail over SSE."
---

# Operator Web UI

The daemon renders a browser surface for the operator from the state it
already holds. The surface is read-only, server-rendered, and reachable only
through an identity-aware reverse proxy that is mechanically the only route
in. It replaces the retired terminal cockpits as the human view of threads,
bros, gaps, projects, and daemon health.

## Invariants

- The UI is served by `blackboxd` in-process. Pages read through the same
  internal functions the MCP tools call. The UI never dials the daemon's own
  MCP endpoint and never holds a credential for it.
- The UI has its own listener, `daemon.ui_listen` (`BBOX_UI_LISTEN`), off by
  default. The daemon refuses a non-loopback bind for it at startup. The main
  listener (MCP, control, admin, producer planes) never serves a UI route.
- The UI listener trusts the identity headers of the proxy in front of it
  (`X-Forwarded-Email`, then `X-Forwarded-User`). A request without an
  identity is answered with a 401 page; there is no anonymous view. This is
  the one listener in the daemon that reads forwarded headers, and it is safe
  because the bind is loopback and the proxy is the only process that can
  reach it. The peer-only rule of the main listener's gates is unchanged.
- Version one is read-only. No route mutates daemon state. Write controls
  (steer, cancel, drain, brofile edits) are a separate design with a CSRF
  boundary and a principal-stamped audit line.
- The configuration page never renders token material. Every secret-bearing
  field and every token-file path is replaced by a fixed redaction marker, and
  a test enumerates the config schema to prove it.
- No node toolchain. Templates compile into the binary; the stylesheet and the
  one vendored script are embedded and served with content-hashed names and
  immutable cache headers.

## Listener and trust model

The daemon builds a second axum router when `ui_listen` is set and serves it
on that bind with connect info. The router carries a principal layer that
reads the forwarded identity, stores it in request extensions, and refuses
without it. Health probes for the UI listener are not needed: the proxy's own
readiness covers the pair, and the daemon's `/readyz` on the main listener
already proves startup.

The deployment overlay runs an OAuth proxy as a sidecar in the daemon's pod,
with the daemon's UI port as its upstream, group-restricted at the proxy, and
publishes the proxy on its own hostname. The proxy forwards the signed-in
user's email. The overlay owns the proxy, the realm client, the ingress, and
the portal registration; this repository owns only the listener and its
principal rule.

## Stack

- HTML: `maud`, compile-time typed templates, no template files to load and
  no runtime parser. A `layout` function renders the shell; page functions
  render into it; fragment functions render the pieces htmx swaps.
- Interaction: `htmx` (vendored, one file) for partial updates and its SSE
  extension for live regions. Pages work without JavaScript; htmx adds live
  refresh and in-place filtering.
- Live data: axum SSE streams fed by the daemon's existing broadcasts (the
  roster delta channel and the tail channel). A lagged subscriber receives a
  `resync` event and the page re-fetches the region.
- Style: one hand-written stylesheet with CSS custom properties, light and
  dark through `prefers-color-scheme`, system font stack with tabular
  numerals for counts and timings. Dense operator-console layout: sticky
  table headers, fixed left navigation, no modal dialogs.
- Assets: embedded with `include_bytes!`, served under `/assets/<name>-<hash>`
  with `Cache-Control: public, max-age=31536000, immutable`.
- Tests: router tests over the UI listener with and without forwarded
  identity; snapshot tests of rendered fragments against a seeded test state;
  the redaction enumeration test.

## Page map

Every page shares the shell: left navigation (Overview, Threads, Bros, Gaps,
Projects, System, Configuration), a header with the daemon's hostname,
version, uptime, and the signed-in principal, and a footer with the
daemon's build identity.

| Route | Content | Source |
| --- | --- | --- |
| `/` | Overview: counts (threads by status, bros by state bucket, registered projects), index health, last activation, open gaps by resolution and by impact | the same readers as `bbox_stats`, `bro_dashboard`, `bbox_project_list`, `bbox_gaps` |
| `/threads` | Thread list with status and project filters; title, status, project, message count, updated | `bbox_thread_list` reader |
| `/threads/{id}` | Thread detail: messages in order, participants, linked entities, gaps filed from the thread | `bbox_thread`, `bbox_gaps` readers |
| `/bros` | Roster table: state bucket, task id, label or brofile, provider, model, elapsed, last event; live through the roster delta stream | `bro_dashboard` projection, `roster_tx` |
| `/bros/{task}` | Task detail: status, dispatch parameters (secrets redacted), report when present, event tail streamed while running, gaps the task filed | `bro_status`, `bbox_gaps` readers, `tail_tx` |
| `/gaps` | Gap list with resolution (default unresolved and acknowledged), kind, impact, blocking, domain, project and text filters; per-impact counts; title, kind, domain, impact, blocking, resolution, linked issue, project, created; source diagnostics; the table re-fetches on an interval | `bbox_gaps` selection (`resolve_gap_list`) |
| `/gaps/{id}` | Gap detail: summary, provenance (published ref and commit, or legacy), origin linked to its task and thread, need, evidence, notes, resolution with linked issues, supersession and same-dedupe-key gaps | `bbox_gaps` exact read |
| `/projects` | Catalog: project id, scope, attachments, publisher state, last activation | `bbox_project_list`, `bbox_project_catalog_list` readers |
| `/projects/{id}` | Project detail: attachments, graph summary, transport ownership, publisher status, open gaps | `bbox_project_catalog_get`, `bbox_project_graph_describe`, `bbox_gaps` readers |
| `/system` | Index stats, doctor findings, storage health and GC schedule, embed routes and queue, providers and allocator status, runtime metrics | `bbox_doctor`, `bbox_storage_health`, `bbox_embed_status`, `bro_providers`, `bro_allocator_status` readers, the runtime metrics collector |
| `/config` | Effective daemon config (redacted), the surface table with each surface's visible tools, brofiles, managed MCP server configs | the loaded config, the surface resolver, `bro_brofile` and `bro_mcp` readers |

Fragments (`/fragments/...`) return the table bodies and detail panes that
htmx swaps; each fragment is also a full page's building block, so there is
one renderer per view.

Gap pages select through the same resolution `bbox_gaps` makes: the project
filter arms the identity and ledger predicates, and the diagnostics the tool
returns (filter misses, skipped project overlays, legacy rows) render under
the table. Issue links come from the URLs in a gap's resolution note. No gap
broadcast exists, so the gap table refreshes by interval rather than SSE.

## Visual design

The console is for one operator reading a lot of state quickly. The rules:

- One accent color for links and the active navigation item; state colors
  only for task state buckets and health verdicts (running, waiting,
  finished, failed, stale), each paired with a text label so color is never
  the only signal.
- Tables over cards. Numbers right-aligned and tabular. Timestamps render as
  relative age with the absolute value on hover.
- Empty states say what is empty and where it comes from; error fragments
  name the reader that failed and keep the rest of the page alive.
- No icon font. A small inline SVG set for the navigation only.

## Slices

1. Plane: listener, principal rule, shell, stylesheet, asset pipeline,
   overview page, tests for the trust model.
2. Threads and bros pages, including the two live streams.
3. Projects, system, and configuration pages, including the redaction test.

## Crosscuts

- [MCP Surfaces](../mcp/mcp-surfaces-hub.md): the tool readers the pages
  share.
- [Fleet TUI](../../fleet-tui/fleet-tui.md): the retired terminal cockpit and
  the daemon-authoritative roster model this surface inherits.
- [Daemon Runtime](../../daemon-runtime/daemon-runtime.md): listener and
  plane isolation.
