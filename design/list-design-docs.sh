#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: design/list-design-docs.sh [proposed|partial ...]

Lists markdown design docs whose frontmatter has:
  kind: design
  lifecycle: proposed or partial

With no arguments, lists both proposed and partial docs.
EOF
}

case "${1:-}" in
  -h|--help)
    usage
    exit 0
    ;;
esac

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

if (($# == 0)); then
  lifecycles=("proposed" "partial")
else
  lifecycles=("$@")
fi

for lifecycle in "${lifecycles[@]}"; do
  case "$lifecycle" in
    proposed|partial) ;;
    *)
      printf 'error: unsupported lifecycle %q (expected proposed or partial)\n' "$lifecycle" >&2
      exit 2
      ;;
  esac
done

find "$script_dir" -type f -name '*.md' -print0 |
  sort -z |
  while IFS= read -r -d '' file; do
    rel="${file#"$repo_root"/}"
    awk -v file="$rel" -v wanted_csv="$(IFS=,; echo "${lifecycles[*]}")" '
      BEGIN {
        split(wanted_csv, wanted, ",")
        for (i in wanted) want[wanted[i]] = 1
      }
      NR == 1 && $0 == "---" {
        in_fm = 1
        next
      }
      in_fm && $0 == "---" {
        if (kind == "design" && lifecycle in want) {
          printf "%-8s %s", lifecycle, file
          if (title != "") {
            printf "  %s", title
          }
          printf "\n"
        }
        exit
      }
      in_fm {
        if ($0 ~ /^kind:[[:space:]]*/) {
          value = $0
          sub(/^kind:[[:space:]]*/, "", value)
          gsub(/^"|"$/, "", value)
          kind = value
        } else if ($0 ~ /^lifecycle:[[:space:]]*/) {
          value = $0
          sub(/^lifecycle:[[:space:]]*/, "", value)
          sub(/[[:space:]]*#.*/, "", value)
          gsub(/^"|"$/, "", value)
          lifecycle = value
        } else if ($0 ~ /^title:[[:space:]]*/) {
          value = $0
          sub(/^title:[[:space:]]*/, "", value)
          gsub(/^"|"$/, "", value)
          title = value
        }
      }
    ' "$file"
  done
