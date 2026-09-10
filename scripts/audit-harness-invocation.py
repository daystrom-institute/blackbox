#!/usr/bin/env python3
"""Diagnostic recorder: successful execution is not a claim of passing contracts."""
from pathlib import Path
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
import argparse, json, os, shlex, shutil, subprocess, tempfile, threading

parser = argparse.ArgumentParser(description="Synthetic full-harness credential-scrub and wrapper-admission observations; no real credentials or model requests.")
parser.add_argument('--harness', default='bro-harness')
parser.add_argument('--check', action='store_true', help='Fail unless the repaired invocation contracts hold')
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
    ('wrapped-scrub', 'exec', {'source':'text(await build.gate('+json.dumps({'command':command+' > gate-observation.txt'})+'));'}, []),
    ('nested-git-diff-scrub', 'exec', {'source':'text(await tools.git_diff({include_untracked:true}));'}, []),
    ('nested-git-hook-scrub', 'exec', {'source':'text(await tools.git_commit({message:"fixture change",paths:["fixture.txt"]}));'}, []),
    ('nested-lsp-scrub', 'exec', {'source':'text(await lsp.executeCommand({language:"rust",command:"fixture"}));'}, []),
    ('diagnostic-lsp-scrub', 'file_write', {'file_path':'fixture.rs','content':'pub fn fixture() {}\n'}, []),
    ('denied-shell-build-gate', 'exec', {'source':'text(await build.gate({command:"printf AUDIT_GATE_RAN > denied-gate-ran.txt; exit 23"}));'}, ['--deny-tools','shell_*']),
]
results = []
try:
    for label, name, args, extra in cases:
        requests.clear(); current = (name,args)
        env = os.environ.copy()
        env.update({'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null','BRO_HOME':str(root/label),'CODEX_HOME':str(root/'empty-codex'),'BRO_HARNESS_TRANSPORT':'openai-chat','OPENAI_BASE_URL':f'http://127.0.0.1:{server.server_port}/v1','OPENAI_API_KEY':'synthetic-fixture','BRO_HARNESS_WEB_SEARCH':'0','BRO_HARNESS_MAX_TURNS':'3','BRO_HARNESS_NUDGES':'0','BRO_HARNESS_SPAWN_SCRUB':'AUDIT_CANARY','AUDIT_CANARY':'synthetic'})
        cwd = root
        if label in ('nested-git-hook-scrub', 'nested-git-diff-scrub'):
            cwd = root/(label+'-repo')
            cwd.mkdir()
            git_env = os.environ.copy()
            git_env.update({'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null'})
            for git_args in [['init','-q'], ['config','user.name','Fixture'], ['config','user.email','fixture@example.invalid'], ['config','commit.gpgsign','false']]:
                subprocess.run(['git','-C',str(cwd),*git_args],env=git_env,check=True,capture_output=True)
            (cwd/'fixture.txt').write_text('synthetic fixture\n')
            hook = cwd/'.git/hooks/pre-commit'
            hook.write_text('#!/bin/sh\n'+command+' > hook-observation.txt\n')
            hook.chmod(0o755)
            if label == 'nested-git-diff-scrub':
                subprocess.run(['git','-C',str(cwd),'add','--','fixture.txt'],env=git_env,check=True,capture_output=True)
                subprocess.run(['git','-C',str(cwd),'commit','-qm','fixture baseline'],env=git_env,check=True,capture_output=True)
                (cwd/'fixture.txt').write_text('changed synthetic fixture\n')
                (cwd/'untracked.txt').write_text('new synthetic fixture\n')
                diff_helper = cwd/'diff-helper.sh'
                diff_helper.write_text('#!/bin/sh\n'+command+'\n')
                diff_helper.chmod(0o755)
                env['GIT_EXTERNAL_DIFF'] = str(diff_helper)
                # Observe the actual Git child environment even when the
                # producer correctly disables external diff commands.
                git_bin = root/(label+'-bin')
                git_bin.mkdir()
                git_observation = root/(label+'-child-observation.txt')
                real_git = shutil.which('git')
                assert real_git
                launcher = git_bin/'git'
                launcher.write_text('#!/bin/sh\n'+command+' >> '+shlex.quote(str(git_observation))+'\nprintf "\\n" >> '+shlex.quote(str(git_observation))+'\nexec '+shlex.quote(real_git)+' "$@"\n')
                launcher.chmod(0o755)
                env['PATH'] = str(git_bin)+os.pathsep+env['PATH']
        if label in ('nested-lsp-scrub', 'diagnostic-lsp-scrub'):
            cwd = root/(label+'-repo')
            cwd.mkdir()
            launcher = cwd/'fake-lsp.sh'
            observation = cwd/'lsp-observation.txt'
            launcher.write_text('#!/bin/sh\n'+command+' > '+shlex.quote(str(observation))+'\nexit 1\n')
            launcher.chmod(0o755)
            env['BRO_LSP_RUST_ANALYZER_BIN'] = str(launcher)
        invocation = [options.harness,'--daemon-worker','--cwd',str(cwd),'--model','gpt-5.5','--code-mode','optional','--system-prompt','','--mcp-config','{"mcpServers":{}}','-p','Complete the fixture task.',*extra]
        process = subprocess.run(invocation, env=env, capture_output=True, text=True, timeout=25)
        outputs = [m.get('content') for request in requests[1:] for m in request.get('messages',[]) if m.get('role')=='tool']
        names = [t['function']['name'] for t in requests[0].get('tools',[])] if requests else []
        row = {'case':label,'exit':process.returncode,'requests':len(requests),'tool_outputs':outputs,'shell_run_visible':'shell_run' in names}
        if label == 'nested-git-hook-scrub':
            observed = cwd/'hook-observation.txt'
            row['hook_observation'] = observed.read_text() if observed.exists() else None
        if label == 'nested-git-diff-scrub':
            observed = root/(label+'-child-observation.txt')
            row['git_child_observations'] = observed.read_text().splitlines() if observed.exists() else []
            row['contract_passed'] = bool(row['git_child_observations']) and all(value == 'CANARY_ABSENT' for value in row['git_child_observations'])
        elif label in ('flat-scrub', 'nested-scrub'):
            rendered = json.dumps(outputs)
            row['contract_passed'] = bool(outputs) and 'CANARY_ABSENT' in rendered and 'CANARY_PRESENT' not in rendered
        elif label == 'wrapped-scrub':
            observed = root/'gate-observation.txt'
            row['gate_observation'] = observed.read_text() if observed.exists() else None
            row['contract_passed'] = row['gate_observation'] == 'CANARY_ABSENT'
        elif label in ('nested-lsp-scrub', 'diagnostic-lsp-scrub'):
            observation = cwd/'lsp-observation.txt'
            row['lsp_observation'] = observation.read_text() if observation.exists() else None
            row['contract_passed'] = row['lsp_observation'] == 'CANARY_ABSENT'
        elif label == 'nested-git-hook-scrub':
            row['contract_passed'] = row['hook_observation'] == 'CANARY_ABSENT'
        else:
            row['gate_executed'] = (root/'denied-gate-ran.txt').exists()
            row['contract_passed'] = bool(outputs) and not row['shell_run_visible'] and not row['gate_executed']
        row['contract_passed'] = row['contract_passed'] and process.returncode == 0
        results.append(row)
        (root/(label+'-wire.json')).write_text(json.dumps(requests,indent=2))
        (root/(label+'-stdout.jsonl')).write_text(process.stdout)
        (root/(label+'-stderr.txt')).write_text(process.stderr)
        print(json.dumps(row), flush=True)
finally:
    server.shutdown(); server.server_close()
(root/'results.json').write_text(json.dumps(results,indent=2))
print('Artifacts:',root)

if options.check and not all(row['contract_passed'] for row in results):
    raise SystemExit(1)
