#!/usr/bin/env python3
"""Exercise exact readers and bounded inventories through standalone isolate."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--isolate', required=True)
parser.add_argument('--check', action='store_true')
args = parser.parse_args()
root = Path(tempfile.mkdtemp(prefix='harness-code-readers-')).resolve()
(root / 'good.txt').write_bytes('α\r\nβ\n'.encode())
(root / 'bad.txt').write_bytes(b'good\n\xff\n')
(root / 'item.rs').write_text('pub fn fixture() {}\n')
env = dict(os.environ)
for key in ['HOME', 'BRO_HOME', 'CODEX_HOME', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME']:
    env[key] = str(root / key.lower())
rows = []

def invoke(name, value):
    run = subprocess.run([args.isolate, '--root', str(root), name, '--args', json.dumps(value)],
                         env=env, capture_output=True, text=True, timeout=30)
    try:
        value = json.loads(run.stdout)
    except ValueError:
        value = None
    return run, value

def record(name, passed):
    row = {'case': name, 'contract_passed': bool(passed)}
    rows.append(row)
    print(json.dumps(row), flush=True)

run, value = invoke('code.readLines', {'file': 'good.txt', 'startLine': 1, 'endLine': 2})
record('exact-crlf-unicode', run.returncode == 0 and value['text'] == 'α\r\nβ\n' and value['truncated'] is False)
run, value = invoke('code.readLines', {'file': 'bad.txt', 'startLine': 1, 'endLine': 2})
record('invalid-utf8-refused', run.returncode != 0 and 'invalid_utf8' in run.stderr)
span = {'file': 'good.txt', 'byte_start': 1, 'byte_end': 2,
        'content_sha256': hashlib.sha256((root / 'good.txt').read_bytes()).hexdigest()}
run, value = invoke('code.read', {'span': span})
record('split-codepoint-refused', run.returncode != 0 and 'invalid_utf8_span' in run.stderr)
files = ['item.rs'] * 129
first, page = invoke('code.items', {'files': files})
second, last = invoke('code.items', {'files': files, 'offset': page.get('next_offset', 0)})
record('inventory-pagination', first.returncode == second.returncode == 0 and page.get('next_offset') == 128 and last.get('next_offset') is None)
(root / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
print('Artifacts:', root)
if args.check and not all(row['contract_passed'] for row in rows):
    raise SystemExit(1)
