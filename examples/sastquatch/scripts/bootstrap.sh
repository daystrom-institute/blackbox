#!/usr/bin/env bash
# Bootstrap the SASTquatch demo against an existing Forgejo instance.
# Default: shares the keystone-forgejo container at :3000 + reuses
# keystone's admin token (sourced from `examples/keystone/.env` if
# present). Override via env to point at a fresh instance:
#
#   FORGEJO_HOST=http://127.0.0.1:3200 \
#   FORGEJO_CONTAINER=mine-forgejo \
#   ADMIN_USER=mine-admin \
#     ./scripts/bootstrap.sh
#
# What it does:
#   - Verifies Forgejo is reachable
#   - Resolves an admin API token (reuses keystone's, or issues a
#     fresh one against the configured admin)
#   - Creates the demo repo `<ADMIN_USER>/quat` (idempotent)
#   - Seeds Rust source with deliberate clippy + unsafe-deps issues +
#     a Cargo.toml with one known-vulnerable crate so cargo-audit fires
#   - Configures the Forgejo webhook → /webhook/sastquatch on the
#     daemon (distinct from keystone's /webhook/forgejo so the two
#     demos can share Forgejo without collision)
#
# Idempotent: re-running on an already-bootstrapped instance is a no-op.
#
# Requires: docker, jq, curl.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT}/.env"
FORGEJO_HOST="${FORGEJO_HOST:-http://127.0.0.1:3000}"
FORGEJO_CONTAINER="${FORGEJO_CONTAINER:-keystone-forgejo}"
ADMIN_USER="${ADMIN_USER:-keystone-admin}"
ADMIN_PASS="${ADMIN_PASS:-keystone-demo-pass-1234}"
REPO_NAME="${REPO_NAME:-quat}"
WEBHOOK_SECRET="${WEBHOOK_SECRET:-keystone-webhook-secret-not-for-prod}"
WEBHOOK_NAME="${WEBHOOK_NAME:-sastquatch}"
WEBHOOK_PORT="${WEBHOOK_PORT:-${BBOX_PORT:-7264}}"
WEBHOOK_TARGET="${WEBHOOK_TARGET:-http://host.docker.internal:${WEBHOOK_PORT}/webhook/${WEBHOOK_NAME}}"
KEYSTONE_ENV_FILE="${KEYSTONE_ENV_FILE:-${ROOT}/../keystone/.env}"

# host.docker.internal works on Docker Desktop. On Linux compose
# leaves the bridge gateway addressable on 172.17.0.1 (default).
HOST_GATEWAY="$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null || echo 172.17.0.1)"
WEBHOOK_TARGET="${WEBHOOK_TARGET//host.docker.internal/$HOST_GATEWAY}"

log() { printf '\033[36m[bootstrap]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[bootstrap]\033[0m %s\n' "$*" >&2; }

wait_for_forgejo() {
    log "waiting for Forgejo to come up at ${FORGEJO_HOST}…"
    for _ in {1..60}; do
        if curl -fsS "${FORGEJO_HOST}/api/v1/version" >/dev/null 2>&1; then
            log "Forgejo is up"
            return 0
        fi
        sleep 1
    done
    warn "Forgejo failed to start within 60s"
    exit 1
}

