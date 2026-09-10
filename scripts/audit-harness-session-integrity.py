#!/usr/bin/env python3
"""Probe scoped instruction admission and explicit resume with isolated Chat fixtures."""
import argparse
import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--harness', required=True)
parser.add_argument('--check', action='store_true')
args = parser.parse_args()
root = Path(tempfile.mkdtemp(prefix='harness-session-integrity-')).resolve()
scenario = ''
fixture = root
requests = []
observations = []

def tool(name, value):
    return {'index': 0, 'id': f'fixture-call-{len(requests)}', 'type': 'function',
            'function': {'name': name, 'arguments': json.dumps(value)}}

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *unused):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(request)
        observations.append({'mutation_exists': (fixture / 'child/result.txt').exists(),
                             'instruction_present': 'EXACT_SCOPED_INSTRUCTION' in json.dumps(request)})
        delta, finish = {'content': 'fixture complete'}, 'stop'
        if scenario.startswith('scoped') and len(requests) <= 2:
            write = {'file_path': 'child/result.txt', 'content': 'accepted'}
            if scenario == 'scoped-flat':
                invocation = tool('file_write', write)
            elif scenario == 'scoped-read-write':
                source = 'text(await tools.file_read({file_path:"child/source.txt"})); text(await tools.file_write(' + json.dumps(write) + '));'
                invocation = tool('exec', {'source': source})
            else:
                source = 'for (let n=0;n<2;n++) { try { text(await tools.file_write(' + json.dumps(write) + ')); } catch(e) { text(String(e)); } }'
                invocation = tool('exec', {'source': source})
            delta, finish = {'tool_calls': [invocation]}, 'tool_calls'
        body = ''.join('data: ' + json.dumps(value) + '\n\n' for value in [
            {'choices': [{'index': 0, 'delta': delta, 'finish_reason': None}]},
            {'choices': [{'index': 0, 'delta': {}, 'finish_reason': finish}]}]) + 'data: [DONE]\n\n'
        body = body.encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()

def configure(name):
    global scenario, fixture
    scenario = name
    fixture = root / name
    fixture.mkdir()
    (fixture / 'child').mkdir()
    (fixture / 'child/AGENTS.md').write_text('EXACT_SCOPED_INSTRUCTION\n')
    (fixture / 'child/source.txt').write_text('ordinary source bytes\n')
    requests.clear()
    observations.clear()
    env = {key: value for key, value in os.environ.items() if not key.startswith('BRO_HARNESS_')}
    env.update({'HOME': str(fixture/'home'), 'XDG_CONFIG_HOME':str(fixture/'config'),
                'XDG_STATE_HOME': str(fixture/'state'), 'BRO_HOME': str(fixture/'bro'),
                'CODEX_HOME': str(fixture/'codex'), 'BRO_HARNESS_TRANSPORT':'openai-chat',
                'OPENAI_BASE_URL':f'http://127.0.0.1:{server.server_port}/v1',
                'OPENAI_API_KEY':'synthetic-fixture', 'BRO_HARNESS_WEB_SEARCH':'0',
                'BRO_HARNESS_NUDGES':'0', 'BRO_HARNESS_MAX_TURNS':'4'})
    cmd = [args.harness, '--cwd', str(fixture), '--model', 'fixture-model', '--code-mode',
           'off' if name == 'scoped-flat' else 'optional', '--system-prompt', '',
           '--mcp-config', '{"mcpServers":{}}']
    return cmd, env

def invoke(cmd, env, *extra):
    result = subprocess.run(cmd + list(extra), env=env, capture_output=True, text=True, timeout=30)
    events = []
    for line in result.stdout.splitlines():
        try:
            events.append(json.loads(line))
        except ValueError:
            pass
    return result, events

rows = []
def record(name, passed, result=None, events=None):
    row = {'case':name, 'contract_passed':bool(passed), 'requests':len(requests)}
    if result is not None:
        row['exit'] = result.returncode
        (fixture/(name+'-stderr.txt')).write_text(result.stderr)
        (fixture/(name+'-events.json')).write_text(json.dumps(events, indent=2)+'\n')
    (fixture/(name+'-requests.json')).write_text(json.dumps(requests, indent=2)+'\n')
    (fixture/(name+'-observations.json')).write_text(json.dumps(observations, indent=2)+'\n')
    rows.append(row)
    print(json.dumps(row), flush=True)

try:
    for name in ['scoped-flat', 'scoped-read-write', 'scoped-caught-retry']:
        cmd, env = configure(name)
        result, events = invoke(cmd, env, '-p', 'Write the fixture file.')
        passed = len(observations) == 3 and not observations[1]['mutation_exists'] and observations[1]['instruction_present'] and observations[2]['mutation_exists']
        record(name, passed, result, events)
    for name in ['resume-missing', 'resume-corrupt', 'resume-unsequenced-tail', 'resume-valid', 'resume-schema']:
        cmd, env = configure(name)
        ident = 'session-fixture'
        if name != 'resume-missing':
            first_cmd = cmd + (['--output-schema', '{"type":"object"}'] if name == 'resume-schema' else [])
            initial, initial_events = invoke(first_cmd, env, '--session-id', ident, '-p', 'Initial turn.')
            snapshots = list((fixture/'bro').rglob(ident+'.json'))
            assert len(snapshots) == 1, (name, initial.stderr, snapshots)
            snapshot = snapshots[0]
            if name == 'resume-corrupt':
                snapshot.write_text('{broken')
            elif name == 'resume-unsequenced-tail':
                logs = list((fixture/'bro').rglob(ident+'.events.jsonl'))
                assert len(logs) == 1, logs
                with logs[0].open('a') as log:
                    log.write(json.dumps({'event':{'type':'user','message':{'role':'user','content':[{'type':'text','text':'unsaved authoritative steer'}]}}})+'\n')
        requests.clear()
        observations.clear()
        result, events = invoke(cmd, env, '--resume', ident, '-p', 'Resumed turn.')
        if name == 'resume-valid':
            passed = bool(requests) and any(event.get('subtype') == 'success' for event in events) and 'Session runtime reset' in json.dumps(requests)
        elif name == 'resume-schema':
            passed = bool(requests) and any(tool.get('function',{}).get('name') == 'final_result' for tool in requests[0].get('tools',[])) and not any(event.get('subtype') == 'success' for event in events)
        else:
            passed = not requests and not any(event.get('subtype') == 'success' for event in events) and result.returncode != 0
        record(name, passed, result, events)
    cmd, env = configure('session-writer-lock')
    first = subprocess.Popen(cmd + ['--session-id','locked-session','--input-format','stream-json'],env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    try:
        selector = selectors.DefaultSelector()
        selector.register(first.stdout, selectors.EVENT_READ)
        assert selector.select(10), 'first session did not initialize'
        line = first.stdout.readline()
        assert line, first.stderr.read()
        selector.close()
        result, events = invoke(cmd, env, '--session-id','locked-session','-p','Conflicting writer.')
        record('session-writer-lock', not requests and result.returncode != 0 and 'writer' in result.stderr.lower(), result, events)
    finally:
        first.communicate('', timeout=10)
finally:
    server.shutdown()
    server.server_close()
(root/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
print('Artifacts:',root)
if args.check and not all(row['contract_passed'] for row in rows):
    raise SystemExit(1)
