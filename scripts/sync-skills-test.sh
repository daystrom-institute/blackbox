#!/usr/bin/env bash
# Focused tests for scripts/sync-skills.sh. Every run uses a temporary HOME and
# never contacts the host's Agent Service Desk.
#
# A stub server checks the request the script sends and how it reports ASD's
# answers. With ASD_CHECKOUT set to a whalesong checkout (its intent-board
# dependencies installed), the script also runs against a real ASD daemon
# started on a free loopback port with a temporary data directory, covering
# adoption of marked copies, conflicts, publisher scope and pruning.
#
# Usage: scripts/sync-skills-test.sh
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
sync="$script_dir/sync-skills.sh"
work=$(mktemp -d)
work=$(CDPATH='' cd -- "$work" && pwd -P)
pids=()
cleanup() {
    for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; done
    rm -rf "$work"
}
trap cleanup EXIT

passed=0
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { passed=$((passed + 1)); echo "ok - $*"; }
free_port() { python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'; }
wait_port() {
    for _ in $(seq 1 200); do
        curl -s -o /dev/null "http://127.0.0.1:$1/" && return 0
        sleep 0.05
    done
    return 1
}
skill() {
    mkdir -p "$1/$2"
    printf -- '---\nname: %s\ndescription: Test skill %s\n---\n' "$2" "$2" >"$1/$2/SKILL.md"
}

# Fixture source and home: two skills, a directory without SKILL.md, the shared
# root and two Claude homes, one of them a symlink to the other.
src="$work/source"
skill "$src" bbox-alpha
skill "$src" bbox-beta
mkdir -p "$src/not-a-skill"
home="$work/home"
mkdir -p "$home/.claude" "$home/.claude-work"
ln -s "$home/.claude" "$home/.claude-alias"

# Stub ASD: records each request and answers from a canned response file.
stub_dir="$work/stub"
mkdir -p "$stub_dir"
cat >"$stub_dir/server.py" <<'PY'
import http.server, json, os, sys
directory = sys.argv[1]
class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        with open(os.path.join(directory, "requests.jsonl"), "a") as log:
            log.write(json.dumps({"path": self.path, "body": json.loads(body)}) + "\n")
        tool = self.path.split("?")[0].rsplit("/", 1)[1]
        with open(os.path.join(directory, tool + ".json")) as reply:
            status, payload = json.load(reply)
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        self.send_response(404); self.end_headers()
    def log_message(self, *args):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[2])), Handler).serve_forever()
PY
stub_port=$(free_port)
python3 -I "$stub_dir/server.py" "$stub_dir" "$stub_port" &
pids+=($!)
wait_port "$stub_port" || fail "stub server did not start"
stub="http://127.0.0.1:$stub_port"
reply() { printf '%s' "$2" >"$stub_dir/$1.json"; }
installed='{"state":"installed","destination":"/d","detail":"Linked to /s"}'

reply skill_publish "[200, {\"result\": {\"publisher\":\"blackbox\",\"skills\":[{\"id\":\"bbox-alpha\",\"installs\":[$installed]},{\"id\":\"bbox-beta\",\"installs\":[{\"state\":\"installed\",\"destination\":\"/d2\",\"detail\":\"Adopted the publisher copy marked .bbox-skill, kept at /a\"}]}],\"unregistered\":[],\"targets_created\":[1,2],\"removed\":[],\"cleanup\":[]}}]"
out=$(HOME="$home" ASD_URL="$stub" BBOX_SKILLS_SOURCE="$src" "$sync")
python3 -I - "$stub_dir/requests.jsonl" "$src" "$home" <<'PY' || fail "publish request shape"
import json, sys
log, src, home = sys.argv[1:]
request = [json.loads(line) for line in open(log)][-1]
assert request["path"] == "/api/tools/skill_publish?as=blackbox-installer", request["path"]
body = request["body"]
assert body["publisher"] == "blackbox"
assert body["skills"] == [{"id": n, "source": f"{src}/{n}", "adopt_marker": ".bbox-skill"} for n in ("bbox-alpha", "bbox-beta")], body["skills"]
assert body["targets"][0] == {"path": f"{home}/.agents/skills", "provider": "shared"}, body["targets"]
assert sorted(t["path"] for t in body["targets"][1:]) == sorted(f"{home}/{h}/skills" for h in (".claude", ".claude-alias", ".claude-work")), body["targets"]
assert "prune" not in body
PY
[[ "$out" == *"adopted bbox-beta at /d2"* && "$out" == *"2 skills registered"* ]] || fail "publish summary: $out"
[ ! -e "$home/.agents" ] || fail "the script itself must not write skill roots"
ok "declares the publisher set, adoption marker, shared root and every Claude home"