resolve_token() {
    # Priority:
    #   1. Existing $ENV_FILE — sastquatch's own bootstrap output
    #   2. Adjacent keystone/.env — share keystone's admin token when
    #      the two demos point at the same Forgejo (the default)
    #   3. Issue a fresh token against ADMIN_USER:ADMIN_PASS
    if [[ -f "${ENV_FILE}" ]] && grep -q '^FORGEJO_TOKEN=' "${ENV_FILE}"; then
        log "token present in ${ENV_FILE}; reusing"
        # shellcheck disable=SC1090
        source "${ENV_FILE}"
        return 0
    fi
    if [[ -f "${KEYSTONE_ENV_FILE}" ]] && grep -q '^FORGEJO_TOKEN=' "${KEYSTONE_ENV_FILE}"; then
        log "reusing keystone admin token from ${KEYSTONE_ENV_FILE}"
        # shellcheck disable=SC1090
        source "${KEYSTONE_ENV_FILE}"
        # Override the repo to ours, keep the rest of keystone's vars.
        FORGEJO_REPO="${REPO_NAME}"
        {
            echo "FORGEJO_BASE_URL=${FORGEJO_BASE_URL}"
            echo "FORGEJO_TOKEN=${FORGEJO_TOKEN}"
            echo "FORGEJO_OWNER=${FORGEJO_OWNER}"
            echo "FORGEJO_REPO=${REPO_NAME}"
            echo "FORGEJO_WEBHOOK_SECRET=${FORGEJO_WEBHOOK_SECRET}"
        } >"${ENV_FILE}"
        log "wrote ${ENV_FILE} (sharing keystone Forgejo)"
        # ADMIN_USER for the rest of bootstrap = whoever owns the
        # token (keystone-admin), not whatever was env-defaulted.
        ADMIN_USER="${FORGEJO_OWNER}"
        export FORGEJO_TOKEN
        return 0
    fi
    log "issuing API token via ${ADMIN_USER}:${ADMIN_PASS}"
    local resp
    resp=$(curl -fsS -u "${ADMIN_USER}:${ADMIN_PASS}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/users/${ADMIN_USER}/tokens" \
        -d '{"name":"sastquatch-bootstrap-'"$(date +%s)"'","scopes":["all"]}')
    FORGEJO_TOKEN=$(jq -r '.sha1' <<<"${resp}")
    if [[ -z "${FORGEJO_TOKEN}" || "${FORGEJO_TOKEN}" == "null" ]]; then
        warn "token issuance failed: ${resp}"
        exit 1
    fi
    {
        echo "FORGEJO_BASE_URL=${FORGEJO_HOST}"
        echo "FORGEJO_TOKEN=${FORGEJO_TOKEN}"
        echo "FORGEJO_OWNER=${ADMIN_USER}"
        echo "FORGEJO_REPO=${REPO_NAME}"
        echo "FORGEJO_WEBHOOK_SECRET=${WEBHOOK_SECRET}"
    } >"${ENV_FILE}"
    log "wrote ${ENV_FILE}"
    export FORGEJO_TOKEN
}

create_repo() {
    if curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}" >/dev/null 2>&1; then
        log "repo '${ADMIN_USER}/${REPO_NAME}' already exists"
        return 0
    fi
    log "creating repo '${ADMIN_USER}/${REPO_NAME}'"
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/user/repos" \
        -d "{\"name\":\"${REPO_NAME}\",\"description\":\"sastquatch SAST-squashing demo\",\"private\":false,\"auto_init\":true,\"default_branch\":\"main\"}" \
        >/dev/null
}

