#!/usr/bin/env python3
"""Synthetic shell output probes through isolate and a local provider fixture."""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--harness', required=True)
parser.add_argument('--isolate', required=True)
parser.add_argument('--check', action='store_true')
args = parser.parse_args()
root = Path(tempfile.mkdtemp(prefix='harness-shell-output-')).resolve()
results = []


def python_command(source):
    return 'python3 -c ' + shlex.quote(source)


def isolate_case(label, source):
    cwd = root / label
    cwd.mkdir()
    process = subprocess.run([args.isolate, '--root', str(cwd), '--cell', source],
                             capture_output=True, text=True, timeout=30)
    (cwd / 'output.txt').write_text(process.stdout)
    (cwd / 'stderr.txt').write_text(process.stderr)
    observations = [json.loads(line) for line in process.stdout.splitlines()
                    if line.startswith('{')]
    observation = observations[-1] if observations else {}
    row = {'case': label, 'exit': process.returncode, **observation}
    row['contract_passed'] = process.returncode == 0 and observation.get('ok') is True
    results.append(row)
    print(json.dumps(row), flush=True)


page_command = python_command('import sys; sys.stdout.write("a"*24000); sys.stderr.write("b"*21000)')
for label, initial_budget in [('exited-pages', 32), ('zero-page', 0)]:
    isolate_case(label, '''
let r = await tools.shell_run({command:COMMAND,yield_time_ms:0,max_output_tokens:BUDGET});
const firstEmpty = r.stdout === "" && r.stderr === "";
const firstPending = r.output_pending === true && typeof r.session_id === "string";
let so=r.stdout, se=r.stderr, pages=1;
while ((r.running || r.output_pending) && pages < 1000) {
  r=await tools.shell_poll({session_id:r.session_id,yield_time_ms:1,max_output_tokens:128});
  so+=r.stdout; se+=r.stderr; pages++;
}
text({ok:so==="a".repeat(24000) && se==="b".repeat(21000) && firstPending &&
  (BUDGET!==0 || firstEmpty),pages,stdout_bytes:so.length,stderr_bytes:se.length,firstEmpty,firstPending});
'''.replace('COMMAND', json.dumps(page_command)).replace('BUDGET', str(initial_budget)))

for label, command, expected, extra in [
    ('split-utf8', "printf '\\342'; printf ready >&2; read -r input; printf '\\202\\254'", '€', {}),
    ('split-filter-line', "printf ER; printf ready >&2; read -r input; printf 'ROR final\\n'", 'ERROR final\n', {'output_filter': {'stdout': '^ERROR final\\n$'}}),
]:
    initial = {'command': command, 'yield_time_ms': 200, **extra}
    isolate_case(label, '''
let r=await tools.shell_run(INITIAL);
let so=r.stdout, se=r.stderr, polls=0;
while (!se.includes("ready") && r.running && polls++ < 10) {
 r=await tools.shell_poll({session_id:r.session_id,yield_time_ms:100}); so+=r.stdout; se+=r.stderr;
}
r=await tools.shell_poll({session_id:r.session_id,stdin:"\\n",close_stdin:true,yield_time_ms:0});
so+=r.stdout;
while (r.output_pending && polls++ < 20) {
 r=await tools.shell_poll({session_id:r.session_id,yield_time_ms:1}); so+=r.stdout;
}
text({ok:so===EXPECTED,observed:so,ready:se.includes("ready")});
'''.replace('INITIAL', json.dumps(initial)).replace('EXPECTED', json.dumps(expected)))

overflow = python_command(r'import sys; sys.stderr.write("x"*(8*1024*1024+500)+"\nFINAL ERROR\n")')
isolate_case('overflow-tail', '''
let r=await tools.shell_run({command:COMMAND,yield_time_ms:0,output_filter:{stderr:"^FINAL ERROR\\n$"}});
let found=r.stderr.includes("FINAL ERROR"), pages=1, loss=JSON.stringify(r.output || {}).includes("dropped_bytes");
while (r.output_pending && pages++ < 3000) {
 r=await tools.shell_poll({session_id:r.session_id,yield_time_ms:1});
 found ||= r.stderr.includes("FINAL ERROR"); loss ||= JSON.stringify(r.output || {}).includes("dropped_bytes");
}
text({ok:found&&loss&&r.exit_code===0,found,loss,pages,command_exit:r.exit_code});
'''.replace('COMMAND', json.dumps(overflow)))

