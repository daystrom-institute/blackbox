#!/usr/bin/env bash
set -euo pipefail

readonly BASE_REF="monolith-decomposition-pre-attempt-2"
readonly -a REVIEW_TOOLS=(
  Read
  Glob
  Grep
  Task
  Agent
  'Bash(git status:*)'
  'Bash(git diff:*)'
  'Bash(git show:*)'
  'Bash(git log:*)'
  'Bash(git rev-parse:*)'
  'Bash(git merge-base:*)'
  'Bash(git tag:*)'
  'Bash(rtk git status:*)'
  'Bash(rtk git diff:*)'
  'Bash(rtk git show:*)'
  'Bash(rtk git log:*)'
  'Bash(rtk git rev-parse:*)'
  'Bash(rtk git merge-base:*)'
  'Bash(rtk git tag:*)'
  'Bash(rg:*)'
  'Bash(grep:*)'
  'Bash(sed:*)'
  'Bash(awk:*)'
  'Bash(find:*)'
  'Bash(ls:*)'
  'Bash(wc:*)'
  'Bash(head:*)'
  'Bash(tail:*)'
  'Bash(cat:*)'
  'Bash(rtk rg:*)'
  'Bash(rtk grep:*)'
  'Bash(rtk sed:*)'
  'Bash(rtk awk:*)'
  'Bash(rtk find:*)'
  'Bash(rtk ls:*)'
  'Bash(rtk wc:*)'
  'Bash(rtk head:*)'
  'Bash(rtk tail:*)'
  'Bash(rtk cat:*)'
)

usage() {
  cat <<'EOF'
usage:
  scripts/kimi-review.sh review
  scripts/kimi-review.sh resume [session-id]
  scripts/kimi-review.sh plan-review
  scripts/kimi-review.sh plan-resume [session-id]

The code-review scope is fixed by prompts/agents/kimi-review.md. The plan-review
scope is fixed by prompts/agents/kimi-plan-review.md. The most recent session
ID for each mode is recorded per Git worktree, so resume normally needs no
argument. Pass an explicit session ID only when resuming an older review from
the same worktree.
EOF
}

die() {
  printf 'kimi-review: %s\n' "$*" >&2
  exit 1
}

resolve_repo_root() {
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  git -C "$script_dir/.." rev-parse --show-toplevel
}

new_session_id() {
  command -v uuidgen >/dev/null 2>&1 || die "uuidgen is required"
  uuidgen | tr '[:upper:]' '[:lower:]'
}

validate_session_id() {
  local session_id="$1"
  [[ "$session_id" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-8][0-9a-fA-F]{3}-[89aAbB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$ ]] ||
    die "invalid session ID: $session_id"
}

repo_root="$(resolve_repo_root)"
prompt_file="$repo_root/prompts/agents/kimi-review.md"
session_file="$(git -C "$repo_root" rev-parse --git-path kimi-review-session)"
plan_prompt_file="$repo_root/prompts/agents/kimi-plan-review.md"
plan_session_file="$(git -C "$repo_root" rev-parse --git-path kimi-plan-review-session)"
claudew="$(command -v claudew || true)"

[[ -n "$claudew" ]] || die "claudew is not on PATH"
[[ -f "$prompt_file" ]] || die "review prompt is missing: $prompt_file"
[[ -f "$plan_prompt_file" ]] || die "plan review prompt is missing: $plan_prompt_file"
[[ -d "${HOME}/.claude-k" ]] || die "Kimi config directory is missing: ${HOME}/.claude-k"
git -C "$repo_root" rev-parse --verify "${BASE_REF}^{commit}" >/dev/null 2>&1 ||
  die "fixed baseline tag is missing: $BASE_REF"
git -C "$repo_root" merge-base --is-ancestor "$BASE_REF" HEAD ||
  die "fixed baseline $BASE_REF is not an ancestor of HEAD"

cd "$repo_root"

# Reviewer fan-outs routinely outlive the 600s print-mode background-wait
# ceiling, which kills the round with no verdict issued. Disable the ceiling
# for every claudew invocation here; an explicit caller value still wins.
export CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS="${CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS:-0}"

