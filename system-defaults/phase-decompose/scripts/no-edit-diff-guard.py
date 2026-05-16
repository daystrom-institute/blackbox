#!/usr/bin/env python3
"""Fail when a no-edit workflow step changes the worktree status."""

import os
import subprocess
import sys


def normalize_status(status: str) -> list[str]:
    """Ignore transient Python bytecode cache noise in no-edit runs."""
    normalized = []
    for line in status.splitlines():
        path = line[3:] if len(line) > 3 else line
        if "__pycache__/" in path or path.endswith("__pycache__/") or path.endswith(".pyc"):
            continue
        normalized.append(line)
    return normalized


def main() -> int:
    baseline = os.environ.get("NO_EDIT_BASELINE", "")
    current = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if current.returncode != 0:
        sys.stderr.write(current.stderr)
        return current.returncode or 1
    if normalize_status(current.stdout) != normalize_status(baseline):
        sys.stderr.write("no-edit guard failed: worktree status changed during sub-unit\n")
        sys.stderr.write("--- baseline ---\n")
        sys.stderr.write(baseline)
        if baseline and not baseline.endswith("\n"):
            sys.stderr.write("\n")
        sys.stderr.write("--- current ---\n")
        sys.stderr.write(current.stdout)
        if current.stdout and not current.stdout.endswith("\n"):
            sys.stderr.write("\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
