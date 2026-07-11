#!/usr/bin/env python3
"""Detect and repair drift in eval-manifest expected_entity_refs (gap-b44fe7ac).

`expected_entity_refs` in eval/queries/*.json embed content hashes
(symbol defn_hash, project_file chunk_hash) and chunk occurrence indices,
so any corpus content change silently invalidates them — and stale refs
zero out every ranking metric built on the suite (MRR/recall@k sweeps,
cross-encoder A/B). This script re-derives the refs from each manifest's
target_locators against the live daemon and reports (or applies) updates.

Resolution model (mirrors the indexer, bbox-corpus-index project_files.rs):
- `symbol:<proj>:<name>:<defn_hash>` — defn_hash IS the chunk_hash of the
  defining chunk, so `bbox_refactor_project_refs(file, query=symbol)` gives
  the current ref deterministically.
- `project_file:<proj>:<relhash>:<chunk_hash>:<idx>` — taken from the
  chunk's entity_ref; among a file's chunks the best match is picked by
  token overlap with the locator description + manifest query.
- id-keyed types (knowledge:, thread:, commit:, transcript spans) do not
  content-drift; they are liveness-checked via bbox_inspect_entity. A dead
  transcript ref gets one extra attempt: search bbox_hybrid_search
  (doc_type=transcript) for the locator's transcript_hint and adopt a hit
  only when it lands in the SAME session as the stale ref (drift repair,
  not a re-target). A hit in a different session is reported as a
  candidate for manual attention, not auto-adopted. Other id-keyed types
  are reported for manual attention when missing.

Stale path_hints (file moved, e.g. the crate decomposition) are chased
through `git ls-files` basename matching when exactly one candidate exists;
the proposed manifest update then rewrites the locator's path_hint too.

Usage:
  eval/scripts/refresh_expected_refs.sh                 # report drift
  eval/scripts/refresh_expected_refs.sh --apply         # rewrite manifests
  eval/scripts/refresh_expected_refs.sh --url http://127.0.0.1:7264/mcp

Manifests are compiled into the eval oracle via include_str! — after
--apply, run `cargo nextest run --workspace -E 'test(~manifest)'` (or the
check.rs tests) to confirm they still parse.
"""

import argparse
import glob
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bbox_mcp import McpClient, default_url

CONTENT_ADDRESSED_TYPES = ("symbol", "project_file")


