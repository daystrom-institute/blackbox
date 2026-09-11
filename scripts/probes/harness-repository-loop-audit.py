#!/usr/bin/env python3
"""Replay a repository repair through real harness tools using a local scripted transport.

This verifies execution/lifecycle contracts, not model reasoning or efficiency.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--harness',required=True)
p.add_argument('--compact',action='store_true',help='Force context compaction during the repair')
a=p.parse_args()
harness=str(Path(a.harness).resolve())
with tempfile.TemporaryDirectory(prefix='harness-repository-loop-') as tmp:
    root=Path(tmp).resolve();(root/'src').mkdir()
    (root/'AGENTS.md').write_text('PRESERVE_FIXTURE_AUTHORITY: only change the incorrect total calculation.\n')
    (root/'src/lib.rs').write_text('pub fn total(a: u32,b: u32)->u32 { a - b }\n#[test] fn totals() { assert_eq!(total(5,2),7); }\n')
    rustc=subprocess.check_output(['rustup','which','rustc'],text=True).strip()
    subprocess.run(['git','init','-q'],cwd=root,check=True)
    command=f'{rustc} --test --error-format=json src/lib.rs -o fixture-tests && ./fixture-tests'
    steps=[('tool_search',{'query':'select:git_status','include_schemas':True}),
           ('exec',{'source':'text(await tools.file_read({file_path:"src/lib.rs"})); text(await tools.git_status({}));'}),
           ('exec',{'source':'text(await tools.file_edit({file_path:"src/lib.rs",old_string:"a - b",new_string:"a + b"}));'}),
           ('exec',{'source':'// @exec: {"yield_time_ms":1}\ntext(await build.gate('+json.dumps({'command':command,'anchor_spans':True})+'));'}),
           ('report',{'message':'Applied the arithmetic fix and verified the compiled fixture tests.','needs_input':False}),
           ('final_result',{'ok':True,'files':['src/lib.rs']})]
    requests=[];errors=[];index=0;waiting=False;waiting_id=None;compiled=False;compacted=False
    def texts(request):
        return '\n'.join(m.get('content','') for m in request.get('messages',[]) if m.get('role')=='tool' and isinstance(m.get('content'),str))
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*unused): pass
        def do_POST(self):
            global index,waiting,waiting_id,compiled,compacted
            request=json.loads(self.rfile.read(int(self.headers['Content-Length'])));requests.append(request)
            try:
                raw=json.dumps(request)
                if 'conversation above is being compacted' in raw:
                    compacted=True
                    delta={'content':'<summary>Repair total arithmetic. The edit is applied. Continue the pending tool sequence.</summary>'};finish='stop'
                else:
                    if compacted: assert 'PRESERVE_FIXTURE_AUTHORITY' in raw, 'typed authority not restored after summary omitted it'
                    tooltext=texts(request)
                    if '"ok":true' in tooltext and 'diagnostics_complete' in tooltext: compiled=True
                    if '"ok": true' in tooltext and 'diagnostics_complete' in tooltext: compiled=True
                    if index==1:
                        names=[t.get('function',{}).get('name') for t in request.get('tools',[])]
                        assert 'git_status' in names, names
                    if waiting:
                        latest=[m.get('content','') for m in request['messages'] if m.get('role')=='tool'][-1]
                        match=re.search(r'Script running with cell ID ([^ .]+)',latest)
                        if match:
                            name,value='wait',{'cell_id':match[1],'yield_time_ms':1000}
                        else:
                            assert 'Script completed' in latest,latest
                            waiting=False;name,value=steps[index];index+=1
                    else:
                        name,value=steps[index];index+=1
                        if index==4: waiting=True
                    delta={'tool_calls':[{'index':0,'id':f'call-{len(requests)}','type':'function','function':{'name':name,'arguments':json.dumps(value)}}]};finish='tool_calls'
                chunks=[{'choices':[{'index':0,'delta':delta,'finish_reason':None}]},{'choices':[{'index':0,'delta':{},'finish_reason':finish}],'usage':{'prompt_tokens':160000 if a.compact and index==4 and not compacted else 1,'completion_tokens':1}}]
            except Exception as exc:
                errors.append(repr(exc));chunks=[{'choices':[{'index':0,'delta':{'content':'fixture driver failed'},'finish_reason':'stop'}]}]
            if not request.get('stream') and compacted:
                data=json.dumps({'choices':[{'message':{'content':delta['content']}}]}).encode()
                self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data);return
            data=(''.join('data: '+json.dumps(c)+'\n\n' for c in chunks)+'data: [DONE]\n\n').encode()
            self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
    server=ThreadingHTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    env={k:v for k,v in os.environ.items() if not k.startswith('BRO_HARNESS_')}
    env.update({'HOME':str(root/'home'),'BRO_HOME':str(root/'bro'),'CODEX_HOME':str(root/'codex'),'XDG_CONFIG_HOME':str(root/'config'),'XDG_STATE_HOME':str(root/'state'),'BRO_HARNESS_TRANSPORT':'openai-chat','OPENAI_BASE_URL':f'http://127.0.0.1:{server.server_port}/v1','OPENAI_API_KEY':'fixture','BRO_HARNESS_WEB_SEARCH':'0','BRO_HARNESS_NUDGES':'0','BRO_HARNESS_MAX_TURNS':'20','BRO_HARNESS_COMPACTION_KEEP_TAIL':'2'})
    proc=subprocess.run([harness,'--cwd',str(root),'--model','scripted-contract-fixture','--code-mode','optional','--system-prompt','','--mcp-config','{"mcpServers":{}}','--output-schema',json.dumps({'type':'object','properties':{'ok':{'type':'boolean'},'files':{'type':'array','items':{'type':'string'}}},'required':['ok','files']}),'-p','Fix total and compile/run its test.'],env=env,capture_output=True,text=True,timeout=90)
    server.shutdown();server.server_close();thread.join()
    events=[json.loads(line) for line in proc.stdout.splitlines() if line.startswith('{')]
    assert not errors,(errors,proc.stderr)
    assert proc.returncode==0,(proc.stdout,proc.stderr)
    assert 'a + b' in (root/'src/lib.rs').read_text()
    assert (root/'fixture-tests').exists(),(texts(requests[-1]),proc.stderr)
    subprocess.run([str(root/'fixture-tests')],check=True,capture_output=True)
    assert compiled,texts(requests[-1])
    assert any(e.get('subtype')=='success' for e in events),events
    assert any(t.get('function',{}).get('name')=='wait' for r in requests for m in r.get('messages',[]) for t in m.get('tool_calls',[])), 'yield/wait path not exercised'
    if a.compact: assert compacted and any(e.get('subtype')=='compact_boundary' for e in events), 'compaction was not observed'
    print(json.dumps({'passed':True,'compaction':compacted,'provider':'local scripted transport, no model inference','tools':['tool_search','exec','wait','file_read','git_status','file_edit','build.gate','report','final_result'],'requests':len(requests),'compiled_fixture_test':'passed'}))
