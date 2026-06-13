#!/usr/bin/env bash
# Run a refactor-v2 live probe through bro-harness's isolate/code-mode surface.
# Provider-specific wrappers set the model/base-url defaults; this runner never
# falls back to bbox/MCP tools.
set -euo pipefail

usage() {
  cat <<'EOF'
usage:
  scripts/isolate-probe-glm.sh [options]
  scripts/isolate-probe-ds.sh  [options]

required target selection:
  --scratch-from <repo>   create a detached git worktree from <repo> and probe there
  --cwd <dir>             probe an existing disposable checkout

prompt selection:
  --prompt-file <file>    use an explicit prompt
  otherwise the java-complex-chain prompt is generated from:
    PROBE_SOURCE_FILE     workspace-relative Java source path
    PROBE_TARGET_FILE     workspace-relative extracted class path
    PROBE_CLASS_NAME      extracted class name
    PROBE_DELEGATE_FIELD  delegate field name
    PROBE_COMPILE_CMD     compile command to run after each apply

optional generated-prompt inputs:
  PROBE_METHODS           comma-separated method names; omit to let the agent choose a seam
  PROBE_MOVE_FIELDS       comma-separated field names; omit to use the cohesion cluster's fields
  PROBE_SEAM_HINT         prose hint for selecting the cohesion cluster

provider/auth:
  --model <model>         override provider default
  --effort <effort>       default: high
  --service-tier <tier>   optional harness service tier
  --shell-env <json>      override shell child env; default passes PATH/HOME/JAVA_HOME
  --build                 cargo build -p bro-harness before running

output/worktree:
  --out-dir <dir>         default: /tmp/isolate-probe-<provider>-<timestamp>
  --scratch-ref <ref>     ref used with --scratch-from; default: HEAD
  --cleanup               remove created scratch worktree after a successful run
  --dry-run               write prompt and print command, but do not run harness

auth discovery:
  Provider wrappers read env first, then a Claude settings dir if present:
    GLM:      ~/.claude-zai/settings.json
    DeepSeek: ~/.claude-ds/settings.json
  Required final env is ANTHROPIC_BASE_URL and one of ANTHROPIC_AUTH_TOKEN or
  ANTHROPIC_API_KEY.
EOF
}

die() {
  printf 'isolate-probe: %s\n' "$*" >&2
  exit 1
}

repo_root() {
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  (cd "$script_dir/.." && git rev-parse --show-toplevel)
}

csv_json_array() {
  local raw="${1:-}"
  python3 - "$raw" <<'PY'
import json, sys
items = [part.strip() for part in sys.argv[1].split(",") if part.strip()]
print(json.dumps(items))
PY
}

json_string() {
  python3 - "$1" <<'PY'
import json, sys
print(json.dumps(sys.argv[1]))
PY
}

detect_java_home() {
  if [[ -n "${JAVA_HOME:-}" && -x "${JAVA_HOME}/bin/java" ]]; then
    printf '%s\n' "$JAVA_HOME"
    return 0
  fi
  if [[ -x /usr/libexec/java_home ]]; then
    local mac_java_home
    mac_java_home="$(/usr/libexec/java_home 2>/dev/null || true)"
    if [[ -n "$mac_java_home" && -x "$mac_java_home/bin/java" ]]; then
      printf '%s\n' "$mac_java_home"
      return 0
    fi
  fi
  if command -v brew >/dev/null 2>&1; then
    local formula prefix candidate
    for formula in openjdk openjdk@21 openjdk@17; do
      prefix="$(brew --prefix "$formula" 2>/dev/null || true)"
      [[ -n "$prefix" ]] || continue
      for candidate in \
        "$prefix/libexec/openjdk.jdk/Contents/Home" \
        "$prefix"; do
        if [[ -x "$candidate/bin/java" ]]; then
          printf '%s\n' "$candidate"
          return 0
        fi
      done
    done
  fi
  return 1
}

