# AST-Grounded Restructure Execution Plan

Date: 2026-05-07
Status: executable checkpoint plan

## Purpose

This restates `design/restructure.md` as a sequence of grounding calls and
`bbox_refactor_*` plans. It is the worker-facing plan for a disposable practice
worktree. The goal is to prove the refactor MCP surface can mechanize the
restructure without benchmark-only shortcuts.

The worker must mutate files only through `bbox_refactor_plan`,
`bbox_refactor_apply`, or `bbox_refactor_run`, except for commands that only
inspect, format, test, or report. If a needed mutation cannot be represented as
a plan, stop and report the missing operation.

## Tool Rules

Before each phase:

1. Pull `sm-refactor` and `sm-refactor-rust` with `bbox_knowledge`.
2. Ground every file touched with `bbox_refactor_status` or an exact file read.
3. Use `bbox_refactor_plan` for single primitive edits or `bbox_refactor_run`
   for phase transactions.
4. Include required command steps in `bbox_refactor_run` when the phase must
   rollback on validation failure.
5. For mutating command steps such as formatters, include `touches` for every
   source path the command may rewrite. Validation-only commands can omit it.
6. Review plan JSON before confirming.

On phase failure: report the `bbox_refactor_run` response, including status,
failed step, error, and rollback errors, then pause. Do not run additional Bash
commands to debug a failed transaction. If the rollback report is incomplete,
say so and pause; the orchestrator owns any design correction loop.

Allowed generic plan kinds:

- `ensure_toml_table`
- `write_file`
- `replace_text`
- `move_file`

Allowed Rust plan kinds:

- `copy_rust_mod_decls`
- `rewrite_rust_mod_visibility`
- `rewrite_rust_item_visibility`
- `rust_lsp_rename`
- `rust_organize_imports`
- `delete_rust_items`
- `extract_rust_items`
- `extract_rust_impl_methods`
- `add_rust_mod_decl`
- `add_rust_use_decl`
- `add_rust_router_to_sum`

Checkpoint rule: after each phase, stop and report:

- phase number and status
- exact `bbox_refactor_run` status
- files written
- validation commands run
- remaining compile/test failures, if any, transcribed from the
  `bbox_refactor_run` response without re-running the failed command
- next phase you intend to run

Do not proceed to the next phase until the orchestrator resumes you.

## Phase 0: Baseline

Grounding:

```text
bbox_project_list()
bbox_project_register(path=<worktree>) if absent
bbox_knowledge(query="sm-refactor", project=<worktree>)
bbox_knowledge(query="sm-refactor-rust", project=<worktree>)
bbox_refactor_status(file="src/main.rs", project_dir=<worktree>, limit=80)
bbox_refactor_status(file="src/packets.rs", project_dir=<worktree>, limit=120)
```

Validation:

```text
cargo test --bin blackboxd
```

Checkpoint: pause after reporting the baseline result.

## Phase 1: Add Library Crate Shell

Goal: add `[lib]` and create a minimal `src/lib.rs` shell while leaving
`src/main.rs` runnable.

Grounding:

```text
bbox_refactor_status(file="src/main.rs", project_dir=<worktree>, item_kinds=["mod_item"], limit=80)
```

Plan shape:

```text
bbox_refactor_run(
  title="phase 1 lib shell",
  project_dir=<worktree>,
  confirm=true,
  steps=[
    {
      "op":"plan",
      "kind":"ensure_toml_table",
      "source":"Cargo.toml",
      "toml_table":"lib",
      "toml_entries":{"name":"blackbox","path":"src/lib.rs"}
    },
    {
      "op":"plan",
      "kind":"write_file",
      "source":"src/lib.rs",
      "new_text":"// Library crate shell. Modules move here after their dependencies are extracted.\n"
    },
    {"op":"command","command":"cargo","args":["fmt"],"touches":["src/lib.rs"],"required":true},
    {"op":"command","command":"cargo","args":["check","--lib"],"required":true},
    {"op":"command","command":"cargo","args":["check","--bin","blackboxd"],"required":true},
    {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
  ]
)
```

Notes:

- Do not copy all current `src/main.rs` module declarations into `src/lib.rs`
  in this phase. Many modules still depend on binary-root items such as
  `SharedState`, `BlackboxServer`, and route helpers.
- `src/lib.rs` can be a minimal shell. Add module declarations only after the
  relevant dependencies have moved or after `cargo check --lib` proves the
  declaration compiles.
- Keep `src/main.rs` module declarations in this phase. Removing or reparenting
  them is a later phase after the relevant module compiles from the library
  root.

Checkpoint: pause.

## Phase 2: Convert `packets.rs` To Module Directory