# A flat model-facing page must survive the outer cap, including JSON escaping.
for cap_kb in (1, 16):
    label = f'flat-escaped-{cap_kb}k'
    cwd = root / label
    cwd.mkdir()
    payload = '\x01"\\\né' * 600
    command = python_command('import sys; sys.stdout.write(' + repr(payload) + '); sys.stderr.write(' + repr(payload) + ')')
    pages = []
    failures = []
    request_count = [0]

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            request_count[0] += 1
            outputs = [m['content'] for m in request.get('messages', []) if m.get('role') == 'tool']
            if outputs:
                try:
                    pages.append(json.loads(outputs[-1]))
                except ValueError:
                    failures.append('outer result is not valid JSON')
            if not outputs:
                name = 'shell_run'
                arguments = {'command': command, 'yield_time_ms': 0, 'max_output_tokens': 3000}
            elif pages and (pages[-1].get('running') or pages[-1].get('output_pending')) and request_count[0] < 200:
                name = 'shell_poll'
                arguments = {'session_id': pages[-1].get('session_id'), 'yield_time_ms': 1, 'max_output_tokens': 3000}
            else:
                name = None
            if name:
                delta = {'tool_calls': [{'index': 0, 'id': 'fixture-' + str(request_count[0]), 'type': 'function',
                                        'function': {'name': name, 'arguments': json.dumps(arguments)}}]}
                stop = 'tool_calls'
            else:
                delta, stop = {'content': 'fixture complete'}, 'stop'
            chunks = [{'choices': [{'index': 0, 'delta': delta, 'finish_reason': None}]},
                      {'choices': [{'index': 0, 'delta': {}, 'finish_reason': stop}]}]
            body = (''.join('data: ' + json.dumps(c) + '\n\n' for c in chunks) + 'data: [DONE]\n\n').encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    env = os.environ.copy()
    env.update({'BRO_HOME': str(cwd / 'home'), 'CODEX_HOME': str(cwd / 'empty-codex'),
                'BRO_HARNESS_TRANSPORT': 'openai-chat', 'OPENAI_BASE_URL': f'http://127.0.0.1:{server.server_port}/v1',
                'OPENAI_API_KEY': 'synthetic-fixture', 'BRO_HARNESS_TOOL_RESULT_CAP_KB': str(cap_kb),
                'BRO_HARNESS_MAX_TURNS': '200', 'BRO_HARNESS_WEB_SEARCH': '0', 'BRO_HARNESS_NUDGES': '0'})
    try:
        process = subprocess.run([args.harness, '--daemon-worker', '--cwd', str(cwd), '--model', 'gpt-5.5',
                                  '--code-mode', 'optional', '--system-prompt', '', '--mcp-config', '{"mcpServers":{}}',
                                  '-p', 'Run the synthetic fixture.'], env=env, capture_output=True, text=True, timeout=45)
    finally:
        server.shutdown()
        server.server_close()
    so = ''.join(p.get('stdout', '') for p in pages)
    se = ''.join(p.get('stderr', '') for p in pages)
    row = {'case': label, 'exit': process.returncode, 'pages': len(pages), 'failures': failures,
           'stdout_exact': so == payload, 'stderr_exact': se == payload,
           'contract_passed': process.returncode == 0 and not failures and so == payload and se == payload}
    results.append(row)
    (cwd / 'events.jsonl').write_text(process.stdout)
    (cwd / 'stderr.txt').write_text(process.stderr)
    print(json.dumps(row), flush=True)

(root / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
print('Artifacts:', root)
if args.check and not all(row['contract_passed'] for row in results):
    raise SystemExit(1)
