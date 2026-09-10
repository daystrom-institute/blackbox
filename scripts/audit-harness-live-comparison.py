#!/usr/bin/env python3
"""Run four synthetic tasks against a real Responses provider (requires credentials).

Artifact checks are independent of the model. verification_call_observed only
records a verification attempt, not successful execution. This small sample is
a smoke comparison, not an efficacy benchmark. Runtime artifacts stay outside
the checkout; never publish credential files or raw provider sessions.
"""

import argparse, json, os, subprocess, tempfile, time
from pathlib import Path
p = argparse.ArgumentParser()
p.add_argument('--harness', required=True)
p.add_argument('--cohort', required=True)
p.add_argument('--task', choices=['edit', 'retrieval'])
a = p.parse_args()
root = Path(tempfile.mkdtemp(prefix='harness-live-' + a.cohort + '-')).resolve()
rows = []
for mode in ['off', 'only']:
    for task in [a.task] if a.task else ['edit', 'retrieval']:
        case = root / (mode + '-' + task)
        case.mkdir()
        (case / 'src').mkdir()
        (case / 'home').mkdir()
        subprocess.run(['git', 'init', '-q', str(case)], check=True)
        (case / 'AGENTS.md').write_text('Use only this synthetic fixture directory. Preserve unrelated files. Complete and verify the requested change. Do not commit.\n')
        (case / 'src/AGENTS.md').write_text('Preserve the first comment line of calculator.py exactly.\n')
        (case / 'src/calculator.py').write_text('# fixture invariant\ndef average(values):\n    return sum(values) / len(values)\n')
        (case / 'records.txt').write_text('background\n' * 210000 + 'AUDIT_RECORD=violet-731\n')
        prompt = 'Fix src/calculator.py so average([]) returns 0 and ordinary averages still work. Preserve the first comment line. Add verify.py with regression assertions and execute it successfully. Do not commit.' if task == 'edit' else 'Find the AUDIT_RECORD value in records.txt, write answer.json with exactly one property named "value" whose string is the exact AUDIT_RECORD value, and verify the answer against the source. Do not commit.'
        env = {k: v for k, v in os.environ.items() if not k.startswith('BRO_HARNESS_')}
        env.update({'HOME': str(case / 'home'), 'CODEX_HOME': str(Path.home() / '.codex'), 'BRO_HOME': str(case / 'bro'), 'BRO_HARNESS_TRANSPORT': 'openai-responses', 'BRO_HARNESS_MAX_TURNS': '12', 'BRO_HARNESS_WEB_SEARCH': '0', 'BRO_HARNESS_NUDGES': '0', 'BRO_HARNESS_SPAWN_SCRUB': 'CODEX_HOME,OPENAI_API_KEY,ANTHROPIC_API_KEY,ANTHROPIC_AUTH_TOKEN'})
        cmd = [a.harness, '--cwd', str(case), '--model', 'gpt-5.5', '--effort', 'low', '--code-mode', mode, '--system-prompt', 'You are working on a small synthetic coding fixture. Read applicable AGENTS.md instructions and complete the requested work and verification.', '--mcp-config', '{"mcpServers":{}}', '-p', prompt]
        start = time.monotonic()
        with (case / 'events.jsonl').open('w') as out, (case / 'stderr.txt').open('w') as err:
            try:
                result = subprocess.run(cmd, env=env, stdout=out, stderr=err, timeout=240)
                exit_code = result.returncode
            except subprocess.TimeoutExpired:
                exit_code = 'timeout'
        elapsed = time.monotonic() - start
        events = []
        for line in (case / 'events.jsonl').read_text().splitlines():
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
        terminal = [e for e in events if e.get('type') == 'result']
        calls = []
        for e in events:
            for item in e.get('message', {}).get('content', []) if isinstance(e.get('message', {}).get('content', []), list) else []:
                if isinstance(item, dict) and item.get('type') == 'tool_use':
                    calls.append(item)
        if task == 'edit':
            verify = subprocess.run(['python3', '-c', 'from src.calculator import average; assert average([])==0; assert average([2,4])==3'], cwd=case, capture_output=True)
            passed = verify.returncode == 0 and (case / 'src/calculator.py').read_text().startswith('# fixture invariant\n') and (case / 'verify.py').exists() and ('assert' in (case / 'verify.py').read_text())
            verified = any(('verify.py' in json.dumps(c.get('input')) and 'python' in json.dumps(c.get('input')) for c in calls))
        else:
            try:
                passed = json.loads((case / 'answer.json').read_text()) == {'value': 'violet-731'}
            except (OSError, ValueError):
                passed = False
            verified = any(('answer.json' in json.dumps(c.get('input')) and any((marker in json.dumps(c.get('input')) for marker in ['assert', 'cat ', 'jq', 'test '])) for c in calls))
        row = {'cohort': a.cohort, 'code_mode': mode, 'task': task, 'exit': exit_code, 'seconds': round(elapsed, 2), 'artifact_passed': passed, 'verification_call_observed': verified, 'model_steps': terminal[-1].get('num_turns') if terminal else None, 'terminal_type': terminal[-1].get('subtype') if terminal else None, 'usage': terminal[-1].get('usage') if terminal else None, 'tool_calls': [c['name'] for c in calls]}
        rows.append(row)
        print(json.dumps(row), flush=True)
        (root / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
print('Artifacts:', root)
