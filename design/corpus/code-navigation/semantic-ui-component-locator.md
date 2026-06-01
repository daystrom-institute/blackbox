---
title: "Semantic UI component locator"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - code-navigation
  - fleet-tui
brief: "A focused code-navigation surface for UI style tasks: locate semantically-related Ratatui/terminal UI components, draw helpers, call sites, lexical anchors, and recent edits as one bundle."
---

# Semantic UI component locator

Agents doing UI polish often know the role they need, not the symbol name:
"the roster composer header/trailer", "the single-agent activity strip", "the
footer chrome", "the tool-call renderer". Today they stitch this together with
`rg`, `bbox_hybrid_search`, `bbox_inspect_entity`, file reads, and `git show`.
That works, but it burns context and is easy to miss the comparable component.

This design adds a focused locator on top of the existing code-navigation and
graph substrate. It is not a general semantic parser; it is a role-oriented
bundle builder for UI component/style work.

## Proposed tool

```text
bbox_ui_component_locator(
  project_dir: String,
  query: String,
  framework: Option<"ratatui" | "terminal" | "web" | "auto">,
  files: Option<Vec<String>>,
  include_recent_edits: Option<bool>,
  limit: Option<usize>
)
```

Example:

```text
bbox_ui_component_locator(
  project_dir="/repo/blackbox",
  query="find the roster composer header/trailer and comparable single-agent composer chrome",
  framework="ratatui",
  files=["src/fleet_tui.rs"]
)
```

## Response shape

Return a compact, evidence-carrying bundle:

```json
{
  "status": "ok",
  "semantic_status": "heuristic_bundle",
  "query": "...",
  "primary_components": [
    {
      "role": "composer",
      "file": "src/fleet_tui.rs",
      "symbols": [
        {"name": "draw_composer", "line": 1910, "kind": "function_item"}
      ],
      "call_sites": [
        {"caller": "draw", "line": 1562, "why": "roster composer"},
        {"caller": "draw", "line": 1550, "why": "single-agent composer"}
      ],
      "lexical_anchors": ["roster_composer_top_titles", "single_agent_status_spans"],
      "recent_edits": [
        {"commit": "abc1234", "subject": "fix(fleet): align roster composer chrome"}
      ],
      "next_reads": [
        {"file": "src/fleet_tui.rs", "start_line": 1540, "end_line": 1605},
        {"file": "src/fleet_tui.rs", "start_line": 1880, "end_line": 1965}
      ]
    }
  ],
  "preview_fixtures": ["roster", "single-agent"]
}
```

The key promise is not perfect semantics. The promise is a small, inspectable
starting set with enough evidence that an agent can verify before editing.

## Search strategy

The implementation should combine four cheap signals:

1. **Lexical role expansion.** Split the query into role terms and known UI
   synonyms: composer/input/header/trailer/footer/chrome, roster/list/table,
   single-agent/detail/transcript/activity, tool/render/diff.
2. **Symbol inventory.** Use `bbox_code_symbols` or the indexed symbol table to
   find functions/types whose names contain role terms. For Ratatui, boost
   functions starting with `draw_`, ending in `_spans`, `_titles`, `_line`,
   `_block`, or `_render`.
3. **Call-site proximity.** Use syntax refs and lexical search to find where the
   candidate symbols are called. Prefer shared callers that wire both compared
   components, such as a top-level `draw` function.
4. **Recent edit grounding.** When requested, inspect recent commits that touched
   candidate files with role terms in the diff or subject. Return commit
   subjects and changed line neighborhoods, not full patches.

For Ratatui specifically, boost files importing `ratatui::prelude::*`,
`ratatui::widgets::*`, or using `Frame`, `Layout`, `Block`, `Paragraph`,
`Table`, `List`, `Line`, and `Span`.

## Relationship to preview snapshots

`bbox_ui_component_locator` should return `preview_fixtures` when it recognizes a
screen that has a deterministic preview. For fleet, those names should line up
with `bro fleet snapshot` fixtures from
`design/fleet-tui/ratatui-snapshot-preview.md`.

The locator answers "where should I edit and what should I compare?" The snapshot
preview answers "what did the UI actually render after the edit?"

## Implementation phases

### Phase 1: Ratatui heuristic bundle

- Implement as a daemon MCP tool in the code-nav category.
- Accept `framework="ratatui"` and `framework="auto"`; return
  `unsupported_framework` for other values.
- Use existing project registration and path confinement rules.
- Use live file/symbol reads only; no new index schema required.
- Return `semantic_status="heuristic_bundle"` explicitly.

### Phase 2: Indexed graph integration

- Add graph/entity refs for returned symbols and files.
- Include `bbox_bundle_evidence`-compatible entity refs where available.
- Use recent edge provenance when code indexing can tie a symbol to commits.

### Phase 3: General UI frameworks

- Add framework profiles for web component files, React/Vue/Svelte naming, and
  server-rendered templates.
- Keep framework profiles data-owned so new UI stacks do not require rewriting
  the core locator.

## Non-goals

- No LLM in the read path.
- No claim that returned components are complete or semantically authoritative.
- No automatic edit generation.
- No full git patch dumping; return commit handles and line windows instead.

## Acceptance

- The example query above returns `draw`, `draw_composer`,
  `roster_composer_top_titles`, `single_agent_composer_top_titles`,
  `roster_status_spans`, and `single_agent_status_spans` in one response.
- Returned line windows are narrow enough to read directly.
- The response labels itself heuristic and includes enough evidence to audit why
  each component was selected.
- The tool degrades cleanly when git history, indexed graph data, or Ratatui
  imports are unavailable.