build_shell_env_json() {
  local java_home="${1:-}"
  local child_path="${PATH:-}"
  if [[ -n "$java_home" ]]; then
    child_path="$java_home/bin:$child_path"
  fi
  python3 - "$child_path" "$java_home" <<'PY'
import json, os, sys
child_path, java_home = sys.argv[1], sys.argv[2]
env = {
    "PATH": child_path,
    "HOME": os.environ.get("HOME", ""),
}
if java_home:
    env["JAVA_HOME"] = java_home
for key in ["GRADLE_USER_HOME", "MAVEN_OPTS", "JAVA_TOOL_OPTIONS"]:
    value = os.environ.get(key)
    if value:
        env[key] = value
print(json.dumps(env, separators=(",", ":")))
PY
}

load_anthropic_env_from_settings() {
  local settings_dir="$1"
  local settings="$settings_dir/settings.json"
  [[ -f "$settings" ]] || return 0
  command -v python3 >/dev/null 2>&1 || {
    printf 'isolate-probe: python3 missing; cannot read %s\n' "$settings" >&2
    return 0
  }
  local exports
  exports="$(python3 - "$settings" <<'PY'
import json, os, shlex, sys
path = sys.argv[1]
try:
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
except Exception:
    sys.exit(0)
env = data.get("env") or {}
for key in [
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_VERSION",
    "ANTHROPIC_MODEL",
]:
    if os.environ.get(key):
        continue
    value = env.get(key)
    if value:
        print(f"export {key}={shlex.quote(str(value))}")
PY
)"
  if [[ -n "$exports" ]]; then
    eval "$exports"
  fi
}

write_generated_prompt() {
  local prompt_path="$1"
  : "${PROBE_SOURCE_FILE:?PROBE_SOURCE_FILE is required without --prompt-file}"
  : "${PROBE_TARGET_FILE:?PROBE_TARGET_FILE is required without --prompt-file}"
  : "${PROBE_CLASS_NAME:?PROBE_CLASS_NAME is required without --prompt-file}"
  : "${PROBE_DELEGATE_FIELD:?PROBE_DELEGATE_FIELD is required without --prompt-file}"
  : "${PROBE_COMPILE_CMD:?PROBE_COMPILE_CMD is required without --prompt-file}"

  local methods_json move_fields_json seam_hint_json
  methods_json="$(csv_json_array "${PROBE_METHODS:-}")"
  move_fields_json="$(csv_json_array "${PROBE_MOVE_FIELDS:-}")"
  seam_hint_json="$(json_string "${PROBE_SEAM_HINT:-}")"

  cat >"$prompt_path" <<EOF
Run the refactor-v2 complex live probe through the isolate bindings only.

Hard constraints:
- Use code-mode bindings: analysis.*, java.*, edits.*, code.*, shell_run/exec.
- Do not use MCP tools, bbox_refactor_* tools, or manual source rewrites.
- Do not hand-edit files; all source changes must flow through java.* + edits.apply.
- Keep private repository details out of final prose except paths already supplied in this prompt.
- Compile gates are terminal. If either compile exits nonzero, call final_result immediately
  with the failure details and do not run later refactor/cleanup steps.

Probe inputs:
- source_file: ${PROBE_SOURCE_FILE}
- target_file: ${PROBE_TARGET_FILE}
- class_name: ${PROBE_CLASS_NAME}
- delegate_field: ${PROBE_DELEGATE_FIELD}
- compile_command: ${PROBE_COMPILE_CMD}
- method_names: ${methods_json}
- move_fields: ${move_fields_json}
- seam_hint: ${seam_hint_json}

Required chain:
1. Call analysis.describe({analysis:"cohesionClusters"}), analysis.describe({analysis:"references"}),
   java.describe({transform:"extractClass"}), and
   java.describe({transform:"removeUnusedConstructorParams"}).
2. Call analysis.cohesionClusters({file: source_file}). If method_names is empty, choose a clean
   high-score delegate-shaped multi-method seam; use seam_hint when present. If method_names is
   non-empty, use exactly those methods and use move_fields when provided; otherwise use the
   cohesion cluster's move_fields for that seam.
3. Call analysis.references for the selected method names with kinds:["method_invocation"].
   Set wrappers=true only if there are callers outside source_file.
4. Call java.extractClass with file, target, className, delegateField, methods, moveFields,
   and wrappers. Leave wiring unset so the transform auto-selects DI external_injection when
   the source is container-managed.
5. Apply via edits.begin -> edits.createFile for creates -> edits.merge for changes -> edits.apply.
6. Run compile_command with shell_run/exec and require exit_code=0. If exit_code is nonzero,
   stop the chain immediately and call final_result with applied_cleanup=false.
7. Call java.removeUnusedConstructorParams({file: source_file}); if it returns changes, apply them
   via edits.begin -> edits.merge -> edits.apply.
8. Run compile_command again and require exit_code=0. If exit_code is nonzero, stop immediately
   and call final_result with the cleanup compile failure.
9. Inspect source_file and verify whether the final constructor parameter list is multiline.

Call final_result with the required fields. Include concise counts for bounces, applies, compiles,
selected methods, moved fields, wrappers, cleanup removed params, and constructor_multiline_after_cleanup.
EOF
}

