#!/usr/bin/env bash
# Install the global Blackbox agent skills on this host.
#
# Source: system-defaults/skills/<name>/ in this repository (one directory per
# skill, holding SKILL.md and its supporting files).
#
# Each skill is copied once into the shared skills directory
# ($HOME/.agents/skills by default), which Codex reads whatever CODEX_HOME is.
# Every Claude Code home ($HOME/.claude and $HOME/.claude-*) then gets a
# symlink to that copy. Install each name in exactly one place: providers do
# not merge same-name skills found in two scanned directories.
#
# Only skills this script installed are ever replaced or removed. An installed
# copy carries a .bbox-skill marker, and a symlink is managed only when it
# already points at the shared copy. Anything else at a target path is
# reported as a conflict and left alone. An installed copy that was edited in
# place is drift and is overwritten from the source.
#
# Usage: scripts/sync-skills.sh [--check] [--claude-home DIR]...
#   --check            report what would change, write nothing, exit 1 on drift
#   --claude-home DIR  link into this Claude home instead of the detected set
#                      (repeatable)
# Environment:
#   BBOX_SKILLS_SOURCE  source directory (default: system-defaults/skills)
#   BBOX_SKILLS_DIR     shared install directory (default: $HOME/.agents/skills)
set -euo pipefail

MARKER=".bbox-skill"
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
source_dir=${BBOX_SKILLS_SOURCE:-"$(dirname -- "$script_dir")/system-defaults/skills"}
install_dir=${BBOX_SKILLS_DIR:-"$HOME/.agents/skills"}

check=0
claude_homes=()
while [ $# -gt 0 ]; do
    case "$1" in
        --check) check=1 ;;
        --claude-home)
            [ $# -ge 2 ] || { echo "sync-skills: --claude-home needs a directory" >&2; exit 2; }
            claude_homes+=("$2")
            shift
            ;;
        -h|--help)
            sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "sync-skills: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

[ -d "$source_dir" ] || { echo "sync-skills: no skill source at $source_dir" >&2; exit 2; }

if [ ${#claude_homes[@]} -eq 0 ]; then
    for home in "$HOME/.claude" "$HOME"/.claude-*; do
        [ -d "$home" ] && claude_homes+=("$home")
    done
fi

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi
}

# Digest of a skill directory's file names and contents, ignoring the marker.
skill_digest() {
    (
        cd "$1"
        find . -type f ! -name "$MARKER" | LC_ALL=C sort | while IFS= read -r file; do
            printf '%s\n' "$file"
            sha256 <"$file"
        done
    ) | sha256 | cut -d' ' -f1
}

changes=0
conflicts=0

note() {
    changes=$((changes + 1))
    if [ "$check" -eq 1 ]; then echo "would $*"; else echo "$*"; fi
}

conflict() {
    conflicts=$((conflicts + 1))
    echo "conflict: $*" >&2
}

names=()
for skill in "$source_dir"/*/; do
    [ -f "${skill}SKILL.md" ] || continue
    names+=("$(basename "$skill")")
done
[ ${#names[@]} -gt 0 ] || { echo "sync-skills: no skills under $source_dir" >&2; exit 2; }

is_source_skill() {
    local candidate
    for candidate in "${names[@]}"; do
        [ "$candidate" = "$1" ] && return 0
    done
    return 1
}

# 1. Shared copies.
for name in "${names[@]}"; do
    src="$source_dir/$name"
    dst="$install_dir/$name"
    digest=$(skill_digest "$src")
    if [ -L "$dst" ] || { [ -e "$dst" ] && [ ! -f "$dst/$MARKER" ]; }; then
        conflict "$dst exists and was not installed by this script"
        continue
    fi
    if [ -f "$dst/$MARKER" ] && [ "$(skill_digest "$dst")" = "$digest" ]; then
        continue
    fi
    if [ -d "$dst" ]; then note "update $dst"; else note "install $dst"; fi
    if [ "$check" -eq 0 ]; then
        mkdir -p "$install_dir"
        staged="$install_dir/.$name.sync.$$"
        rm -rf "$staged"
        cp -R "$src" "$staged"
        printf '%s\n' "$digest" >"$staged/$MARKER"
        rm -rf "$dst"
        mv "$staged" "$dst"
    fi
done

# 2. Shared copies whose skill left the source.
if [ -d "$install_dir" ]; then
    for dst in "$install_dir"/*/; do
        [ -f "${dst}$MARKER" ] || continue
        name=$(basename "$dst")
        is_source_skill "$name" && continue
        note "remove ${dst%/}"
        [ "$check" -eq 0 ] && rm -rf "${dst%/}"
    done
fi

# 3. Claude homes: one symlink per skill, and no links to removed skills.
for home in ${claude_homes[@]+"${claude_homes[@]}"}; do
    [ -d "$home" ] || { conflict "$home is not a directory"; continue; }
    for name in "${names[@]}"; do
        link="$home/skills/$name"
        target="$install_dir/$name"
        if [ -L "$link" ]; then
            [ "$(readlink "$link")" = "$target" ] && continue
            conflict "$link points somewhere else"
            continue
        elif [ -e "$link" ]; then
            conflict "$link exists and is not a symlink"
            continue
        fi
        note "link $link"
        if [ "$check" -eq 0 ]; then
            mkdir -p "$home/skills"
            ln -sfn "$target" "$link"
        fi
    done
    [ -d "$home/skills" ] || continue
    for link in "$home/skills"/*; do
        [ -L "$link" ] || continue
        [ "$(dirname "$(readlink "$link")")" = "$install_dir" ] || continue
        is_source_skill "$(basename "$link")" && continue
        note "unlink $link"
        [ "$check" -eq 0 ] && rm -f "$link"
    done
done

if [ "$conflicts" -gt 0 ]; then
    echo "sync-skills: $conflicts conflict(s) left untouched" >&2
    exit 1
fi
if [ "$check" -eq 1 ] && [ "$changes" -gt 0 ]; then
    echo "sync-skills: $changes change(s) pending"
    exit 1
fi
[ "$changes" -gt 0 ] || echo "sync-skills: ${#names[@]} skills current in $install_dir, ${#claude_homes[@]} Claude home(s) linked"