out=$(HOME="$home" ASD_URL="$stub" BBOX_SKILLS_SOURCE="$src" BBOX_SKILLS_DIR="$work/custom" "$sync" --claude-home "$home/.claude-work")
python3 -I - "$stub_dir/requests.jsonl" "$work" "$home" <<'PY' || fail "explicit roots"
import json, sys
log, work, home = sys.argv[1:]
body = [json.loads(line) for line in open(log)][-1]["body"]
assert body["targets"] == [{"path": f"{work}/custom", "provider": "shared"}, {"path": f"{home}/.claude-work/skills", "provider": "claude"}], body["targets"]
PY
ok "BBOX_SKILLS_DIR and --claude-home select explicit roots"

reply skill_publish "[200, {\"result\": {\"publisher\":\"blackbox\",\"skills\":[{\"id\":\"bbox-alpha\",\"installs\":[{\"state\":\"conflict\",\"destination\":\"/d\",\"detail\":\"exists and was not installed by ASD\"}]}],\"unregistered\":[\"bbox-gone\"],\"targets_created\":[],\"removed\":[\"/x/bbox-gone\"],\"cleanup\":[]}}]"
set +e
out=$(HOME="$home" ASD_URL="$stub" BBOX_SKILLS_SOURCE="$src" "$sync" 2>&1); status=$?
set -e
[ "$status" -eq 1 ] || fail "conflict exit status $status"
[[ "$out" == *"conflict: bbox-alpha at /d"* && "$out" == *"unregistered bbox-gone"* && "$out" == *"unlinked /x/bbox-gone"* ]] || fail "conflict report: $out"
ok "reports conflicts, unregistered skills and removed links; exits 1 on problems"

reply skill_publish '[400, {"error": "Skill bbox-alpha is registered by someone"}]'
set +e
out=$(HOME="$home" ASD_URL="$stub" BBOX_SKILLS_SOURCE="$src" "$sync" 2>&1); status=$?
set -e
[ "$status" -eq 3 ] && [[ "$out" == *"ASD refused skill_publish (HTTP 400)"*"registered by someone"* ]] || fail "refusal ($status): $out"
ok "an ASD refusal exits 3 with its reason"

closed=$(free_port)
set +e
out=$(HOME="$home" ASD_URL="http://127.0.0.1:$closed" BBOX_SKILLS_SOURCE="$src" "$sync" 2>&1); status=$?
set -e
[ "$status" -eq 3 ] && [[ "$out" == *"not reachable at http://127.0.0.1:$closed"*"nothing was registered"* ]] || fail "unreachable ($status): $out"
[ ! -e "$home/.agents" ] || fail "unreachable ASD must not fall back to copying"
ok "an unreachable ASD exits 3 and installs nothing"

reply skill_list "[200, {\"result\": {\"skills\":[{\"id\":\"bbox-alpha\",\"source\":{\"path\":\"$src/bbox-alpha\"},\"adopt_marker\":\".bbox-skill\",\"installs\":[{\"state\":\"pending\",\"destination\":\"/d\",\"detail\":\"Not linked\"},{\"state\":\"disabled\",\"destination\":\"/e\",\"detail\":\"Target disabled\"}]},{\"id\":\"bbox-old\",\"source\":{\"path\":\"/old\"},\"adopt_marker\":\".bbox-skill\",\"installs\":[]}],\"targets\":[{\"path\":\"$home/.agents/skills\",\"canonical\":\"$home/.agents/skills\"},{\"path\":\"$home/.claude/skills\",\"canonical\":\"$home/.claude/skills\"}],\"pending_removals\":[{\"destination\":\"/r\",\"skill_id\":\"bbox-old\"}],\"cleanup\":[{\"destination\":\"/c\",\"skill_id\":\"bbox-old\",\"error\":\"EACCES\"}],\"reconcile_error\":\"disk full\"}}]"
: >"$stub_dir/requests.jsonl"
set +e
out=$(HOME="$home" ASD_URL="$stub" BBOX_SKILLS_SOURCE="$src" "$sync" --check --claude-home "$home/.claude-alias" --claude-home "$home/.claude-work" 2>&1); status=$?
set -e
[ "$status" -eq 1 ] || fail "check drift status $status: $out"
for line in "would register bbox-beta" "would unregister bbox-old" "would add target $home/.claude-work/skills" "bbox-alpha: pending /d: Not linked" "would unlink /r (bbox-old)" "cleanup pending: /c: EACCES" "ASD reconciliation failed: disk full"; do
    [[ "$out" == *"$line"* ]] || fail "check report lacks '$line': $out"