Goal: move `src/packets.rs` to `src/packets/mod.rs` without changing module
identity.

Grounding:

```text
bbox_refactor_status(file="src/packets.rs", project_dir=<worktree>, limit=120)
exact read eval/queries/cross-modal-rule-packets.json
```

Plan shape:

```text
bbox_refactor_run(
  title="phase 2 packets directory move",
  project_dir=<worktree>,
  confirm=true,
  steps=[
    {"op":"plan","kind":"move_file","source":"src/packets.rs","target":"src/packets/mod.rs"},
    {"op":"plan","kind":"replace_text","source":"eval/queries/cross-modal-rule-packets.json","old_text":"\"path_hint\": \"src/packets.rs\"","new_text":"\"path_hint\": \"src/packets/mod.rs\""},
    {"op":"plan","kind":"replace_text","source":"eval/queries/cross-modal-rule-packets.json","old_text":"project_file:d723917f:c4453102:e7e1c91d986cf6841a36d7101f858eb54313d94b287bc54fa238d28dbd1a2a67:8","new_text":"project_file:d723917f:a1d21fdd:e7e1c91d986cf6841a36d7101f858eb54313d94b287bc54fa238d28dbd1a2a67:8"},
    {"op":"command","command":"cargo","args":["fmt"],"touches":["src/packets/mod.rs"],"required":true},
    {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
  ]
)
```

Notes:

- The manifest updates are literal metadata repairs for a hard-coded eval
  fixture. Do not generalize them into symbolic rename behavior.
- The replacement `a1d21fdd` is the first 8 hex chars of
  `sha256("src/packets/mod.rs")`, matching the project-file relative path hash
  logic in `eval/check.rs`.

Checkpoint: pause.

## Phase 3: Split Packets Leaf Modules

Goal: extract low-risk packet layers first: `coerce.rs`, `events.rs`, and
`ast.rs`.

Grounding:

```text
bbox_refactor_status(file="src/packets/mod.rs", project_dir=<worktree>, limit=300)
```

Use exact reads/searches to identify the item names for:

- JSON string-to-structure coercion helpers
- packet event structs/functions
- predicate AST enums/structs and parsing helpers

Plan shape:

```text
bbox_refactor_run(
  title="phase 3 packets leaf modules",
  project_dir=<worktree>,
  confirm=true,
  steps=[
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/packets/mod.rs","module_name":"coerce"},
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/packets/mod.rs","module_name":"events"},
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/packets/mod.rs","module_name":"ast"},
    {"op":"plan","kind":"extract_rust_items","source":"src/packets/mod.rs","target":"src/packets/coerce.rs","item_names":[...],"target_prelude":"use super::*;\n"},
    {"op":"plan","kind":"extract_rust_items","source":"src/packets/mod.rs","target":"src/packets/events.rs","item_names":[...],"target_prelude":"use super::*;\nuse anyhow::{Context, Result};\nuse serde::{Deserialize, Serialize};\nuse std::fs;\nuse std::path::{Path, PathBuf};\n"},
    {"op":"plan","kind":"extract_rust_items","source":"src/packets/mod.rs","target":"src/packets/ast.rs","item_names":[...],"target_prelude":"use super::*;\nuse anyhow::Result;\nuse serde::{Deserialize, Serialize};\nuse std::collections::BTreeMap;\n"},
    {"op":"plan","kind":"rewrite_rust_item_visibility","source":"src/packets/coerce.rs","item_names":[...],"item_kinds":["function_item"],"visibility":"pub(super)"},
    {"op":"plan","kind":"rewrite_rust_item_visibility","source":"src/packets/events.rs","item_names":["packets_dir","scope_dir","packet_path","events_log_path","append_line","PacketEvent"],"visibility":"pub(super)"},
    {"op":"plan","kind":"rewrite_rust_item_visibility","source":"src/packets/events.rs","item_names":["now","with_packet_id","with_domain","with_details"],"item_kinds":["impl_method"],"impl_name":"impl PacketEvent","visibility":"pub(super)"},
    {"op":"plan","kind":"rewrite_rust_item_visibility","source":"src/packets/ast.rs","item_names":["Value","CmpOp","Predicate","Emit","Rule","RuleInput","review_lattice","review_prefix_inference","infer_classification","infer_classification_detailed","validate_path","validate_predicate","collect_apply_refs","rule_antecedents_referencing","collect_apply_by_id","check_apply_expect_against_lattice","default_confidence"],"visibility":"pub(super)"},
    {"op":"plan","kind":"rewrite_rust_item_visibility","source":"src/packets/ast.rs","item_names":["from_json","to_json"],"item_kinds":["impl_method"],"impl_name":"impl Value","visibility":"pub(super)"},
    {"op":"plan","kind":"rewrite_rust_item_visibility","source":"src/packets/ast.rs","item_names":["apply"],"item_kinds":["impl_method"],"impl_name":"impl CmpOp","visibility":"pub(super)"},
    {"op":"plan","kind":"rewrite_rust_item_visibility","source":"src/packets/ast.rs","item_names":["materialize"],"item_kinds":["impl_method"],"impl_name":"impl RuleInput","visibility":"pub(super)"},
    {"op":"plan","kind":"add_rust_use_decl","source":"src/packets/mod.rs","use_path":"ast::*","visibility":"pub"},
    {"op":"plan","kind":"add_rust_use_decl","source":"src/packets/mod.rs","use_path":"coerce::*"},
    {"op":"plan","kind":"add_rust_use_decl","source":"src/packets/mod.rs","use_path":"events::*","visibility":"pub"},
    {"op":"command","command":"cargo","args":["fmt"],"touches":["src/packets/mod.rs","src/packets/coerce.rs","src/packets/events.rs","src/packets/ast.rs"],"required":true},
    {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
  ]
)
```

