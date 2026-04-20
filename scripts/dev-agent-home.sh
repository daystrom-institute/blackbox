#!/usr/bin/env bash
set -euo pipefail

resolve_repo_root() {
  if [[ -n "${BBOX_DEV_REPO_ROOT:-}" ]]; then
    printf '%s\n' "$BBOX_DEV_REPO_ROOT"
    return 0
  fi

  if git_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    printf '%s\n' "$git_root"
    return 0
  fi

  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  printf '%s\n' "$(cd "$script_dir/.." && pwd)"
}

repo_root="$(resolve_repo_root)"
dev_root="${BBOX_DEV_ROOT:-$repo_root/.dev-agent}"
dev_home="${BBOX_DEV_HOME:-$dev_root/home}"
host_links_file="${BBOX_DEV_HOST_LINKS_FILE:-$repo_root/.dev-agent-links}"

export HOME="$dev_home"
export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
export XDG_STATE_HOME="${XDG_STATE_HOME:-$HOME/.local/state}"
export XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"

export BBOX_PORT="${BBOX_PORT:-7265}"
export BLACKBOX_MCP_NAME="${BLACKBOX_MCP_NAME:-blackbox-dev}"
export BLACKBOX_STATE_DIR="${BLACKBOX_STATE_DIR:-$XDG_STATE_HOME/blackbox-dev}"
export BLACKBOX_KNOWLEDGE_PATH="${BLACKBOX_KNOWLEDGE_PATH:-$BLACKBOX_STATE_DIR/blackbox-knowledge.json}"
export BLACKBOX_THREADS_PATH="${BLACKBOX_THREADS_PATH:-$BLACKBOX_STATE_DIR/blackbox-threads.json}"
export BLACKBOX_NOTES_PATH="${BLACKBOX_NOTES_PATH:-$BLACKBOX_STATE_DIR/blackbox-notes.json}"
export BLACKBOX_PACKETS_DIR="${BLACKBOX_PACKETS_DIR:-$BLACKBOX_STATE_DIR/packets}"
export BLACKBOX_BACKUP_DIR="${BLACKBOX_BACKUP_DIR:-$BLACKBOX_STATE_DIR/backups}"
export TRANSCRIPT_SEARCH_INDEX_PATH="${TRANSCRIPT_SEARCH_INDEX_PATH:-$XDG_DATA_HOME/blackbox-dev/index}"
export TRANSCRIPT_SEARCH_ROOTS="${TRANSCRIPT_SEARCH_ROOTS:-claude=$HOME/.claude}"
export TRANSCRIPT_SEARCH_CODEX_ROOT="${TRANSCRIPT_SEARCH_CODEX_ROOT:-$HOME/.codex}"
export BRO_HOME="${BRO_HOME:-$BLACKBOX_STATE_DIR/bro}"
export BLACKBOX_GLOBAL_CLAUDE_MD="${BLACKBOX_GLOBAL_CLAUDE_MD:-$HOME/.claude-shared/CLAUDE.md}"
export BLACKBOX_GLOBAL_CODEX_MD="${BLACKBOX_GLOBAL_CODEX_MD:-$HOME/.codex/AGENTS.md}"
export BLACKBOX_GLOBAL_GEMINI_MD="${BLACKBOX_GLOBAL_GEMINI_MD:-$HOME/.gemini/GEMINI.md}"
export RUST_LOG="${RUST_LOG:-blackbox=info}"
export PATH="$HOME/.local/bin:$PATH"

ensure_base_tree() {
  mkdir -p \
    "$HOME" \
    "$HOME/.claude" \
    "$HOME/.claude-shared" \
    "$HOME/.codex" \
    "$HOME/.gemini" \
    "$HOME/.local/bin" \
    "$XDG_CONFIG_HOME" \
    "$XDG_STATE_HOME" \
    "$XDG_DATA_HOME" \
    "$BLACKBOX_STATE_DIR" \
    "$BLACKBOX_PACKETS_DIR" \
    "$BLACKBOX_BACKUP_DIR" \
    "$BRO_HOME"
}

claude_config_json() {
  cat <<EOF
{
  "mcpServers": {
    "${BLACKBOX_MCP_NAME}": {
      "type": "http",
      "url": "http://127.0.0.1:${BBOX_PORT}/mcp"
    }
  }
}
EOF
}

write_if_missing() {
  local path="$1"
  local body="$2"
  if [[ -e "$path" ]]; then
    return 0
  fi
  mkdir -p "$(dirname "$path")"
  printf '%s\n' "$body" >"$path"
}

