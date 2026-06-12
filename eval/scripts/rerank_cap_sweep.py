#!/usr/bin/env python3
"""Rerank-cap sweep over the eval suite (gap-39b3ce16 protocol).

For each candidate cap, run every eval-suite query through
bbox_hybrid_search with the `rerank_cap` operator probe against the live
daemon and score the ranked entity_ids against each manifest's
expected_entity_refs with MRR and recall@k — mirroring
bbox_corpus_core::search::metrics (see that module's header for the
protocol). Run eval/scripts/refresh_expected_refs.sh first: stale expected
refs zero every metric and make the sweep meaningless.

Usage:
  eval/scripts/rerank_cap_sweep.py                       # dev daemon, default caps
  eval/scripts/rerank_cap_sweep.py --url http://127.0.0.1:7264/mcp \
      --caps 1.5,1.75,2.0 --limit 30
"""

import argparse
import glob
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bbox_mcp import McpClient, default_url

DEFAULT_CAPS = [1.0, 1.25, 1.5, 1.75, 2.0, 2.5]
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
    return sum(1 for e in expected if e in top) / len(expected)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", default=None, help=f"MCP endpoint (default {default_url()})")
    parser.add_argument("--queries-dir", default=None)
    parser.add_argument(
        "--caps", default=",".join(str(c) for c in DEFAULT_CAPS),
        help="comma-separated rerank caps to sweep",
    )
    parser.add_argument("--limit", type=int, default=30, help="search depth per query")
    args = parser.parse_args()
    caps = [float(c) for c in args.caps.split(",") if c.strip()]

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
    per_query = {}  # (id, cap) -> rr, for drill-down
    for cap in caps:
        rows = []
        for manifest in manifests:
            expected = manifest.get("expected_entity_refs", [])
            if not expected:
                continue
            try:
                result = client.call_tool(
                    "bbox_hybrid_search",
                    {"query": manifest["query"], "limit": args.limit, "rerank_cap": cap},
                )
            except Exception as err:
                print(f"  ! {manifest['id']} cap={cap}: {err}", file=sys.stderr)
                continue
            ranked = [r["entity_id"] for r in result.get("results", [])]
            per_query[(manifest["id"], cap)] = reciprocal_rank(ranked, expected)
            rows.append((ranked, expected))
        n = len(rows) or 1
        report[cap] = {
            "queries": len(rows),
            "mrr": sum(reciprocal_rank(r, e) for r, e in rows) / n,
            **{
                f"recall@{k}": sum(recall_at_k(r, e, k) for r, e in rows) / n
                for k in KS
            },
        }
        print(f"cap={cap} done: {report[cap]}", file=sys.stderr)

    print("\ncap    queries  MRR      " + "  ".join(f"R@{k:<4}" for k in KS))
    for cap in caps:
        r = report[cap]
        print(
            f"{cap:<6} {r['queries']:<8} {r['mrr']:.4f}  "
            + "  ".join(f"{r[f'recall@{k}']:.4f}" for k in KS)
        )

    print("\nqueries whose RR changes across caps:")
    moved = False
    for manifest in manifests:
        rrs = [per_query.get((manifest["id"], cap)) for cap in caps]
        if None in rrs:
            continue
        if max(rrs) - min(rrs) > 1e-9:
            moved = True
            print(
                f"  {manifest['id']}: "
                + " ".join(f"{c}:{r:.3f}" for c, r in zip(caps, rrs))
            )
    if not moved:
        print("  (none — ranking of expected refs is cap-insensitive on this corpus)")


if __name__ == "__main__":
    main()