provider=""
default_model=""
default_base_url=""
default_settings_dir=""
model=""
effort="${ISOLATE_PROBE_EFFORT:-high}"
service_tier="${BRO_HARNESS_SERVICE_TIER:-}"
shell_env_json="${ISOLATE_PROBE_SHELL_ENV_JSON:-}"
probe_cwd=""
scratch_from=""
scratch_ref="${PROBE_SCRATCH_REF:-HEAD}"
prompt_file=""
out_dir=""
build_first=false
cleanup_success=false
dry_run=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --provider) provider="$2"; shift 2 ;;
    --default-model) default_model="$2"; shift 2 ;;
    --default-base-url) default_base_url="$2"; shift 2 ;;
    --default-settings-dir) default_settings_dir="$2"; shift 2 ;;
    --model) model="$2"; shift 2 ;;
    --effort) effort="$2"; shift 2 ;;
    --service-tier) service_tier="$2"; shift 2 ;;
    --shell-env) shell_env_json="$2"; shift 2 ;;
    --cwd) probe_cwd="$2"; shift 2 ;;
    --scratch-from) scratch_from="$2"; shift 2 ;;
    --scratch-ref) scratch_ref="$2"; shift 2 ;;
    --prompt-file) prompt_file="$2"; shift 2 ;;
    --out-dir) out_dir="$2"; shift 2 ;;
    --build) build_first=true; shift ;;
    --cleanup) cleanup_success=true; shift ;;
    --dry-run) dry_run=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$provider" ]] || die "--provider missing; use a provider wrapper"
[[ -n "$default_model" ]] || die "--default-model missing; use a provider wrapper"
[[ -n "$default_base_url" ]] || die "--default-base-url missing; use a provider wrapper"
[[ -n "$default_settings_dir" ]] || die "--default-settings-dir missing; use a provider wrapper"
[[ -z "$probe_cwd" || -z "$scratch_from" ]] || die "pass only one of --cwd or --scratch-from"
[[ -n "$probe_cwd" || -n "$scratch_from" ]] || die "pass --scratch-from <repo> or --cwd <dir>"

root="$(repo_root)"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
out_dir="${out_dir:-/tmp/isolate-probe-${provider}-${timestamp}}"
mkdir -p "$out_dir"

load_anthropic_env_from_settings "$default_settings_dir"
export BRO_HARNESS_PROVIDER="${BRO_HARNESS_PROVIDER:-$provider}"
export BRO_HARNESS_TRANSPORT="${BRO_HARNESS_TRANSPORT:-anthropic}"
export ANTHROPIC_BASE_URL="${ANTHROPIC_BASE_URL:-$default_base_url}"
model="${model:-${BRO_HARNESS_MODEL:-${ANTHROPIC_MODEL:-$default_model}}}"

if [[ "$dry_run" != true ]]; then
  [[ -n "${ANTHROPIC_BASE_URL:-}" ]] || die "ANTHROPIC_BASE_URL is required"
  [[ -n "${ANTHROPIC_AUTH_TOKEN:-}" || -n "${ANTHROPIC_API_KEY:-}" ]] || \
    die "ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY is required"
fi

if [[ -z "$shell_env_json" ]]; then
  java_home="$(detect_java_home || true)"
  shell_env_json="$(build_shell_env_json "$java_home")"
fi

