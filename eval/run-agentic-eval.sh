#!/usr/bin/env bash
# Agentic corpus evaluation harness.
# Usage: eval/run-agentic-eval.sh [all|failed] [trials] [parallel]
# By default, the harness runs LLM commands in an isolated git worktree to
# contain accidental file modifications from eval agents. Set
# EVAL_USE_WORKTREE=0 to run directly in the current checkout. The default LLM
# command is `codex exec --dangerously-bypass-approvals-and-sandbox`; set
# EVAL_LLM_CMD to use another provider/command.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
QUERY_DIR="$SCRIPT_DIR/queries"
OUT_ROOT="${EVAL_OUT_DIR:-$SCRIPT_DIR/eval-output}"
TIMESTAMP="${EVAL_TIMESTAMP:-$(date +%Y%m%d-%H%M%S)}"
RUN_DIR="$OUT_ROOT/$TIMESTAMP"
MCP_URL="${EVAL_MCP_URL:-http://127.0.0.1:${BBOX_DEV_PORT:-7265}/mcp}"
MODE="${1:-all}"
TRIALS="${2:-1}"
PARALLEL="${3:-1}"
STRATEGIES="${EVAL_STRATEGIES:-search-only,static-hybrid,agentic}"
LIMIT="${EVAL_LIMIT:-0}"
EVAL_USE_WORKTREE="${EVAL_USE_WORKTREE:-1}"
LLM_WORKDIR="$REPO_ROOT"
EVAL_WORKTREE_DIR=""

mkdir -p "$RUN_DIR"

setup_eval_worktree() {
    if [[ "$EVAL_USE_WORKTREE" != "1" ]]; then
        return 0
    fi
    EVAL_WORKTREE_DIR="${EVAL_WORKTREE_PATH:-/tmp/agentic-eval-worktree-${TIMESTAMP}-$$}"
    git -C "$REPO_ROOT" worktree add --detach "$EVAL_WORKTREE_DIR" HEAD >/dev/null
    LLM_WORKDIR="$EVAL_WORKTREE_DIR"
}

cleanup_eval_worktree() {
    if [[ -n "$EVAL_WORKTREE_DIR" && -d "$EVAL_WORKTREE_DIR" ]]; then
        git -C "$REPO_ROOT" worktree remove --force "$EVAL_WORKTREE_DIR" >/dev/null 2>&1 || true
    fi
}

check_dev_daemon() {
    if [[ "${EVAL_SKIP_DEV_CHECK:-0}" == "1" ]]; then
        return 0
    fi
    local code
    code="$(curl -s -o /dev/null -w '%{http_code}' "$MCP_URL" || true)"
    if [[ "$code" == "000" || "$code" -ge 500 ]]; then
        echo "ERROR: blackboxd-dev MCP endpoint is not reachable at $MCP_URL (HTTP $code)" >&2
        echo "Start blackbox-dev.service or set EVAL_MCP_URL/EVAL_SKIP_DEV_CHECK." >&2
        exit 1
    fi
}

manifest_list() {
    python3 - "$QUERY_DIR" "$MODE" "$OUT_ROOT/latest-failed.txt" "$LIMIT" <<'PY'
import json, pathlib, sys
query_dir = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
failed_path = pathlib.Path(sys.argv[3])
limit = int(sys.argv[4])
paths = sorted(query_dir.glob("*.json"))
if mode == "failed" and failed_path.exists():
    failed = {line.strip() for line in failed_path.read_text().splitlines() if line.strip()}
    paths = [p for p in paths if p.stem in failed]
elif mode not in {"all", "failed"}:
    raise SystemExit("usage: run-agentic-eval.sh [all|failed] [trials] [parallel]")
if limit > 0:
    paths = paths[:limit]
for path in paths:
    print(path)
PY
}

prompt_for() {
    local manifest="$1"
    local strategy="$2"
    python3 - "$manifest" "$strategy" "$MCP_URL" <<'PY'
import json, pathlib, sys
manifest = json.loads(pathlib.Path(sys.argv[1]).read_text())
strategy = sys.argv[2]
mcp_url = sys.argv[3]
common = f"""You are running the blackbox agentic-corpus eval suite against blackboxd-dev at {mcp_url}.
Return ONLY a JSON object with this shape:
{{"answer": "...", "collected_entity_refs": ["..."], "path_ids": [], "notes": "..."}}

Question: {manifest["query"]}
Expected evidence type: {manifest["required_evidence"]}
Forbidden stale answers: {manifest.get("forbidden_stale_answers", [])}
"""
if strategy == "search-only":
    instructions = "Use only bbox_search. Collect canonical entity refs from the search result metadata or snippets when present."
elif strategy == "static-hybrid":
    instructions = "Call bbox_hybrid_search exactly once. Do not inspect or traverse. Collect entity_id values from the result rows."
else:
    instructions = """Use the full agentic loop:
1. bbox_discover_seed_entities for seeds.
2. bbox_inspect_entity on the best 1-3 seeds.
3. bbox_find_paths when the question asks for provenance, a chain, or cross-modal evidence.
4. bbox_bundle_evidence with the final entity_refs and path_ids.
Collect entity_refs from the evidence bundle."""
print(common + "\nStrategy: " + strategy + "\n" + instructions)
PY
}