If item extraction cannot express a helper because it is nested inside a test
module, use `replace_text` or `write_file` only after grounding the exact source
range and report the fallback.

Use `rewrite_rust_item_visibility` after extraction when parent code still calls
helpers or inherent methods moved into child modules. This is a structural Rust
visibility repair, not a literal text fallback.

Checkpoint: pause.

## Phase 4: Split Packets Core Evaluation Modules

Goal: extract `compile.rs`, `apply.rs`, `audit.rs`, and `scanner.rs`.

Grounding:

```text
bbox_refactor_status(file="src/packets/mod.rs", project_dir=<worktree>, limit=500)
bbox_refactor_status(file="src/packets/ast.rs", project_dir=<worktree>, limit=200)
```

Plan shape:

```text
bbox_refactor_run(
  title="phase 4 packets core modules",
  project_dir=<worktree>,
  confirm=true,
  steps=[
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/packets/mod.rs","module_name":"compile"},
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/packets/mod.rs","module_name":"apply"},
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/packets/mod.rs","module_name":"audit"},
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/packets/mod.rs","module_name":"scanner"},
    {"op":"plan","kind":"extract_rust_items","source":"src/packets/mod.rs","target":"src/packets/compile.rs","item_names":[...],"target_prelude":"use super::*;"},
    {"op":"plan","kind":"extract_rust_items","source":"src/packets/mod.rs","target":"src/packets/apply.rs","item_names":[...],"target_prelude":"use super::*;"},
    {"op":"plan","kind":"extract_rust_items","source":"src/packets/mod.rs","target":"src/packets/audit.rs","item_names":[...],"target_prelude":"use super::*;"},
    {"op":"plan","kind":"extract_rust_items","source":"src/packets/mod.rs","target":"src/packets/scanner.rs","item_names":[...],"target_prelude":"use super::*;"},
    {"op":"plan","kind":"add_rust_use_decl","source":"src/packets/mod.rs","use_path":"compile::*","visibility":"pub"},
    {"op":"plan","kind":"add_rust_use_decl","source":"src/packets/mod.rs","use_path":"apply::*","visibility":"pub"},
    {"op":"plan","kind":"add_rust_use_decl","source":"src/packets/mod.rs","use_path":"audit::*","visibility":"pub"},
    {"op":"plan","kind":"add_rust_use_decl","source":"src/packets/mod.rs","use_path":"scanner::*","visibility":"pub"},
    {"op":"command","command":"cargo","args":["fmt"],"touches":["src/packets/mod.rs","src/packets/compile.rs","src/packets/apply.rs","src/packets/audit.rs","src/packets/scanner.rs"],"required":true},
    {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
  ]
)
```

Checkpoint: pause.

## Phase 5: Extract Server State And Progress

Goal: introduce `src/server/` and move low-risk server subsystems.

Grounding:

```text
bbox_refactor_status(file="src/main.rs", project_dir=<worktree>, limit=300)
```

Plan shape:

```text
bbox_refactor_run(
  title="phase 5 server state progress",
  project_dir=<worktree>,
  confirm=true,
  steps=[
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/lib.rs","module_name":"server","visibility":"pub"},
    {"op":"plan","kind":"write_file","source":"src/server/mod.rs","new_text":"pub mod state;\npub mod progress;\n\npub use state::*;\npub use progress::*;\n"},
    {"op":"plan","kind":"extract_rust_items","source":"src/main.rs","target":"src/server/state.rs","item_names":[...],"target_prelude":"use crate::*;\nuse super::*;"},
    {"op":"plan","kind":"extract_rust_items","source":"src/main.rs","target":"src/server/progress.rs","item_names":[...],"target_prelude":"use crate::*;\nuse super::*;"},
    {"op":"command","command":"cargo","args":["fmt"],"touches":["src/lib.rs","src/server/mod.rs","src/server/state.rs","src/server/progress.rs","src/main.rs"],"required":true},
    {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
  ]
)
```

