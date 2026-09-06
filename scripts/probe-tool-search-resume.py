#!/usr/bin/env python3
"""Observe deferred-tool schemas across real harness process resumes, with a local scripted endpoint."""
import argparse
import http.server
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=pathlib.Path, required=True)
args = parser.parse_args()
root = pathlib.Path(tempfile.mkdtemp(prefix='bbox-tool-resume-')).resolve()
requests = []


class Endpoint(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(body)
        (root / f'request-{len(requests)}.json').write_text(json.dumps(body, indent=2))
        first = len(requests) == 1
        block = ({'type': 'tool_use', 'id': 'activate-read', 'name': 'tool_search',
                  'input': {'query': 'select:file_read'}} if first else
                 {'type': 'text', 'text': 'PROBE_OK'})
        events = [
            {'type': 'message_start', 'message': {'id': 'synthetic', 'type': 'message',
             'role': 'assistant', 'content': [], 'model': body['model'],
             'usage': {'input_tokens': 10, 'output_tokens': 1}}},
            {'type': 'content_block_start', 'index': 0, 'content_block': block},
            {'type': 'content_block_stop', 'index': 0},
            {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if first else 'end_turn'},
             'usage': {'output_tokens': 1}},
            {'type': 'message_stop'},
        ]
        if not first:
            events[1]['content_block']['text'] = ''
            events.insert(2, {'type': 'content_block_delta', 'index': 0,
                             'delta': {'type': 'text_delta', 'text': 'PROBE_OK'}})
        data = ''.join('event: ' + e['type'] + '\ndata: ' + json.dumps(e) + '\n\n' for e in events).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Endpoint)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
env = {k: v for k, v in os.environ.items() if k in ['PATH', 'TMPDIR', 'SYSTEMROOT']}
env.update({'HOME': str(root), 'BRO_HOME': str(root / 'bro'),
            'XDG_CONFIG_HOME': str(root / 'config'), 'XDG_STATE_HOME': str(root / 'state'),
            'XDG_DATA_HOME': str(root / 'data'), 'XDG_CACHE_HOME': str(root / 'cache'),
            'BRO_HARNESS_PROVIDER': 'glm', 'BRO_HARNESS_TRANSPORT': 'anthropic',
            'ANTHROPIC_AUTH_TOKEN': 'synthetic',
            'ANTHROPIC_BASE_URL': f'http://127.0.0.1:{server.server_port}'})
sid = str(uuid.uuid4())
base = [str(args.binary.resolve()), '--cwd', str(root), '--system-prompt', '',
        '--model', 'glm-5.3', '--code-mode', 'only', '--prompt', 'Synthetic protocol probe.']


def run(label, extra, env_extra=None):
    with (root / f'{label}.stdout').open('w') as out, (root / f'{label}.stderr').open('w') as err:
        subprocess.run(base + extra, env=env | (env_extra or {}), stdout=out, stderr=err, timeout=45, check=True)


try:
    run('initial', ['--session-id', sid])
    run('resume', ['--resume', sid])
    snapshot_path = root / 'bro' / 'harness-sessions' / (sid + '.json')
    snapshot = json.loads(snapshot_path.read_text())
    snapshot['side'].pop('tool_activations', None)
    # Model a legacy session whose compaction removed the activation result.
    snapshot['snapshot'] = [{'role': 'user', 'content': [{'type': 'text', 'text': 'Earlier work summarized.'}]}]
    snapshot_path.write_text(json.dumps(snapshot))
    run('legacy-resume', ['--resume', sid])
    run('denied-resume', ['--resume', sid, '--deny-tools', 'file_read'])
    snapshot = json.loads(snapshot_path.read_text())
    snapshot['side']['tool_activations'] = []
    snapshot_path.write_text(json.dumps(snapshot))
    run('explicit-empty-resume', ['--resume', sid])
    observation = {'evidence': str(root), 'request_count': len(requests),
                   'file_read_present': [any(t['name'] == 'file_read' for t in r.get('tools', []))
                                         for r in requests],
                   'native_search_present': [any(t.get('type') == 'web_search_20250305'
                                                 for t in r.get('tools', [])) for r in requests]}
    print(json.dumps(observation, indent=2), flush=True)
    (root / 'summary.json').write_text(json.dumps(observation, indent=2))
    assert observation['file_read_present'] == [False, True, True, True, False, False], observation
    assert all(observation['native_search_present']), observation
finally:
    server.shutdown()
    server.server_close()
    worker.join()
