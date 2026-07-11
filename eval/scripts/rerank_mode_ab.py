#!/usr/bin/env python3
"""Model-vs-heuristic rerank A/B over the eval suite (Layer 3 gate).

Runs every eval-suite query through bbox_hybrid_search once per rerank
mode (none, heuristic, model) against the live daemon and scores the
ranked entity_ids against each manifest's expected_entity_refs with MRR
and recall@k, mirroring bbox_corpus_core::search::metrics. The design
(multimodal-embedding-routing.md Layer 3) flips the heuristic default to
model rerank only on a measured win here.

Run eval/scripts/refresh_expected_refs.sh first: stale expected refs zero
every metric. Model-mode calls hit the hosted cross-encoder; a query whose
response carries degraded.rerank_unavailable is counted and invalidates
the model arm if frequent.

Usage:
  eval/scripts/rerank_mode_ab.py --url 'http://127.0.0.1:7264/mcp?surface=interactive'
"""

import argparse
import glob
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bbox_mcp import McpClient, default_url

MODES = ["none", "heuristic", "model"]
KS = [1, 5, 10, 30]


def reciprocal_rank(ranked, expected):
    for idx, item in enumerate(ranked):
        if item in expected:
            return 1.0 / (idx + 1)
    return 0.0


def recall_at_k(ranked, expected, k):
    if not expected:
        return 0.0
    top = ranked[:k]
    hits = sum(1 for item in expected if item in top)
    return hits / len(expected)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", default=None, help=f"MCP endpoint (default {default_url()})")
    parser.add_argument("--queries-dir", default=None)
    parser.add_argument("--limit", type=int, default=30, help="search depth per query")
    args = parser.parse_args()

    script_dir = os.path.dirname(os.path.abspath(__file__))
    queries_dir = args.queries_dir or os.path.join(script_dir, "..", "queries")
    manifests = []
    for path in sorted(glob.glob(os.path.join(queries_dir, "*.json"))):
        with open(path) as fh:
            manifests.append(json.load(fh))
    print(f"{len(manifests)} eval manifests loaded", file=sys.stderr)

    client = McpClient(url=args.url)
    print(f"daemon: {client.url}", file=sys.stderr)

    report = {}
    per_query = {}
    degraded_counts = {mode: 0 for mode in MODES}
    for mode in MODES:
        rows = []
        for manifest in manifests:
            expected = manifest.get("expected_entity_refs", [])
            if not expected:
                continue
            try:
                result = client.call_tool(
                    "bbox_hybrid_search",
                    {"query": manifest["query"], "limit": args.limit, "rerank": mode},
                )
            except Exception as err:
                print(f"  ! {manifest['id']} mode={mode}: {err}", file=sys.stderr)
                continue
            degraded = result.get("degraded") or {}
            if degraded.get("rerank_unavailable"):
                degraded_counts[mode] += 1
                print(
                    f"  ! {manifest['id']} mode={mode} degraded: "
                    f"{degraded['rerank_unavailable']}",
                    file=sys.stderr,
                )
            ranked = [r["entity_id"] for r in result.get("results", [])]
            per_query[(manifest["id"], mode)] = reciprocal_rank(ranked, expected)
            rows.append((ranked, expected))
        n = len(rows) or 1
        report[mode] = {
            "queries": len(rows),
            "mrr": sum(reciprocal_rank(r, e) for r, e in rows) / n,
            **{
                f"recall@{k}": sum(recall_at_k(r, e, k) for r, e in rows) / n
                for k in KS
            },
        }
        print(f"mode={mode} done: {report[mode]}", file=sys.stderr)

    print("\nmode       queries  degraded  MRR      " + "  ".join(f"R@{k:<4}" for k in KS))
    for mode in MODES:
        r = report[mode]
        print(
            f"{mode:<10} {r['queries']:<8} {degraded_counts[mode]:<9} {r['mrr']:.4f}  "
            + "  ".join(f"{r[f'recall@{k}']:.4f}" for k in KS)
        )

    print("\nqueries whose RR changes between heuristic and model:")
    moved = False
    for manifest in manifests:
        h = per_query.get((manifest["id"], "heuristic"))
        m = per_query.get((manifest["id"], "model"))
        if h is None or m is None or abs(h - m) < 1e-9:
            continue
        moved = True
        arrow = "UP  " if m > h else "DOWN"
        print(f"  {arrow} {manifest['id']}: heuristic={h:.4f} model={m:.4f}")
    if not moved:
        print("  (none)")


if __name__ == "__main__":
    main()