Use `replace_text` for explicit path/import adjustments reported by the compiler.

Checkpoint: pause.

## Phase 6: Extract Tool Domains

Goal: create `src/tools/` and move one coherent tool domain at a time.

Grounding per domain:

```text
bbox_refactor_status(file="src/main.rs", project_dir=<worktree>, item_kinds=["impl_method"], limit=500)
```

Start with a small domain such as projects or notes. For each domain:

```text
bbox_refactor_run(
  title="phase 6 tools bootstrap and <domain>",
  project_dir=<worktree>,
  confirm=true,
  steps=[
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/lib.rs","module_name":"tools","visibility":"pub"},
    {"op":"plan","kind":"write_file","source":"src/tools/mod.rs","new_text":"pub mod <domain>;\n"},
    {"op":"plan","kind":"extract_rust_impl_methods","source":"src/main.rs","target":"src/tools/<domain>.rs","item_names":[...],"item_kinds":["impl_method"],"impl_name":"impl BlackboxServer","router_name":"<domain>_tools","router_export_name":"router","target_prelude":"use crate::*;\nuse crate::server::*;"},
    {"op":"plan","kind":"add_rust_router_to_sum","source":"src/main.rs","router_call":"tools::<domain>::router()"},
    {"op":"command","command":"cargo","args":["fmt"],"touches":["src/lib.rs","src/tools/mod.rs","src/tools/<domain>.rs","src/main.rs"],"required":true},
    {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
  ]
)
```

For later domains, first ground `src/tools/mod.rs` and use either
`add_rust_mod_decl` or `replace_text` to add exactly one new `pub mod <domain>;`
line without rewriting existing domain declarations.

Checkpoint after each domain. Do not batch many domains in one checkpoint.

## Phase 7: Extract HTTP Routes And Tail

Goal: move route helpers and SSE tail endpoint out of `main.rs`.

Grounding:

```text
bbox_refactor_status(file="src/main.rs", project_dir=<worktree>, limit=500)
```

Plan shape:

```text
bbox_refactor_run(
  title="phase 7 routes tail",
  project_dir=<worktree>,
  confirm=true,
  steps=[
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/server/mod.rs","module_name":"routes","visibility":"pub"},
    {"op":"plan","kind":"add_rust_mod_decl","source":"src/server/mod.rs","module_name":"tail","visibility":"pub"},
    {"op":"plan","kind":"extract_rust_items","source":"src/main.rs","target":"src/server/routes.rs","item_names":[...],"target_prelude":"use crate::*;\nuse super::*;"},
    {"op":"plan","kind":"extract_rust_items","source":"src/main.rs","target":"src/server/tail.rs","item_names":[...],"target_prelude":"use crate::*;\nuse super::*;"},
    {"op":"command","command":"cargo","args":["fmt"],"touches":["src/server/mod.rs","src/server/routes.rs","src/server/tail.rs","src/main.rs"],"required":true},
    {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
  ]
)
```

Checkpoint: pause.

## Phase 8: Shrink `main.rs`

Goal: leave `main.rs` as daemon bootstrap only and route shared code through the
library.

Grounding:

```text
bbox_refactor_status(file="src/main.rs", project_dir=<worktree>, limit=500)
```

Plan shape:

```text
bbox_refactor_run(
  title="phase 8 shrink main",
  project_dir=<worktree>,
  confirm=true,
  steps=[
    {"op":"plan","kind":"write_file","source":"src/main.rs","new_text":"<bootstrap-only main.rs assembled from grounded remaining bootstrap code>"},
    {"op":"plan","kind":"rust_organize_imports","source":"src/main.rs"},
    {"op":"command","command":"cargo","args":["fmt"],"touches":["src/main.rs"],"required":true},
    {"op":"command","command":"cargo","args":["check","--all-targets"],"required":true},
    {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
  ]
)
```

Use `rust_lsp_rename` for any actual symbol rename discovered during this
phase. Use `replace_text` only for a bounded literal edit where the intended
change is textual rather than binding-aware, and report it explicitly as a
literal fallback.

Checkpoint: pause.

## Phase 9: Final Validation

Run:

```text
cargo fmt --check
cargo check --all-targets
cargo test --bin blackboxd
```

Then report:

- final file layout
- remaining large files by `wc -l`
- any remaining manual cleanup recommendations
- whether every mutation was performed through `bbox_refactor_*`

Checkpoint: final pause.
