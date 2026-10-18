#!/usr/bin/env bash
# Register the global Blackbox agent skills with the host's Agent Service Desk.
#
# Source: system-defaults/skills/<name>/ in this repository (one directory per
# skill, holding SKILL.md and its supporting files).
#
# The Agent Service Desk (ASD) daemon owns installation: it links each skill
# directory, by name, into every registered skill root and keeps those links
# current. This script declares Blackbox's complete set as publisher
# "blackbox", together with the roots it should reach: the shared skills
# directory ($HOME/.agents/skills by default), which Codex reads whatever
# CODEX_HOME is, and every Claude Code home's skills directory ($HOME/.claude
# and $HOME/.claude-*). ASD unregisters Blackbox skills that left the source
# and never changes other publishers' registrations or anything it did not
# install. Roots are shared with other publishers' skills.
#
# Copies made by the earlier copy-based installer carry a .bbox-skill marker.
# ASD adopts them: each copy is moved under its data directory and replaced by
# the managed link. Anything else at a destination is reported as a conflict
# and left alone.
#
# Links point at the source directory, which must stay in place: run this
# from the checkout that keeps the skills (or name it with BBOX_SKILLS_SOURCE),
# not from a session worktree that will be removed.
#
# Usage: scripts/sync-skills.sh [--check] [--claude-home DIR]...
#   --check            compare ASD's registration and the links on disk with
#                      this source, write nothing, exit 1 on drift
#   --claude-home DIR  register this Claude home instead of the detected set
#                      (repeatable)
# Environment:
#   BBOX_SKILLS_SOURCE  source directory (default: system-defaults/skills)
#   BBOX_SKILLS_DIR     shared skill root (default: $HOME/.agents/skills)
#   ASD_URL             ASD daemon (default: $INTENT_BOARD_URL, else
#                       http://localhost:7717); it must run on this host
# Requires curl and python3. Exit status: 0 current, 1 conflict or drift,
# 2 usage or source error, 3 ASD unreachable or refused the request.
set -euo pipefail