write_seed_configs() {
  local claude_json
  claude_json="$(claude_config_json)"
  write_if_missing "$HOME/.claude.json" "$claude_json"
  write_if_missing "$HOME/.claude/.claude.json" "$claude_json"
  write_if_missing "$HOME/.codex/config.toml" "[mcp_servers.\"${BLACKBOX_MCP_NAME}\"]"$'\n'"url = \"http://127.0.0.1:${BBOX_PORT}/mcp\""
  write_if_missing "$HOME/.gemini/GEMINI.md" "<!-- bb:managed-start -->"$'\n'"<!-- dev-agent placeholder; blackbox render owns this region -->"$'\n'"<!-- bb:managed-end -->"
  write_if_missing "$HOME/.claude-shared/CLAUDE.md" "<!-- bb:managed-start -->"$'\n'"<!-- dev-agent placeholder; blackbox render owns this region -->"$'\n'"<!-- bb:managed-end -->"
  write_if_missing "$HOME/.codex/AGENTS.md" "<!-- bb:managed-start -->"$'\n'"<!-- dev-agent placeholder; blackbox render owns this region -->"$'\n'"<!-- bb:managed-end -->"
}

apply_host_links() {
  if [[ ! -f "$host_links_file" ]]; then
    return 0
  fi

  while IFS=$'\t' read -r rel_path source_path _; do
    [[ -z "${rel_path:-}" ]] && continue
    [[ "${rel_path:0:1}" == "#" ]] && continue
    [[ -z "${source_path:-}" ]] && {
      printf 'dev-agent-home: skipping malformed line in %s: %s\n' "$host_links_file" "$rel_path" >&2
      continue
    }
    if [[ "${rel_path:0:1}" == "/" ]]; then
      printf 'dev-agent-home: relative path must stay under dev HOME: %s\n' "$rel_path" >&2
      continue
    fi
    if [[ ! -e "$source_path" ]]; then
      printf 'dev-agent-home: source missing, not linking %s -> %s\n' "$rel_path" "$source_path" >&2
      continue
    fi

    local target="$HOME/$rel_path"
    mkdir -p "$(dirname "$target")"
    rm -rf "$target"
    ln -s "$source_path" "$target"
  done <"$host_links_file"
}

print_env() {
  cat <<EOF
repo_root=$repo_root
dev_root=$dev_root
HOME=$HOME
XDG_CONFIG_HOME=$XDG_CONFIG_HOME
XDG_STATE_HOME=$XDG_STATE_HOME
XDG_DATA_HOME=$XDG_DATA_HOME
BBOX_PORT=$BBOX_PORT
BLACKBOX_MCP_NAME=$BLACKBOX_MCP_NAME
BLACKBOX_STATE_DIR=$BLACKBOX_STATE_DIR
TRANSCRIPT_SEARCH_INDEX_PATH=$TRANSCRIPT_SEARCH_INDEX_PATH
BLACKBOX_GLOBAL_CLAUDE_MD=$BLACKBOX_GLOBAL_CLAUDE_MD
BLACKBOX_GLOBAL_CODEX_MD=$BLACKBOX_GLOBAL_CODEX_MD
BLACKBOX_GLOBAL_GEMINI_MD=$BLACKBOX_GLOBAL_GEMINI_MD
BRO_HOME=$BRO_HOME
TRANSCRIPT_SEARCH_ROOTS=$TRANSCRIPT_SEARCH_ROOTS
TRANSCRIPT_SEARCH_CODEX_ROOT=$TRANSCRIPT_SEARCH_CODEX_ROOT
host_links_file=$host_links_file
EOF
}

init_home() {
  ensure_base_tree
  write_seed_configs
  apply_host_links
}

usage() {
  cat <<'EOF'
usage:
  dev-agent-home.sh init
  dev-agent-home.sh env
  dev-agent-home.sh exec <command> [args...]

notes:
  - state lives under ./.dev-agent by default
  - optional host auth passthrough comes from ./.dev-agent-links
  - .dev-agent-links lines are TAB-separated: <relative-path-under-dev-home><TAB><absolute-host-path>
EOF
}

cmd="${1:-}"
case "$cmd" in
  init)
    init_home
    print_env
    ;;
  env)
    init_home
    print_env
    ;;
  exec)
    shift
    if [[ $# -eq 0 ]]; then
      usage
      exit 1
    fi
    init_home
    exec "$@"
    ;;
  *)
    usage
    exit 1
    ;;
esac
