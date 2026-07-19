#!/usr/bin/env bash
# Phase 4 concurrency enforcement backstop — concurrency-model §5.
#
# clippy's disallowed_methods (clippy.toml + the src/tools module deny) is the
# primary gate, but it is syntactic and cannot express two handler-shape
# rules. This script covers them:
#
#   1. No NEW sync #[tool] handlers in src/tools/. A sync handler runs its
#      whole closure inline on a tokio worker (the Self::run path); new
#      handlers must be `async fn` and route real work through
#      Self::run_blocking. The pre-existing cheap in-memory readers are
#      allowlisted in the python block — do not grow that list without a
#      reason.
#
#   2. No thread spawns in src/tools/. Sanctioned actor threads live in their
#      owner modules (store_persister, index/writer_actor, server/background,
#      ...), never in tool handlers.
#
# Deliberately dependency-free (python3 + grep only — rg is not guaranteed on
# every host), and fail-loud: a matcher error fails the lint rather than
# passing vacuously.

set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'EOF'
import re, sys, pathlib

ALLOWLIST = {
    # Baseline: sync handlers present when Phase 4 landed (wave 16). These
    # are in-memory reads / control ops verified non-blocking at baseline
    # time (wave-13 inventory + wave-16 sweep; the two disk-writing sync
    # handlers, bbox_packet_gap and bro_slack_bind, were converted instead
    # of baselined). Do NOT add names here without a reasoned review —
    # convert the handler to async + run_blocking instead.
    "atom_delegate", "atom_describe", "atom_get", "atom_list", "atom_search",
    "atom_status", "badgey_close_loops", "badgey_collect", "badgey_dismiss",
    "badgey_list", "badgey_proposals_list", "badgey_status",
    "badgey_triage_inbox", "bbox_artifact_list", "bbox_describe_schema",
    "bbox_embed_status", "bbox_gaps", "bbox_mcp_surface", "bbox_notes",
    "bbox_project_list", "bbox_thread_list",
    "bro_agent_describe", "bro_agent_get", "bro_agent_list",
    "bro_agent_search", "bro_allocator_probe", "bro_allocator_status",
    "bro_allocator_trace", "bro_brofile", "bro_cancel", "bro_council_list",
    "bro_council_open", "bro_council_posts", "bro_dashboard",
    "bro_interrupt", "bro_mcp", "bro_providers", "bro_prune", "bro_report",
    "bro_retro", "bro_slack_link_lookup", "bro_slack_link_record",
    "bro_status", "bro_steer", "macro_validate", "tool_identity_get",
    "tool_identity_list", "tool_system_event_list", "tool_system_event_open",
}

fail = False
tools = sorted(pathlib.Path("src/tools").rglob("*.rs"))
if not tools:
    print("lint-concurrency: src/tools/*.rs not found — wrong cwd?", file=sys.stderr)
    sys.exit(2)

# Rule 1: a #[tool(...)] attribute followed by a NON-async handler fn.
handler = re.compile(
    r"#\[tool\((?:[^\]]|\](?!\s*\n\s*(?:pub|async|fn)))*?\)\]\s*"
    r"(?:pub(?:\([a-z]+\))?\s+)?(async\s+)?fn\s+([a-z_0-9]+)",
    re.S,
)
total = 0
for path in tools:
    src = path.read_text()
    for m in handler.finditer(src):
        total += 1
        is_async, name = bool(m.group(1)), m.group(2)
        if not is_async and name not in ALLOWLIST:
            line = src[: m.start()].count("\n") + 1
            print(
                f"error: {path}:{line} — sync #[tool] handler '{name}'; make it"
                " 'async fn' and put blocking work in Self::run_blocking"
                " (concurrency-model §3 I2)",
                file=sys.stderr,
            )
            fail = True

# Self-check: the matcher must see the tool surface at all. If it matches
# nothing, the regex rotted — fail loudly instead of passing vacuously.
if total < 50:
    print(
        f"lint-concurrency: matcher self-check failed — only {total} #[tool]"
        " handlers matched (expected 100+); the regex or layout drifted",
        file=sys.stderr,
    )
    sys.exit(2)

# Rule 2: thread spawns inside tool handlers.
spawn = re.compile(r"std::thread::(spawn|Builder)")
for path in tools:
    lines = path.read_text().splitlines()
    for i, linetext in enumerate(lines, 1):
        if spawn.search(linetext):
            # Reasoned inline exemption (annotation on the preceding line).
            if i >= 2 and "lint-concurrency: allow(thread-spawn)" in lines[i - 2]:
                continue
            print(
                f"error: {path}:{i} — thread spawn inside src/tools/;"
                " sanctioned actor threads live in their owner modules"
                " (concurrency-model §3 I3)",
                file=sys.stderr,
            )
            fail = True

if fail:
    print(
        "\nlint-concurrency: FAILED — see design/daemon-runtime/"
        "concurrency-model.md §5 (Phase 4)",
        file=sys.stderr,
    )
    sys.exit(1)
print(f"lint-concurrency: ok ({total} #[tool] handlers checked)")
EOF
