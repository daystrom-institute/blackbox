#!/usr/bin/env python3
"""Capture harness observations, or assert workspace contracts with --check-workspace."""
from pathlib import Path
from http.server import ThreadingHTTPServer,BaseHTTPRequestHandler
import argparse,json,os,subprocess,tempfile,threading,collections,time,sys
parser=argparse.ArgumentParser(description="Synthetic installed-harness contract audit. Uses local HTTP fixtures, fake credentials and temporary files; no model/provider requests.")
parser.add_argument('--isolate',default='isolate')
parser.add_argument('--harness',default='bro-harness')
parser.add_argument('--check-workspace',action='store_true',help='Assert search/glob/edit/patch workspace contracts, exit nonzero on failure, and skip HTTP/provider-loop fixtures')
parser.add_argument('--output-dir',type=Path,help='New, nonexistent fixture/output directory')
args=parser.parse_args()
if args.output_dir:
    args.output_dir.mkdir(parents=True,exist_ok=False)
    ROOT=args.output_dir.resolve()
else:
    ROOT=Path(tempfile.mkdtemp(prefix='harness-contract-audit-')).resolve()
ISO=args.isolate
HARNESS=args.harness
results=[]
fixture_env=os.environ.copy()
fixture_env.update({'HOME':str(ROOT/'home'),'XDG_CONFIG_HOME':str(ROOT/'config'),'XDG_STATE_HOME':str(ROOT/'state'),'BRO_HOME':str(ROOT/'bro-home')})
for directory in ['home','config','state','bro-home']:(ROOT/directory).mkdir()
def iso(name,args):
 r=subprocess.run([ISO,'--root',str(ROOT),name,'--args',json.dumps(args)],env=fixture_env,capture_output=True,text=True,timeout=20)
 return {'exit':r.returncode,'output':r.stdout.strip(),'error':r.stderr.strip()}
(ROOT/'large.txt').write_text('KNOWN_NEEDLE\n'+'x'*2_000_000)
(ROOT/'build').mkdir();(ROOT/'build'/'logic.txt').write_text('KNOWN_NEEDLE\nONLY_IN_BUILD\n')
results.append({'case':'oversized-file-search','actual':iso('content_search',{'path':'large.txt','pattern':'KNOWN_NEEDLE'}),'expected':'match or explicit incomplete-search disclosure'})
results.append({'case':'explicit-pruned-directory','actual':iso('content_search',{'path':'build','pattern':'KNOWN_NEEDLE'}),'expected':'match or explicit excluded-directory disclosure'})
results.append({'case':'default-pruned-directory','actual':iso('content_search',{'pattern':'ONLY_IN_BUILD'}),'expected':'match or explicit incomplete-search disclosure'})
(ROOT/'small.txt').write_text('MATCH\n')
results.append({'case':'zero-search-limit','actual':iso('content_search',{'path':'small.txt','pattern':'MATCH','max_results':0}),'expected':'reject zero or return zero matches'})
(ROOT/'nested').mkdir();(ROOT/'nested'/'unit.rs').write_text('MATCH\n')
results.append({'case':'path-glob-search','actual':iso('content_search',{'pattern':'MATCH','glob':'nested/*.rs'}),'classification':'ergonomic divergence; filename-only glob is documented','expected':'Compare with glob path-pattern behavior before substituting one tool for the other'})
if args.check_workspace:
 for replace_all in [False,True]:
  target=ROOT/'edit-empty.txt';target.write_bytes(b'unchanged\r\n')
  actual=iso('file_edit',{'file_path':'edit-empty.txt','old_string':'','new_string':'inserted','replace_all':replace_all})
  actual['unchanged']=target.read_bytes()==b'unchanged\r\n'
  results.append({'case':f'empty-edit-needle-{str(replace_all).lower()}','actual':actual})
 target=ROOT/'crlf.txt';target.write_bytes(b'first\r\nold\r\nlast\r\n')
 actual=iso('apply_patch',{'patch':'*** Begin Patch\n*** Update File: crlf.txt\n@@\n-old\n+new\n*** End Patch'})
 actual['bytes_preserved']=target.read_bytes()==b'first\r\nnew\r\nlast\r\n'
 results.append({'case':'patch-uniform-crlf','actual':actual})
 results.extend([
  {'case':'explicit-file-search','actual':iso('content_search',{'path':'build/logic.txt','pattern':'KNOWN_NEEDLE'})},
  {'case':'explicit-file-glob','actual':iso('glob',{'path':'build/logic.txt','pattern':'**/*.txt'})},
  {'case':'zero-glob-limit','actual':iso('glob',{'pattern':'*.txt','max_results':0})},
 ])
 checks={
  'empty-edit-needle-false':lambda r:r['unchanged'] and 'old_string must not be empty' in r['output']+r['error'],
  'empty-edit-needle-true':lambda r:r['unchanged'] and 'old_string must not be empty' in r['output']+r['error'],
  'patch-uniform-crlf':lambda r:r['exit']==0 and r['bytes_preserved'],
  'oversized-file-search':lambda r:'oversized=1' in r['output'] and 'complete_within_scope=false' in r['output'],
  'explicit-pruned-directory':lambda r:r['exit']==0 and 'build/logic.txt:1:KNOWN_NEEDLE' in r['output'] and 'pruned_dirs=0' in r['output'],
  'default-pruned-directory':lambda r:'pruned_dirs=1' in r['output'] and 'complete_within_scope=false' in r['output'],
  'zero-search-limit':lambda r:'max_results must be greater than zero' in r['output']+r['error'],
  'explicit-file-search':lambda r:r['exit']==0 and 'build/logic.txt:1:KNOWN_NEEDLE' in r['output'],
  'explicit-file-glob':lambda r:r['exit']==0 and 'build/logic.txt' in r['output'] and 'pruned_dirs=0' in r['output'],
  'zero-glob-limit':lambda r:'max_results must be greater than zero' in r['output']+r['error'],
 }
 failures=[]
 for result in results:
  check=checks.get(result['case'])
  if check is not None:
   result['passed']=bool(check(result['actual']))
   if not result['passed']:failures.append(result['case'])
  print(json.dumps(result))
 (ROOT/'results.json').write_text(json.dumps(results,indent=2)+'\n')
 print(json.dumps({'workspace_checks':len(checks),'failed':failures,'artifacts':str(ROOT)}))
 sys.exit(1 if failures else 0)