verb="${1:-}"
case "$verb" in
  review)
    [[ $# -eq 1 ]] || die "review accepts no arguments"
    session_id="$(new_session_id)"
    mkdir -p "$(dirname "$session_file")"
    printf '%s\n' "$session_id" >"$session_file"
    printf 'kimi-review: session %s\n' "$session_id" >&2
    if ! CLAUDE_CONFIG_DIR="${HOME}/.claude-k" \
      "$claudew" \
      --permission-mode dontAsk \
      --allowedTools "${REVIEW_TOOLS[@]}" \
      --append-system-prompt-file "$prompt_file" \
      --session-id "$session_id" \
      --name "kimi-full-scope-review" \
      --disallowedTools Edit Write NotebookEdit \
      -p "Perform the complete mandatory review now."; then
      if [[ -f "$session_file" ]] && [[ "$(tr -d '[:space:]' <"$session_file")" == "$session_id" ]]; then
        rm -f "$session_file"
      fi
      exit 1
    fi
    ;;
  resume)
    [[ $# -le 2 ]] || die "resume accepts only an optional session ID"
    if [[ $# -eq 2 ]]; then
      session_id="$2"
    else
      [[ -f "$session_file" ]] ||
        die "no recorded review session; pass an explicit session ID"
      session_id="$(tr -d '[:space:]' <"$session_file")"
    fi
    validate_session_id "$session_id"
    printf 'kimi-review: resuming session %s\n' "$session_id" >&2
    CLAUDE_CONFIG_DIR="${HOME}/.claude-k" \
      "$claudew" \
      --permission-mode dontAsk \
      --allowedTools "${REVIEW_TOOLS[@]}" \
      --append-system-prompt-file "$prompt_file" \
      --resume "$session_id" \
      --disallowedTools Edit Write NotebookEdit \
      -p "Reinspect the complete mandatory scope. Reproduce and recheck every prior finding against the actual current code, verify each claimed fix, and search the entire scope for regressions and newly exposed defects. Do not rely on a fix summary and do not narrow the review to files changed since the previous turn."
    ;;
  plan-review)
    [[ $# -eq 1 ]] || die "plan-review accepts no arguments"
    session_id="$(new_session_id)"
    mkdir -p "$(dirname "$plan_session_file")"
    printf '%s\n' "$session_id" >"$plan_session_file"
    printf 'kimi-review: plan session %s\n' "$session_id" >&2
    if ! CLAUDE_CONFIG_DIR="${HOME}/.claude-k" \
      "$claudew" \
      --permission-mode dontAsk \
      --allowedTools "${REVIEW_TOOLS[@]}" \
      --append-system-prompt-file "$plan_prompt_file" \
      --session-id "$session_id" \
      --name "kimi-durable-project-catalog-plan-review" \
      --disallowedTools Edit Write NotebookEdit \
      -p "Perform the complete mandatory plan review now."; then
      if [[ -f "$plan_session_file" ]] && [[ "$(tr -d '[:space:]' <"$plan_session_file")" == "$session_id" ]]; then
        rm -f "$plan_session_file"
      fi
      exit 1
    fi
    ;;
  plan-resume)
    [[ $# -le 2 ]] || die "plan-resume accepts only an optional session ID"
    if [[ $# -eq 2 ]]; then
      session_id="$2"
    else
      [[ -f "$plan_session_file" ]] ||
        die "no recorded plan review session; pass an explicit session ID"
      session_id="$(tr -d '[:space:]' <"$plan_session_file")"
    fi
    validate_session_id "$session_id"
    printf 'kimi-review: resuming plan session %s\n' "$session_id" >&2
    CLAUDE_CONFIG_DIR="${HOME}/.claude-k" \
      "$claudew" \
      --permission-mode dontAsk \
      --allowedTools "${REVIEW_TOOLS[@]}" \
      --append-system-prompt-file "$plan_prompt_file" \
      --resume "$session_id" \
      --disallowedTools Edit Write NotebookEdit \
      -p "Reinspect the complete mandatory plan scope. Reproduce every prior plan finding against the current document and actual code, verify each correction, and search the entire plan for new gaps. Do not rely on a fix summary and do not narrow review to paragraphs changed since the previous turn."
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 1
    ;;
esac