put_file() {
    local path="$1"; local message="$2"; local content_b64="$3"
    local probe
    probe=$(curl -sS -o /dev/null -w '%{http_code}' \
        -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/contents/${path}")
    if [[ "${probe}" == "200" ]]; then
        log "  ${path} already present; skipping"
        return 0
    fi
    log "  seeding ${path}"
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/contents/${path}" \
        -d "{\"branch\":\"main\",\"content\":\"${content_b64}\",\"message\":\"${message}\"}" \
        >/dev/null
}

seed_repo() {
    log "seeding Rust crate with deliberate SAST-squashable issues"

    # Cargo.toml — pin one crate at a known-vulnerable version (rustls-
    # webpki 0.103.x is published with a known DoS advisory) so cargo-
    # audit fires on the first run.
    local cargo
    cargo=$(base64 -w0 <<'TOML'
[package]
name = "quat"
version = "0.1.0"
edition = "2021"

[dependencies]
# Pinned old to make cargo-audit fire (RUSTSEC-2024-* class). The
# fixer arc is expected to bump this when it picks the cluster.
serde = "1.0.193"
serde_json = "1.0"
TOML
)
    put_file "Cargo.toml" "seed initial Cargo.toml" "${cargo}"

    # src/main.rs with deliberate clippy nits — these become the
    # adoption-dimension cluster the analyzer will pick up.
    local mainrs
    mainrs=$(base64 -w0 <<'RS'
//! quat — sample crate the SASTquatch arc squashes.
//!
//! The body has deliberate clippy hits across multiple categories so
//! the analyzer has real clusters to pick from. None of these are
//! soundness errors — the demo is about iteration, not catastrophe.

use std::collections::HashMap;

fn main() {
    println!("quat starting");
    let v = vec![1, 2, 3, 4, 5];
    let sorted = sort_descending(v);
    println!("sorted: {:?}", sorted);
    let counts = build_counts();
    for (k, v) in counts.iter() {
        println!("{}: {}", k, v);
    }
}

// clippy::unnecessary_sort_by — `sort_by(|a, b| b.cmp(a))` should be
// `sort_by_key(|x| std::cmp::Reverse(*x))` or `sort_unstable_by`.
fn sort_descending(mut v: Vec<i32>) -> Vec<i32> {
    v.sort_by(|a, b| b.cmp(a));
    v
}

// clippy::derivable_impls — could use `#[derive(Default)]`.
struct Counter {
    n: u32,
}

impl Default for Counter {
    fn default() -> Self {
        Counter { n: 0 }
    }
}

// clippy::needless_collect / loop-style — overly verbose construction.
fn build_counts() -> HashMap<String, u32> {
    let words = vec!["hello", "world", "hello", "sastquatch", "world", "hello"];
    let mut counts: HashMap<String, u32> = HashMap::new();
    for w in words.iter() {
        let entry = counts.entry(w.to_string()).or_insert(0);
        *entry += 1;
    }
    let _c = Counter::default();
    counts
}

// clippy::nonminimal_bool — `!(a && b)` should be `!a || !b`.
#[allow(dead_code)]
fn is_invalid(a: bool, b: bool) -> bool {
    !(a && b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_descending_works() {
        assert_eq!(sort_descending(vec![3, 1, 2]), vec![3, 2, 1]);
    }

    #[test]
    fn build_counts_works() {
        let c = build_counts();
        assert_eq!(c.get("hello"), Some(&3));
        assert_eq!(c.get("world"), Some(&2));
    }
}
RS
)
    put_file "src/main.rs" "seed src/main.rs with clippy issues" "${mainrs}"

    # sast-bridge.json — biofilter wiring identical in shape to the
    # one we built for transcript-search itself. The fixer arc reads
    # this, the verify-after-fix step reads this.
    local bridge
    bridge=$(base64 -w0 <<'JSON'
{
  "tools": {
    "clippy": {
      "run": "bash -c 'export PATH=\"$HOME/.cargo/bin:$PATH\"; set -o pipefail; cargo clippy --message-format=json --all-targets 2>/dev/null | clippy-sarif > daystrom/sast/clippy.sarif'",
      "sarif": ["daystrom/sast/clippy.sarif"],
      "dimensions": {
        "clippy::unnecessary_sort_by": "efficiency",
        "clippy::needless_collect": "efficiency",
        "clippy::redundant_clone": "efficiency",
        "clippy::derivable_impls": "adoption",
        "clippy::nonminimal_bool": "soundness"
      },
      "default_dimension": "adoption"
    },
    "cargo-audit": {
      "run": "bash -c 'export PATH=\"$HOME/.cargo/bin:$PATH\"; cargo audit --json | python3 scripts/cargo-audit-to-sarif.py > daystrom/sast/cargo-audit.sarif'",
      "sarif": ["daystrom/sast/cargo-audit.sarif"],
      "dimensions": {
        "vulnerability": "resilience",
        "unsound": "soundness",
        "unmaintained": "adoption",
        "denial-of-service": "resilience"
      },
      "default_dimension": "resilience"
    }
  }
}
JSON
)
    put_file "sast-bridge.json" "seed sast-bridge.json" "${bridge}"

    # The cargo-audit-to-sarif.py converter — same one transcript-
    # search uses, copied so the fixer arc's worktree has it.
    local converter
    converter=$(base64 -w0 <<'PY'
#!/usr/bin/env python3
"""Convert `cargo audit --json` output to SARIF 2.1.0."""

import json
import sys

LEVEL_BY_KIND = {
    "vulnerability": "error",
    "unsound": "error",
    "unmaintained": "warning",
    "notice": "note",
}


def make_rule(advisory, kind):
    return {
        "id": advisory["id"],
        "name": advisory["id"],
        "shortDescription": {"text": advisory.get("title", advisory["id"])},
        "fullDescription": {"text": advisory.get("description", "")[:2000]},
        "helpUri": advisory.get("url") or f"https://rustsec.org/advisories/{advisory['id']}",
        "properties": {
            "kind": kind,
            "categories": advisory.get("categories", []),
            "tags": advisory.get("categories", []) + [kind],
        },
    }


def make_result(advisory, package, kind):
    pkg_name = package.get("name", "unknown")
    pkg_ver = package.get("version", "?")
    return {
        "ruleId": advisory["id"],
        "level": LEVEL_BY_KIND.get(kind, "warning"),
        "message": {
            "text": f"{advisory.get('title', advisory['id'])} ({pkg_name} {pkg_ver}) [{kind}]"
        },
        "locations": [{
            "physicalLocation": {
                "artifactLocation": {"uri": "Cargo.lock"},
                "region": {"startLine": 1},
            }
        }],
        "properties": {"package": pkg_name, "version": pkg_ver, "kind": kind},
    }


def main():
    raw = sys.stdin.read()
    report = json.loads(raw) if raw.strip() else {}
    rules = {}
    results = []
    vulns = (report.get("vulnerabilities") or {}).get("list") or []
    for entry in vulns:
        adv = entry["advisory"]
        pkg = entry.get("package", {})
        rules.setdefault(adv["id"], make_rule(adv, "vulnerability"))
        results.append(make_result(adv, pkg, "vulnerability"))
    for kind, entries in (report.get("warnings") or {}).items():
        for entry in entries:
            adv = entry["advisory"]
            pkg = entry.get("package", {})
            rules.setdefault(adv["id"], make_rule(adv, kind))
            results.append(make_result(adv, pkg, kind))
    sarif = {
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "cargo-audit",
                    "informationUri": "https://github.com/rustsec/rustsec/tree/main/cargo-audit",
                    "rules": list(rules.values()),
                }
            },
            "results": results,
        }],
    }
    json.dump(sarif, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
PY
)
    put_file "scripts/cargo-audit-to-sarif.py" "seed cargo-audit→SARIF converter" "${converter}"

    # .gitignore — keep daystrom/ + target/ out of commits the fixer arc makes.
    local ignore
    ignore=$(base64 -w0 <<'IGN'
/target/
/daystrom/
IGN
)
    put_file ".gitignore" "seed .gitignore" "${ignore}"
}

configure_webhook() {
    local existing
    existing=$(curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/hooks" \
        | jq -r ".[] | select(.config.url == \"${WEBHOOK_TARGET}\") | .id")
    if [[ -n "${existing}" ]]; then
        log "webhook already configured (id=${existing}); skipping"
        return 0
    fi
    log "configuring webhook → ${WEBHOOK_TARGET}"
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/hooks" \
        -d "{
            \"type\":\"forgejo\",
            \"config\":{\"url\":\"${WEBHOOK_TARGET}\",\"content_type\":\"json\",\"secret\":\"${WEBHOOK_SECRET}\"},
            \"events\":[\"pull_request\",\"pull_request_review\",\"push\"],
            \"active\":true
        }" >/dev/null
}

main() {
    wait_for_forgejo
    resolve_token
    create_repo
    seed_repo
    configure_webhook
    log "bootstrap complete"
    log "  admin:   ${ADMIN_USER}"
    log "  api:     ${FORGEJO_HOST}/api/v1"
    log "  repo:    ${FORGEJO_HOST}/${ADMIN_USER}/${REPO_NAME}"
    log "  env:     ${ENV_FILE}"
    log "  webhook: ${WEBHOOK_TARGET}"
}

main "$@"