requests=[];case_mode='normal'
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  requests.append(body)
  if case_mode=='bad-final':
   delta={'tool_calls':[{'index':0,'id':'call-final','type':'function','function':{'name':'final_result','arguments':'{"answer":false}'}}]}
   stop='tool_calls'
  else: delta={'content':'fixture done'};stop='stop'
  chunks=[{'choices':[{'index':0,'delta':delta,'finish_reason':None}]}]
  if case_mode!='truncated':chunks.append({'choices':[{'index':0,'delta':{},'finish_reason':stop}]})
  data=''.join('data: '+json.dumps(c)+'\n\n' for c in chunks)
  if case_mode!='truncated':data+='data: [DONE]\n\n'
  data=data.encode();self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
try:
 for mode,case in [('off','normal'),('optional','normal'),('only','normal'),('off','truncated'),('off','bad-final')]:
  requests.clear();case_mode=case
  env=os.environ.copy();env.update({'BRO_HOME':str(ROOT/f'home-{mode}-{case}'),'CODEX_HOME':str(ROOT/'empty-codex'),'BRO_HARNESS_TRANSPORT':'openai-chat','OPENAI_BASE_URL':f'http://127.0.0.1:{server.server_port}/v1','OPENAI_API_KEY':'synthetic-fixture','BRO_HARNESS_WEB_SEARCH':'0','BRO_HARNESS_MAX_TURNS':'3','BRO_HARNESS_NUDGES':'0'})
  args=[HARNESS,'--cwd',str(ROOT),'--model','gpt-5.5','--code-mode',mode,'--system-prompt','','--mcp-config','{"mcpServers":{}}','-p','Complete the fixture task.']
  if case=='bad-final':args+=['--output-schema','{"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"]}']
  r=subprocess.run(args,env=env,capture_output=True,text=True,timeout=20)
  events=[]
  for line in r.stdout.splitlines():
   try:events.append(json.loads(line))
   except ValueError:pass
  names=[t['function']['name'] for t in requests[0].get('tools',[])] if requests else []
  system=[m['content'] for m in requests[0]['messages'] if m['role']=='system'] if requests else []
  row={'case':case,'code_mode':mode,'exit':r.returncode,'tools':names,'tool_schema_bytes':len(json.dumps(requests[0].get('tools',[]))) if requests else 0,'system_bytes':len(json.dumps(system)),'events':events,'stderr':r.stderr}
  results.append(row)
  (ROOT/f'wire-{mode}-{case}.json').write_text(json.dumps(requests,indent=2))
finally:server.shutdown();server.server_close()
(ROOT/'results.json').write_text(json.dumps(results,indent=2)+'\n')
for r in results:
 if 'actual' in r:print(json.dumps(r))
 else:print(json.dumps({k:v for k,v in r.items() if k not in ['events','stderr']} | {'terminal':[e for e in r['events'] if e.get('type')=='result']}))
print('Artifacts:',ROOT)
