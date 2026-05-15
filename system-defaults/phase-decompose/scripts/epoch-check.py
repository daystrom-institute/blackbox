#!/usr/bin/env python3
"""Compute phase-decompose epoch ceiling status."""

import argparse
import json
import sys


def parse_int(value, default, name):
    try:
        if value is None or value == "" or value == "null":
            return default
        if "${" in str(value):
            raise ValueError(f"unresolved template in {name}: {value}")
        return int(value)
    except TypeError:
        return default
    except ValueError as exc:
        raise SystemExit(f"{name} must be an integer: {value}") from exc


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--epoch")
    parser.add_argument("--max-epochs")
    args = parser.parse_args()

    epoch = parse_int(args.epoch, 0, "epoch")
    max_epochs = parse_int(args.max_epochs, 3, "max_epochs")
    status = "halt" if epoch >= max_epochs else "continue"
    print(json.dumps({"status": status, "epoch": epoch, "max_epochs": max_epochs}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(1) from exc