done
[[ "$out" == *".claude-alias"* || "$out" == *"disabled /e"* ]] && fail "an alias of a registered root or a disabled destination is not drift: $out"
grep -q '"fresh": true' "$stub_dir/requests.jsonl" || fail "--check asks for a fresh read"
grep -q skill_publish "$stub_dir/requests.jsonl" && fail "--check must not publish"
ok "--check reads a fresh registration, compares resolved roots, reports drift, cleanup and errors, and writes nothing"

# A maintained linked worktree is a valid source; the script registers where it runs.
repo="$work/repo"
mkdir -p "$repo/scripts"
cp "$sync" "$repo/scripts/"
skill "$repo/system-defaults/skills" bbox-alpha
git -C "$repo" init -q
git -C "$repo" add .
git -C "$repo" -c user.email=t@example.invalid -c user.name=t commit -qm fixture
git -C "$repo" worktree add -q "$work/linked" 2>/dev/null
reply skill_publish "[200, {\"result\": {\"publisher\":\"blackbox\",\"skills\":[],\"unregistered\":[],\"targets_created\":[],\"removed\":[],\"cleanup\":[]}}]"
HOME="$home" ASD_URL="$stub" "$work/linked/scripts/sync-skills.sh" >/dev/null || fail "a linked worktree source is accepted"
python3 -I - "$stub_dir/requests.jsonl" "$work" <<'PY' || fail "linked worktree source path"
import json, sys
body = [json.loads(line) for line in open(sys.argv[1])][-1]["body"]
assert body["skills"] == [{"id": "bbox-alpha", "source": f"{sys.argv[2]}/linked/system-defaults/skills/bbox-alpha", "adopt_marker": ".bbox-skill"}], body["skills"]
PY
ok "registers the checkout it runs from, including a maintained linked worktree"

if [ -z "${ASD_CHECKOUT:-}" ]; then
    echo "skip - real ASD daemon (set ASD_CHECKOUT to a whalesong checkout)"
    echo "sync-skills-test: $passed passed"
    exit 0
fi

# Real daemon: temporary data directory and HOME, loopback only, no background loops.
asd_home="$work/asd"
real_home="$work/real-home"
shared="$real_home/.agents/skills"
mkdir -p "$asd_home" "$real_home/.claude/skills" "$real_home/.claude-two"
# Earlier copy-based install: marked copies plus Claude links to them.
for name in bbox-alpha bbox-beta; do
    mkdir -p "$shared/$name"
    printf 'old copy\n' >"$shared/$name/SKILL.md"
    printf 'digest\n' >"$shared/$name/.bbox-skill"
    ln -s "$shared/$name" "$real_home/.claude/skills/$name"
done
# Unrelated content that must survive: an unmarked directory using a Blackbox
# name, another publisher's skill and an unrelated link.
skill "$src" bbox-gamma
mkdir -p "$shared/bbox-gamma"
printf 'operator notes\n' >"$shared/bbox-gamma/notes.txt"
skill "$work/other" other-skill
ln -s /nonexistent "$real_home/.claude/skills/unrelated"

asd_port=$(free_port)
(cd "$ASD_CHECKOUT/intent-board" && HOME="$real_home" INTENT_BOARD_HOME="$asd_home" INTENT_BOARD_HOST=127.0.0.1 \
    INTENT_BOARD_PORT="$asd_port" ASD_BACKGROUND=off exec node src/server/main.ts >"$work/asd.log" 2>&1) &
pids+=($!)
wait_port "$asd_port" || fail "ASD did not start: $(cat "$work/asd.log")"
asd="http://127.0.0.1:$asd_port"
curl -sf -X POST -H 'content-type: application/json' "$asd/api/tools/skill_register" \
    -d "{\"id\":\"other-skill\",\"source\":\"$work/other/other-skill\",\"publisher\":\"operator\"}" >/dev/null || fail "register other publisher"