MARKER=".bbox-skill"
PUBLISHER="blackbox"
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
source_dir=${BBOX_SKILLS_SOURCE:-"$(dirname -- "$script_dir")/system-defaults/skills"}
install_dir=${BBOX_SKILLS_DIR:-"$HOME/.agents/skills"}
asd_url=${ASD_URL:-${INTENT_BOARD_URL:-http://localhost:7717}}
asd_url=${asd_url%/}

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
            sed -n '2,35p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "sync-skills: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

for tool in curl python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "sync-skills: $tool is required" >&2; exit 2; }
done
[ -d "$source_dir" ] || { echo "sync-skills: no skill source at $source_dir" >&2; exit 2; }
source_dir=$(CDPATH='' cd -- "$source_dir" && pwd)

if [ ${#claude_homes[@]} -eq 0 ]; then
    for home in "$HOME/.claude" "$HOME"/.claude-*; do
        [ -d "$home" ] && claude_homes+=("$home")
    done
fi

names=()
for skill in "$source_dir"/*/; do
    [ -f "${skill}SKILL.md" ] || continue
    names+=("$(basename "$skill")")
done
[ ${#names[@]} -gt 0 ] || { echo "sync-skills: no skills under $source_dir" >&2; exit 2; }

# The request body: skills, then targets, one per line, for python to encode.
request=$(
    {
        for name in "${names[@]}"; do printf 'skill\t%s\t%s\n' "$name" "$source_dir/$name"; done
        printf 'target\tshared\t%s\n' "$install_dir"
        for home in ${claude_homes[@]+"${claude_homes[@]}"}; do
            printf 'target\tclaude\t%s\n' "$(CDPATH='' cd -- "$home" 2>/dev/null && pwd || printf '%s' "$home")/skills"
        done
    } | python3 -I -c '
import json, sys
marker, publisher = sys.argv[1], sys.argv[2]
skills, targets = [], []
for line in sys.stdin.read().splitlines():
    kind, a, b = line.split("\t", 2)
    if kind == "skill":
        skills.append({"id": a, "source": b, "adopt_marker": marker})
    else:
        targets.append({"path": b, "provider": a})
print(json.dumps({"publisher": publisher, "skills": skills, "targets": targets}))
' "$MARKER" "$PUBLISHER"
)

# Prints the response body; exit 3 with the reason when ASD is unreachable or
# refuses the request.
call() {
    local tool=$1 body=$2 response status
    if ! response=$(curl -sS --max-time 60 -X POST -H 'content-type: application/json' \
        -w '\n%{http_code}' --data-binary "$body" \
        "$asd_url/api/tools/$tool?as=blackbox-installer" 2>&1); then
        echo "sync-skills: Agent Service Desk is not reachable at $asd_url ($response)." >&2
        echo "sync-skills: start the ASD daemon on this host (or set ASD_URL) and rerun; nothing was registered." >&2
        exit 3
    fi
    status=${response##*$'\n'}
    response=${response%$'\n'*}
    if [ "$status" != 200 ]; then
        echo "sync-skills: ASD refused $tool (HTTP $status): $response" >&2
        exit 3
    fi
    printf '%s' "$response"
}

if [ "$check" -eq 1 ]; then
    current=$(call skill_list "{\"publisher\":\"$PUBLISHER\",\"fresh\":true}")
    printf '%s' "$current" | python3 -I -c '
import json, os, sys
want = json.loads(sys.argv[1])
have = json.loads(sys.stdin.read())["result"]
registered = {s["id"]: s for s in have["skills"]}
# ASD treats paths resolving to the same directory as one root.
roots = {os.path.realpath(t["path"]) for t in have["targets"]} | {t["canonical"] for t in have["targets"]}
drift = 0
def report(text):
    global drift
    drift += 1
    print(text)
for skill in want["skills"]:
    found = registered.pop(skill["id"], None)
    if not found:
        report("would register " + skill["id"])
    elif found["source"]["path"] != skill["source"] or found["adopt_marker"] != skill["adopt_marker"]:
        report("would update " + skill["id"])
    else:
        for install in found["installs"]:
            if install["state"] not in ("installed", "disabled", "aliased"):
                report("%s: %s %s: %s" % (skill["id"], install["state"], install["destination"], install["detail"]))
for name in registered:
    report("would unregister " + name)
for target in want["targets"]:
    if os.path.realpath(target["path"]) not in roots:
        report("would add target " + target["path"])
for item in have.get("pending_removals", []):
    report("would unlink %s (%s)" % (item["destination"], item["skill_id"]))
for item in have["cleanup"]:
    report("cleanup pending: %s: %s" % (item["destination"], item["error"]))
if have.get("reconcile_error"):
    report("ASD reconciliation failed: " + have["reconcile_error"])
if drift:
    print("sync-skills: %d item(s) pending" % drift)
    sys.exit(1)
print("sync-skills: %d skills current" % len(want["skills"]))
' "$request"
    exit $?
fi

result=$(call skill_publish "$request")
printf '%s' "$result" | python3 -I -c '
import json, sys
result = json.loads(sys.stdin.read())["result"]
problems = 0
for target in result["targets_created"]:
    print("added target #%d" % target)
for name in result["unregistered"]:
    print("unregistered " + name)
for path in result["removed"]:
    print("unlinked " + path)
for skill in result["skills"]:
    for install in skill["installs"]:
        state = install["state"]
        if state in ("conflict", "source_missing", "error", "pending"):
            problems += 1
            print("%s: %s at %s: %s" % (state, skill["id"], install["destination"], install["detail"]), file=sys.stderr)
        elif install["detail"].startswith("Adopted"):
            print("adopted %s at %s" % (skill["id"], install["destination"]))
for item in result["cleanup"]:
    problems += 1
    print("cleanup pending: %s: %s" % (item["destination"], item["error"]), file=sys.stderr)
if problems:
    print("sync-skills: %d problem(s) left for the operator; see the Skills page" % problems, file=sys.stderr)
    sys.exit(1)
print("sync-skills: %d skills registered with Agent Service Desk" % len(result["skills"]))
'