run_llm() {
    local prompt_file="$1"
    local raw_file="$2"
    local strategy="$3"
    local manifest="$4"
    if [[ "${EVAL_LLM_CMD:-}" == "__oracle__" ]]; then
        python3 - "$manifest" <<'PY' >"$raw_file"
import json, pathlib, sys
m = json.loads(pathlib.Path(sys.argv[1]).read_text())
print(json.dumps({
    "answer": "oracle fixture",
    "collected_entity_refs": m.get("expected_entity_refs", []),
    "path_ids": [],
    "notes": "EVAL_LLM_CMD=__oracle__"
}))
PY
        return 0
    fi
    if [[ "${EVAL_LLM_CMD:-}" == "__empty__" ]]; then
        printf '{"answer":"empty fixture","collected_entity_refs":[],"path_ids":[],"notes":"EVAL_LLM_CMD=__empty__"}\n' >"$raw_file"
        return 0
    fi
    local prompt_text
    prompt_text="$(<"$prompt_file")"
    if [[ -n "${EVAL_LLM_CMD:-}" ]]; then
        (
            cd "$LLM_WORKDIR"
            BLACKBOX_MCP_URL="$MCP_URL" BLACKBOX_MCP_NAME="blackbox-dev" \
                bash -lc "$EVAL_LLM_CMD" <"$prompt_file" >"$raw_file"
        )
    else
        BLACKBOX_MCP_URL="$MCP_URL" BLACKBOX_MCP_NAME="blackbox-dev" \
            codex exec --dangerously-bypass-approvals-and-sandbox -C "$LLM_WORKDIR" "$prompt_text" >"$raw_file"
    fi
    [[ -s "$raw_file" ]]
}

score_one() {
    local manifest="$1"
    local strategy="$2"
    local trial="$3"
    local raw_file="$4"
    local verdict_file="$5"
    python3 - "$manifest" "$strategy" "$trial" "$raw_file" "$verdict_file" "${EVAL_SYNTHETIC_REGRESSION:-0}" <<'PY'
import json, pathlib, re, sys
manifest_path, strategy, trial, raw_path, verdict_path, synthetic = sys.argv[1:]
manifest = json.loads(pathlib.Path(manifest_path).read_text())
raw = pathlib.Path(raw_path).read_text(errors="replace")

def extract_json(text):
    start = text.find("{")
    while start != -1:
        depth = 0
        for idx in range(start, len(text)):
            ch = text[idx]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    try:
                        return json.loads(text[start:idx+1])
                    except Exception:
                        break
        start = text.find("{", start + 1)
    return {}

payload = extract_json(raw)
refs = payload.get("collected_entity_refs") or []
if not isinstance(refs, list):
    refs = []
if not refs:
    pattern = r"\b(?:knowledge|project_file|symbol|transcript|session|thread|note|brofile|whiteboard|commit|task|bash_call):[A-Za-z0-9_./:@|+~=-]+"
    refs = re.findall(pattern, raw)
refs = list(dict.fromkeys(str(ref).rstrip(".,;)") for ref in refs))

expected = list(manifest.get("expected_entity_refs", []))
if synthetic == "1" and manifest["id"] == sorted([manifest["id"]])[0]:
    expected = ["knowledge:synthetic-regression-missing-ref"]
strictness = manifest.get("pass_strictness", "any")
if strictness == "all":
    passed = all(ref in refs for ref in expected)
elif strictness == "first":
    passed = bool(expected) and expected[0] in refs
else:
    passed = any(ref in refs for ref in expected)
forbidden_hits = [needle for needle in manifest.get("forbidden_stale_answers", []) if needle and needle in raw]
if forbidden_hits:
    passed = False

verdict = {
    "manifest_id": manifest["id"],
    "query_class": manifest["query_class"],
    "strategy": strategy,
    "trial": int(trial),
    "query": manifest["query"],
    "pass_strictness": strictness,
    "expected_entity_refs": expected,
    "collected_entity_refs": refs,
    "path_ids": payload.get("path_ids", []) if isinstance(payload, dict) else [],
    "passed": passed,
    "forbidden_hits": forbidden_hits,
    "raw_output": str(raw_path),
}
pathlib.Path(verdict_path).write_text(json.dumps(verdict, indent=2) + "\n")
print("PASS" if passed else "FAIL")
PY
}