set +e
out=$(HOME="$real_home" ASD_URL="$asd" BBOX_SKILLS_SOURCE="$src" "$sync" 2>&1); status=$?
set -e
[ "$status" -eq 1 ] || fail "real conflict status $status: $out"
for name in bbox-alpha bbox-beta; do
    [ "$(readlink "$shared/$name")" = "$src/$name" ] || fail "$name shared link"
    [ "$(readlink "$real_home/.claude/skills/$name")" = "$src/$name" ] || fail "$name claude link"
    [ "$(readlink "$real_home/.claude-two/skills/$name")" = "$src/$name" ] || fail "$name second claude home"
done
[ "$(find "$asd_home/skill-adoptions" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')" = 2 ] || fail "adopted copies kept"
[ "$(cat "$shared/bbox-gamma/notes.txt")" = "operator notes" ] || fail "unmarked content kept"
[[ "$out" == *"conflict: bbox-gamma at $shared/bbox-gamma"* && "$out" == *"adopted bbox-alpha"* ]] || fail "real report: $out"
[ "$(readlink "$real_home/.claude/skills/unrelated")" = /nonexistent ] || fail "unrelated link kept"
[ "$(readlink "$shared/other-skill")" = "$work/other/other-skill" ] || fail "other publisher installed through Blackbox targets"
ok "real ASD adopts marked copies and their links, links every home, keeps unmarked content as a conflict"

rm -rf "$shared/bbox-gamma"
HOME="$real_home" ASD_URL="$asd" BBOX_SKILLS_SOURCE="$src" "$sync" >/dev/null || fail "rerun after resolving the conflict"
[ "$(readlink "$shared/bbox-gamma")" = "$src/bbox-gamma" ] || fail "gamma installed after conflict cleared"
HOME="$real_home" ASD_URL="$asd" BBOX_SKILLS_SOURCE="$src" "$sync" --check >/dev/null || fail "--check is current after sync"
rm "$real_home/.claude/skills/bbox-alpha"
set +e
out=$(HOME="$real_home" ASD_URL="$asd" BBOX_SKILLS_SOURCE="$src" "$sync" --check 2>&1); status=$?
set -e
[ "$status" -eq 1 ] && [[ "$out" == *"bbox-alpha: pending $real_home/.claude/skills/bbox-alpha"* ]] || fail "--check sees a deleted link ($status): $out"
[ ! -e "$real_home/.claude/skills/bbox-alpha" ] || fail "--check must not repair"
HOME="$real_home" ASD_URL="$asd" BBOX_SKILLS_SOURCE="$src" "$sync" >/dev/null || fail "sync repairs the deleted link"
[ "$(readlink "$real_home/.claude/skills/bbox-alpha")" = "$src/bbox-alpha" ] || fail "link restored"
rm -rf "$src/bbox-beta"
set +e
HOME="$real_home" ASD_URL="$asd" BBOX_SKILLS_SOURCE="$src" "$sync" --check >/dev/null; status=$?
set -e
[ "$status" -eq 1 ] || fail "--check reports a removed skill"
HOME="$real_home" ASD_URL="$asd" BBOX_SKILLS_SOURCE="$src" "$sync" >/dev/null || fail "sync after removal"
[ ! -e "$shared/bbox-beta" ] && [ ! -L "$real_home/.claude/skills/bbox-beta" ] || fail "removed skill unlinked"
[ "$(readlink "$shared/other-skill")" = "$work/other/other-skill" ] || fail "other publisher's skill survives pruning"
curl -sf -X POST -H 'content-type: application/json' "$asd/api/tools/skill_get" -d '{"id":"other-skill"}' | grep -q '"publisher":"operator"' || fail "other publisher registration kept"
ok "real ASD prunes only Blackbox's removed skill; --check sees links deleted on disk"

fresh_home="$work/fresh-home"
mkdir -p "$fresh_home/.claude"
HOME="$fresh_home" ASD_URL="$asd" BBOX_SKILLS_SOURCE="$src" "$sync" >/dev/null || fail "first install on a fresh home"
[ "$(readlink "$fresh_home/.agents/skills/bbox-alpha")" = "$src/bbox-alpha" ] || fail "shared root created on a fresh home"
[ "$(readlink "$fresh_home/.claude/skills/bbox-alpha")" = "$src/bbox-alpha" ] || fail "fresh Claude home linked"
ok "first install on a home without ~/.agents creates the shared root"

echo "sync-skills-test: $passed passed"
