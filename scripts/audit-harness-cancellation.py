#!/usr/bin/env python3
"""Deterministic cancellation regressions with local provider and file fixtures."""
import argparse
import errno
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--harness', required=True)
parser.add_argument('--isolate', required=True)
parser.add_argument('--check', action='store_true')
parser.add_argument('--output-dir', type=Path)
args = parser.parse_args()
root = args.output_dir or Path(tempfile.mkdtemp(prefix='harness-cancellation-'))
if args.output_dir:
    root.mkdir(parents=True, exist_ok=False)
root = root.resolve()
results = []


def wait_until(predicate, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.01)
    raise TimeoutError('fixture condition did not occur')


def interrupt_case(label, nested=False, yielded=False, shell=False):
    cwd = root / label
    cwd.mkdir()
    events = []
    requests = []
    release_model = threading.Event()
    if shell:
        call = {'type':'function_call', 'call_id':'fixture-call', 'name':'shell_run', 'arguments':json.dumps({
            'command':'printf ready > started; (sleep 0.6; printf escaped > escaped.txt) & sleep 3',
            'stdin':'x' * 1_000_000, 'yield_time_ms':0,
        })}
    else:
        os.mkfifo(cwd / 'blocked.fifo')
        patch = '*** Begin Patch\n*** Add File: committed.txt\n+committed\n*** Delete File: blocked.fifo\n*** End Patch'
        source = 'text(await tools.apply_patch(' + json.dumps({'source':patch}) + '));'
        if yielded:
            source = '// @exec: {"yield_time_ms":1}\n' + source
        call = {'type':'custom_tool_call', 'call_id':'fixture-call', 'name':'exec' if nested else 'apply_patch', 'input':source if nested else patch}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
            if len(requests) == 1:
                item = call
            else:
                release_model.wait(10)
                item = {'type':'message','role':'assistant','content':[{'type':'output_text','text':'fixture done'}]}
            chunks = [
                {'type':'response.output_item.done','item':item},
                {'type':'response.completed','response':{'usage':{'input_tokens':1,'output_tokens':1}}},
            ]
            data = ''.join('data: '+json.dumps(chunk)+'\n\n' for chunk in chunks).encode()
            try:
                self.send_response(200)
                self.send_header('Content-Type','text/event-stream')
                self.send_header('Content-Length',str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(('127.0.0.1',0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    env = os.environ.copy()
    env.update({'BRO_HOME':str(cwd/'home'),'CODEX_HOME':str(cwd/'empty-codex'),
                'BRO_HARNESS_TRANSPORT':'openai-responses','OPENAI_BASE_URL':f'http://127.0.0.1:{server.server_port}/v1',
                'OPENAI_API_KEY':'synthetic-fixture','BRO_HARNESS_WEB_SEARCH':'0','BRO_HARNESS_NUDGES':'0',
                'BRO_HARNESS_MAX_TURNS':'3','GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null'})
    stderr = (cwd/'stderr.txt').open('w')
    process = subprocess.Popen([args.harness,'--daemon-worker','--cwd',str(cwd),'--model','gpt-5.5',
        '--code-mode','optional','--input-format','stream-json','--system-prompt','',
        '--mcp-config','{"mcpServers":{}}'],env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr,text=True)
    def collect():
        for line in process.stdout:
            try:
                events.append({'time':time.monotonic(),'event':json.loads(line)})
            except ValueError:
                pass
    reader = threading.Thread(target=collect, daemon=True)
    reader.start()
    def send(value):
        process.stdin.write(json.dumps(value)+'\n')
        process.stdin.flush()
    writer = None
    try:
        send({'type':'user','message':{'role':'user','content':'Perform the synthetic fixture.'}})
        if shell:
            wait_until(lambda: (cwd/'started').exists())
        else:
            def open_writer():
                nonlocal writer
                try:
                    writer = os.open(cwd/'blocked.fifo', os.O_WRONLY | os.O_NONBLOCK)
                    return True
                except OSError as error:
                    if error.errno != errno.ENXIO:
                        raise
                    return False
            wait_until(open_writer)
            if yielded:
                wait_until(lambda: len(requests) >= 2)
        send({'type':'control_request','request_id':'fixture-interrupt','request':{'subtype':'interrupt'}})
        time.sleep(0.15)
        early_terminal = any(row['event'].get('type') == 'result' for row in events)
        early_ack = any(row['event'].get('type') == 'control_response' for row in events)
        if writer is not None:
            os.close(writer)
            writer = None
        wait_until(lambda: any(row['event'].get('type') == 'result' for row in events))
        wait_until(lambda: any(row['event'].get('type') == 'control_response' for row in events))
        if shell:
            time.sleep(0.8)
        else:
            wait_until(lambda: (cwd/'committed.txt').exists())
        terminal = next(row['event'] for row in events if row['event'].get('type') == 'result')
        observed = json.dumps([row['event'] for row in events if row['event'].get('type') == 'user'])
        row = {'case':label,'terminal_before_release':early_terminal,'ack_before_release':early_ack,
               'terminal_subtype':terminal.get('subtype'),'committed_file':(cwd/'committed.txt').exists(),
               'escaped_child_write':(cwd/'escaped.txt').exists(),'actual_outcome_visible':'committed.txt' in observed if not shell else 'cancelled' in observed}
        row['contract_passed'] = (not row['escaped_child_write'] and row['actual_outcome_visible']) if shell else (
            not early_terminal and not early_ack and row['committed_file'] and row['actual_outcome_visible'])
        results.append(row)
        row['contract_passed'] = row['contract_passed'] and terminal.get('subtype') == 'interrupted'
    finally:
        if writer is not None:
            os.close(writer)
        release_model.set()
        process.stdin.close()
        try:
            process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        reader.join(timeout=2)
        stderr.close()
        server.shutdown()
        server.server_close()
        (cwd/'events.json').write_text(json.dumps(events,indent=2)+'\n')
        (cwd/'requests.json').write_text(json.dumps(requests,indent=2)+'\n')
    row['process_exit'] = process.returncode
    row['contract_passed'] = row['contract_passed'] and process.returncode == 0
    print(json.dumps(row), flush=True)


for label, nested, yielded in [('flat-blocking',False,False),('nested-blocking',True,False),('yielded-cell-blocking',True,True)]:
    interrupt_case(label,nested,yielded)
interrupt_case('shell-stdin-cancellation',shell=True)

# The deadline must run while the model is doing other work and never polling.
for label, source in [
    ('passive-deadline', 'const r = await tools.shell_run({command:"sleep 0.3; printf escaped > escaped.txt; sleep 1",yield_time_ms:1,timeout_ms:50}); await new Promise(resolve=>setTimeout(resolve,600)); text(await tools.shell_poll({session_id:r.session_id,yield_time_ms:0}));'),
    ('stdin-deadline', 'text(await tools.shell_run({command:"sleep 2",stdin:"x".repeat(1000000),yield_time_ms:0,timeout_ms:100}));'),
]:
    cwd = root/label
    cwd.mkdir()
    started = time.monotonic()
    process = subprocess.run([args.isolate,'--root',str(cwd),'--cell',source],capture_output=True,text=True,timeout=15)
    elapsed = time.monotonic()-started
    row = {'case':label,'exit':process.returncode,'elapsed_seconds':round(elapsed,3),'escaped_child_write':(cwd/'escaped.txt').exists(),'output':process.stdout}
    row['contract_passed'] = process.returncode == 0 and not row['escaped_child_write'] and '"timed_out":true' in process.stdout and (label!='stdin-deadline' or elapsed<1.5)
    results.append(row)
    print(json.dumps(row),flush=True)
(root/'results.json').write_text(json.dumps(results,indent=2)+'\n')
print('Artifacts:',root)
if args.check and not all(row['contract_passed'] for row in results):
    raise SystemExit(1)