run_one() {
    local manifest="$1"
    local strategy="$2"
    local trial="$3"
    local id
    id="$(basename "$manifest" .json)"
    local prefix="$RUN_DIR/${id}-${strategy}-t${trial}"
    local prompt_file="${prefix}.prompt.txt"
    local raw_file="${prefix}.raw.txt"
    local verdict_file="${prefix}.verdict.json"
    prompt_for "$manifest" "$strategy" >"$prompt_file"
    if run_llm "$prompt_file" "$raw_file" "$strategy" "$manifest"; then
        local result
        result="$(score_one "$manifest" "$strategy" "$trial" "$raw_file" "$verdict_file")"
        echo "$result $id $strategy t$trial"
    else
        python3 - "$manifest" "$strategy" "$trial" "$raw_file" "$verdict_file" <<'PY'
import json, pathlib, sys
m = json.loads(pathlib.Path(sys.argv[1]).read_text())
pathlib.Path(sys.argv[5]).write_text(json.dumps({
  "manifest_id": m["id"], "query_class": m["query_class"], "strategy": sys.argv[2],
  "trial": int(sys.argv[3]), "query": m["query"], "pass_strictness": m.get("pass_strictness", "any"),
  "expected_entity_refs": m.get("expected_entity_refs", []), "collected_entity_refs": [],
  "path_ids": [], "passed": False, "error": "llm command failed", "raw_output": sys.argv[4]
}, indent=2) + "\n")
PY
        echo "FAIL $id $strategy t$trial"
    fi
}

aggregate() {
    python3 - "$RUN_DIR" "$OUT_ROOT" "${EVAL_BASELINE_PASS_RATE:-}" <<'PY'
import collections, json, pathlib, sys
run_dir = pathlib.Path(sys.argv[1])
out_root = pathlib.Path(sys.argv[2])
baseline_raw = sys.argv[3]
verdicts = [json.loads(path.read_text()) for path in sorted(run_dir.glob("*.verdict.json"))]
by_strategy = collections.defaultdict(lambda: [0, 0])
by_class = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0]))
failed = set()
for v in verdicts:
    key = v["strategy"]
    by_strategy[key][1] += 1
    by_strategy[key][0] += int(bool(v.get("passed")))
    by_class[key][v["query_class"]][1] += 1
    by_class[key][v["query_class"]][0] += int(bool(v.get("passed")))
    if not v.get("passed"):
        failed.add(v["manifest_id"])

scoreboard = {
    "status": "ok",
    "total_verdicts": len(verdicts),
    "strategies": {
        k: {"passed": p, "total": t, "pass_rate": (p / t if t else 0.0)}
        for k, (p, t) in sorted(by_strategy.items())
    },
    "by_query_class": {
        strategy: {
            cls: {"passed": p, "total": t, "pass_rate": (p / t if t else 0.0)}
            for cls, (p, t) in sorted(classes.items())
        }
        for strategy, classes in sorted(by_class.items())
    },
}
agentic = scoreboard["strategies"].get("agentic")
if baseline_raw and agentic:
    baseline = float(baseline_raw)
    delta_pp = abs(agentic["pass_rate"] - baseline) * 100.0
    verdict = "stable" if delta_pp <= 5.0 else "drift_minor" if delta_pp <= 10.0 else "drift_major"
    scoreboard["drift"] = {"baseline": baseline, "agentic": agentic["pass_rate"], "delta_pp": delta_pp, "verdict": verdict}
score_path = run_dir / "scoreboard.json"
score_path.write_text(json.dumps(scoreboard, indent=2) + "\n")
(out_root / "latest.json").write_text(json.dumps(scoreboard, indent=2) + "\n")
(out_root / "latest-failed.txt").write_text("\n".join(sorted(failed)) + ("\n" if failed else ""))
print(json.dumps(scoreboard, indent=2))
PY
}

main() {
    check_dev_daemon
    setup_eval_worktree
    trap cleanup_eval_worktree EXIT
    mapfile -t manifests < <(manifest_list)
    IFS=',' read -r -a strategies <<<"$STRATEGIES"
    if [[ "${#manifests[@]}" -eq 0 ]]; then
        echo "No manifests selected." >&2
        exit 1
    fi
    echo "=== Agentic corpus eval: $TIMESTAMP ==="
    echo "MCP: $MCP_URL"
    echo "Strategies: $STRATEGIES  Manifests: ${#manifests[@]}  Trials: $TRIALS  Parallel: $PARALLEL"
    echo "Output: $RUN_DIR"

    declare -a pids=()
    for manifest in "${manifests[@]}"; do
        for strategy in "${strategies[@]}"; do
            for trial in $(seq 1 "$TRIALS"); do
                run_one "$manifest" "$strategy" "$trial" &
                pids+=("$!")
                if [[ "${#pids[@]}" -ge "$PARALLEL" ]]; then
                    wait "${pids[0]}"
                    pids=("${pids[@]:1}")
                fi
            done
        done
    done
    for pid in "${pids[@]}"; do
        wait "$pid"
    done
    aggregate
}

main "$@"