def base_repo_root():
    """Registered BASE checkout root (worktree-aware).

    Entity refs embed the base project's id and the daemon indexes the base
    checkout's content, so ref DERIVATION (project_dir for project_refs)
    must target the base. Manifest WRITES go to the current checkout
    (`checkout_root`) — never mutate the shared base from a worktree.
    """
    common = subprocess.run(
        ["git", "rev-parse", "--git-common-dir"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    return os.path.dirname(os.path.abspath(common))


def checkout_root():
    return subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()


def tokens(text):
    return set(re.findall(r"[a-zA-Z_][a-zA-Z0-9_]+", (text or "").lower()))


def candidate_paths(repo, path_hint, symbol_hint=None):
    """Candidate relative paths for a locator, chasing moved files.

    Exact hit wins outright. Otherwise: same-basename files (the crate
    decomposition moved modules without renaming them), and as a last
    resort files that mention the symbol_hint. Ambiguity is fine — the
    caller scores every candidate's chunks and picks the best match.
    """
    if path_hint and os.path.exists(os.path.join(repo, path_hint)):
        return [path_hint]
    candidates = []
    if path_hint:
        base = os.path.basename(path_hint)
        out = subprocess.run(
            ["git", "-C", repo, "ls-files", f"*{base}"],
            capture_output=True, text=True,
        ).stdout.splitlines()
        candidates = [m for m in out if os.path.basename(m) == base]
    if not candidates and symbol_hint:
        out = subprocess.run(
            ["git", "-C", repo, "grep", "-l", "--", symbol_hint],
            capture_output=True, text=True,
        ).stdout.splitlines()
        candidates = [m for m in out if re.search(r"\.(rs|md|toml|py|sh)$", m)][:5]
    return candidates


def project_chunks(client, repo, rel_path, query=None):
    args = {
        "file": rel_path,
        "project_dir": repo,
        "limit": 1000,
        "include_content": True,
    }
    if query:
        args["query"] = query
    result = client.call_tool("bbox_refactor_project_refs", args)
    if result.get("status") != "ok":
        raise RuntimeError(f"project_refs {rel_path}: {result}")
    return result


def entity_exists(client, ref):
    result = client.call_tool(
        "bbox_inspect_entity", {"entity_ref": ref, "per_type_limit": 0}
    )
    return "error" not in result


def best_chunk(chunks, locator, manifest_query):
    """Rank chunks by token overlap with the locator description + query."""
    want = tokens(locator.get("description", "")) | tokens(manifest_query)
    scored = []
    for chunk in chunks:
        have = tokens(chunk.get("excerpt", "")) | tokens(chunk.get("symbol", ""))
        scored.append((len(want & have), chunk))
    scored.sort(key=lambda pair: -pair[0])
    return [chunk for _, chunk in scored]


def transcript_session(ref):
    """Return (provider, session_id) parsed out of a transcript ref, or None.

    Accepts both the canonical `transcript:<provider>:<session_id>:<byte>:<idx>`
    shape and the legacy unprefixed `<provider>:<session_id>:<byte>:<idx>` form
    that older/undeployed daemons can still emit from hybrid_search hits
    (gap-edc84378 fold note: hybrid_entity_id now canonicalizes with the
    `transcript:` prefix, but a live daemon predating that fix may not).
    """
    parts = ref.split(":")
    if len(parts) == 5 and parts[0] == "transcript":
        return parts[1], parts[2]
    if len(parts) == 4:
        return parts[0], parts[1]
    return None


def reanchor_transcript(client, locator, old_ref, manifest_query, notes):
    """Attempt to re-anchor a dead transcript ref within the SAME session.

    A dead transcript ref (session purged/reparsed, byte offsets shifted) is
    not necessarily gone corpus-wide -- the conversation may still be
    indexed under a new line_offset. Search bbox_hybrid_search for the
    locator's transcript_hint (falling back to the manifest query), scoped
    to doc_type=transcript with vector_weight=0.0 (dodges the doc_type/RRF
    starvation bug a live daemon may still carry: small limits can return
    zero hits when the vector lane crowds out the requested doc_type before
    fusion). Adopt a hit only when it lands back in the SAME session as the
    stale ref -- that's drift repair, not a meaning change. A hit landing in
    a DIFFERENT session would re-target the manifest to a different
    conversation, which changes what the query is provenance for and needs
    operator judgment: list the top 3 candidates as a note and return None
    instead of auto-adopting.
    """
    old_session = transcript_session(old_ref)
    if old_session is None:
        return None
    hint = locator.get("transcript_hint") or manifest_query
    if not hint:
        return None
    try:
        result = client.call_tool(
            "bbox_hybrid_search",
            {"query": hint, "doc_type": "transcript", "vector_weight": 0.0, "limit": 20},
        )
    except Exception as err:
        notes.append(f"transcript re-anchor search failed: {err}")
        return None
    hits = result.get("results", [])
    for hit in hits:
        entity_id = hit.get("entity_id", "")
        if transcript_session(entity_id) == old_session:
            canonical = entity_id if entity_id.startswith("transcript:") else f"transcript:{entity_id}"
            notes.append(f"transcript re-anchored within session: {old_ref} -> {canonical}")
            return canonical
    if hits:
        candidates = [
            f"{hit.get('entity_id')} ({hit.get('label', '')})" for hit in hits[:3]
        ]
        notes.append(
            "session purged; candidate re-targets (different session, needs operator judgment): "
            + "; ".join(candidates)
        )
    return None


def resolve_locator(client, repo, locator, old_ref, manifest_query, notes):
    """Return the current expected ref for one locator, or None."""
    ref_type = old_ref.split(":", 1)[0]
    if ref_type not in CONTENT_ADDRESSED_TYPES:
        if entity_exists(client, old_ref):
            return old_ref
        # System memories migrated from knowledge:sm-* to system_memory:sm-*.
        if ref_type == "knowledge" and old_ref.startswith("knowledge:sm-"):
            renamed = "system_memory:" + old_ref.split(":", 1)[1]
            if entity_exists(client, renamed):
                notes.append(f"type migrated: {old_ref} -> {renamed}")
                return renamed
        if ref_type == "transcript":
            reanchored = reanchor_transcript(client, locator, old_ref, manifest_query, notes)
            if reanchored is not None:
                return reanchored
        notes.append(f"id-keyed ref missing from corpus (manual fix): {old_ref}")
        return None

    symbol_hint = locator.get("symbol_hint")
    paths = candidate_paths(repo, locator.get("path_hint"), symbol_hint)
    if not paths:
        notes.append(f"path_hint unresolvable: {locator.get('path_hint')}")
        return None

    # Score every candidate file's chunks; exact symbol match dominates,
    # token overlap with the locator description + query breaks ties.
    want = tokens(locator.get("description", "")) | tokens(manifest_query)
    best = None  # (exact_symbol, overlap, chunk, project_id, rel_path)
    for rel_path in paths:
        try:
            result = project_chunks(client, repo, rel_path, query=symbol_hint)
        except RuntimeError as err:
            notes.append(f"project_refs failed for {rel_path}: {err}")
            continue
        chunks = result.get("chunks", [])
        if symbol_hint and not chunks:
            chunks = project_chunks(client, repo, rel_path).get("chunks", [])
        for chunk in chunks:
            exact = 1 if symbol_hint and chunk.get("symbol") == symbol_hint else 0
            have = tokens(chunk.get("excerpt", "")) | tokens(chunk.get("symbol", ""))
            key = (exact, len(want & have))
            if best is None or key > best[0]:
                best = (key, chunk, result["project_id"], rel_path)
    if best is None:
        notes.append(f"no chunks found in candidates: {paths}")
        return None
    _, chunk, project_id, rel_path = best

    if rel_path != locator.get("path_hint"):
        notes.append(f"path moved: {locator.get('path_hint')} -> {rel_path}")
        locator["path_hint"] = rel_path

    if ref_type == "symbol":
        name = chunk.get("symbol") or old_ref.split(":")[2]
        return f"symbol:{project_id}:{name}:{chunk['chunk_hash']}"
    return chunk["entity_ref"]


def pair_locators(manifest):
    """Pair expected refs with locators positionally; pad with {} on mismatch."""
    refs = manifest.get("expected_entity_refs", [])
    locators = manifest.get("target_locators", [])
    if len(locators) < len(refs):
        locators = locators + [{}] * (len(refs) - len(locators))
    return list(zip(refs, locators))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", default=None, help=f"MCP endpoint (default {default_url()})")
    parser.add_argument("--queries-dir", default=None)
    parser.add_argument("--apply", action="store_true", help="rewrite manifests in place")
    parser.add_argument("--manifest", action="append", help="restrict to manifest id(s)")
    args = parser.parse_args()

    repo = base_repo_root()
    queries_dir = args.queries_dir or os.path.join(checkout_root(), "eval", "queries")
    client = McpClient(url=args.url)
    print(f"daemon: {client.url}\nresolve against: {repo}\nqueries: {queries_dir}\n", file=sys.stderr)

    total = drifted = unresolved = 0
    proposals = []
    for path in sorted(glob.glob(os.path.join(queries_dir, "*.json"))):
        with open(path) as fh:
            manifest = json.load(fh)
        if args.manifest and manifest.get("id") not in args.manifest:
            continue
        total += 1
        notes = []
        new_refs = []
        before = json.dumps(manifest, sort_keys=True)
        for old_ref, locator in pair_locators(manifest):
            current = resolve_locator(
                client, repo, locator, old_ref, manifest.get("query", ""), notes
            )
            if current is None:
                unresolved += 1
                new_refs.append(old_ref)  # never drop a ref silently
                continue
            if current != old_ref:
                notes.append(f"drift: {old_ref}\n      -> {current}")
            new_refs.append(current)
        manifest["expected_entity_refs"] = new_refs
        changed = json.dumps(manifest, sort_keys=True) != before

        status = "DRIFTED" if changed else ("UNRESOLVED" if notes else "ok")
        if changed:
            drifted += 1
        if changed or notes:
            print(f"[{status}] {manifest.get('id')} ({os.path.basename(path)})")
            for note in notes:
                print(f"    {note}")
        if changed:
            proposals.append((path, manifest))

    print(
        f"\n{total} manifests: {drifted} drifted, "
        f"{unresolved} unresolvable ref(s), {total - drifted} clean",
        file=sys.stderr,
    )
    if not args.apply:
        if proposals:
            print("re-run with --apply to write the updates", file=sys.stderr)
        return 1 if proposals else 0
    for path, manifest in proposals:
        with open(path, "w") as fh:
            json.dump(manifest, fh, indent=2)
            fh.write("\n")
        print(f"wrote {path}", file=sys.stderr)
    if proposals:
        print(
            "manifests are include_str! into eval/check.rs — run the check tests",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