created_worktree=false
if [[ -n "$scratch_from" ]]; then
  scratch_from="$(cd "$scratch_from" && pwd)"
  probe_cwd="${out_dir}/worktree"
  if [[ "$dry_run" != true ]]; then
    git -C "$scratch_from" worktree add --detach "$probe_cwd" "$scratch_ref" >/dev/null
    created_worktree=true
  fi
else
  probe_cwd="$(cd "$probe_cwd" && pwd)"
fi

prompt_path="$out_dir/prompt.md"
if [[ -n "$prompt_file" ]]; then
  prompt_file="$(cd "$(dirname "$prompt_file")" && pwd)/$(basename "$prompt_file")"
  cp "$prompt_file" "$prompt_path"
else
  write_generated_prompt "$prompt_path"
fi

schema_path="$out_dir/final-result.schema.json"
cat >"$schema_path" <<'EOF'
{
  "type": "object",
  "properties": {
    "applied_extract": { "type": "boolean" },
    "applied_cleanup": { "type": "boolean" },
    "bounces": { "type": "integer" },
    "compile_after_extract": { "type": "string" },
    "compile_after_cleanup": { "type": "string" },
    "constructor_multiline_after_cleanup": { "type": "boolean" },
    "used_analysis_references": { "type": "boolean" },
    "used_cohesion_clusters": { "type": "boolean" },
    "used_extract_class": { "type": "boolean" },
    "used_remove_unused_constructor_params": { "type": "boolean" },
    "wrappers": { "type": "boolean" },
    "summary": { "type": "string" }
  },
  "required": [
    "applied_extract",
    "applied_cleanup",
    "bounces",
    "compile_after_extract",
    "compile_after_cleanup",
    "constructor_multiline_after_cleanup",
    "used_analysis_references",
    "used_cohesion_clusters",
    "used_extract_class",
    "used_remove_unused_constructor_params",
    "wrappers",
    "summary"
  ],
  "additionalProperties": true
}
EOF

if [[ "$dry_run" != true ]]; then
  if [[ "$build_first" == true || ! -x "$root/target/debug/bro-harness" ]]; then
    cargo build --manifest-path "$root/Cargo.toml" -p bro-harness
  fi
  [[ -x "$root/target/debug/bro-harness" ]] || die "bro-harness binary missing after build"
fi

out_jsonl="$out_dir/out.jsonl"
err_log="$out_dir/err.log"
cmd=(
  "$root/target/debug/bro-harness"
  --model "$model"
  --cwd "$probe_cwd"
  --code-mode only
  --dangerously-skip-permissions
  --shell-env "$shell_env_json"
  --output-schema "$(tr -d '\n' <"$schema_path")"
  --prompt "$(cat "$prompt_path")"
)
if [[ -n "$effort" ]]; then
  cmd+=(--effort "$effort")
fi
if [[ -n "$service_tier" ]]; then
  cmd+=(--service-tier "$service_tier")
fi

cat >"$out_dir/run.env" <<EOF
provider=$provider
model=$model
transport=$BRO_HARNESS_TRANSPORT
base_url=$ANTHROPIC_BASE_URL
probe_cwd=$probe_cwd
prompt_path=$prompt_path
out_jsonl=$out_jsonl
err_log=$err_log
EOF

printf 'isolate-probe: provider=%s model=%s cwd=%s\n' "$provider" "$model" "$probe_cwd"
printf 'isolate-probe: out=%s\n' "$out_dir"

if [[ "$dry_run" == true ]]; then
  printf 'isolate-probe: dry run; command not executed\n'
  printf '%q ' "${cmd[@]}"
  printf '\n'
  exit 0
fi

set +e
"${cmd[@]}" >"$out_jsonl" 2>"$err_log"
status=$?
set -e

if [[ $status -eq 0 && "$cleanup_success" == true && "$created_worktree" == true ]]; then
  git -C "$scratch_from" worktree remove --force "$probe_cwd" >/dev/null || true
fi

if [[ $status -ne 0 ]]; then
  printf 'isolate-probe: harness failed with exit %s\n' "$status" >&2
  printf 'isolate-probe: stderr: %s\n' "$err_log" >&2
  exit "$status"
fi

printf 'isolate-probe: complete\n'
printf 'isolate-probe: stdout jsonl: %s\n' "$out_jsonl"
printf 'isolate-probe: stderr log:   %s\n' "$err_log"
