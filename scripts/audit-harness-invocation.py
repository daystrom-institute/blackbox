#!/usr/bin/env python3
"""Diagnostic recorder: successful execution is not a claim of passing contracts."""
from pathlib import Path
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
import argparse, json, os, subprocess, tempfile, threading

parser = argparse.ArgumentParser(description="Synthetic full-harness credential-scrub and wrapper-admission observations; no real credentials or model requests.")
parser.add_argument('--harness', default='bro-harness')
parser.add_argument('--output-dir', type=Path, help='New, nonexistent fixture/output directory')
options = parser.parse_args()
if options.output_dir:
    options.output_dir.mkdir(parents=True, exist_ok=False)
    root = options.output_dir.resolve()
else:
    root = Path(tempfile.mkdtemp(prefix='harness-invocation-audit-')).resolve()
requests = []
current = None
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(body)
        if len(requests) == 1:
            name, args = current
            delta = {'tool_calls':[{'index':0,'id':'audit-call','type':'function','function':{'name':name,'arguments':json.dumps(args)}}]}
            stop = 'tool_calls'
        else:
            delta = {'content':'fixture done'}
            stop = 'stop'
        chunks = [{'choices':[{'index':0,'delta':delta,'finish_reason':None}]}, {'choices':[{'index':0,'delta':{},'finish_reason':stop}]}]
        data = (''.join('data: '+json.dumps(c)+'\n\n' for c in chunks)+'data: [DONE]\n\n').encode()
        self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.send_header('Content-Length',str(len(data))); self.end_headers(); self.wfile.write(data)

server = ThreadingHTTPServer(('127.0.0.1',0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
command = 'if [ -n "${AUDIT_CANARY:-}" ]; then printf CANARY_PRESENT; else printf CANARY_ABSENT; fi'
cases = [
    ('flat-scrub', 'shell_run', {'command':command,'yield_time_ms':0}, []),
    ('nested-scrub', 'exec', {'source':'text(await tools.shell_run('+json.dumps({'command':command,'yield_time_ms':0})+'));'}, []),
    ('denied-shell-build-gate', 'exec', {'source':'text(await build.gate({command:"printf AUDIT_GATE_RAN; exit 23"}));'}, ['--deny-tools','shell_*']),
]
results = []
try:
    for label, name, args, extra in cases:
        requests.clear(); current = (name,args)
        env = os.environ.copy()
        env.update({'BRO_HOME':str(root/label),'CODEX_HOME':str(root/'empty-codex'),'BRO_HARNESS_TRANSPORT':'openai-chat','OPENAI_BASE_URL':f'http://127.0.0.1:{server.server_port}/v1','OPENAI_API_KEY':'synthetic-fixture','BRO_HARNESS_WEB_SEARCH':'0','BRO_HARNESS_MAX_TURNS':'3','BRO_HARNESS_NUDGES':'0','BRO_HARNESS_SPAWN_SCRUB':'AUDIT_CANARY','AUDIT_CANARY':'synthetic'})
        invocation = [options.harness,'--daemon-worker','--cwd',str(root),'--model','gpt-5.5','--code-mode','optional','--system-prompt','','--mcp-config','{"mcpServers":{}}','-p','Complete the fixture task.',*extra]
        process = subprocess.run(invocation, env=env, capture_output=True, text=True, timeout=25)
        outputs = [m.get('content') for request in requests[1:] for m in request.get('messages',[]) if m.get('role')=='tool']
        names = [t['function']['name'] for t in requests[0].get('tools',[])] if requests else []
        row = {'case':label,'exit':process.returncode,'requests':len(requests),'tool_outputs':outputs,'shell_run_visible':'shell_run' in names}
        results.append(row)
        (root/(label+'-wire.json')).write_text(json.dumps(requests,indent=2))
        (root/(label+'-stdout.jsonl')).write_text(process.stdout)
        (root/(label+'-stderr.txt')).write_text(process.stderr)
        print(json.dumps(row), flush=True)
finally:
    server.shutdown(); server.server_close()
(root/'results.json').write_text(json.dumps(results,indent=2))
print('Artifacts:',root)
