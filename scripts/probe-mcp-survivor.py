#!/usr/bin/env python3
"""Exercise MCP caller contracts against isolated synthetic state. Run on the binary host."""
import argparse, json, os, pathlib, re, socket, subprocess, tempfile, time, urllib.request, uuid
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--repo',type=pathlib.Path,default=pathlib.Path(__file__).resolve().parents[1])
repo=parser.parse_args().repo.resolve()
root=pathlib.Path(tempfile.mkdtemp(prefix='bbox-mcp-http-'))
print('evidence',root,flush=True)
for name in ['state','home','config','cache','data','xdg-state','transcripts','codex']:(root/name).mkdir()
token=root/'synthetic.token'; token.write_text(uuid.uuid4().hex+uuid.uuid4().hex); token.chmod(0o600)
config=root/'daemon.toml'; config.write_text('[code_collection]\nenabled = true\nknowledge_transport_enabled = true\n[[code_collection.producers]]\nproducer_id = \"synthetic-audit\"\ntoken_file = '+json.dumps(str(token))+'\nscopes = [{ repo_id = \"synthetic-audit-repo\", bbox_root_relpath = \".\" }]\n')
sock=socket.socket(); sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]; sock.close()
env={'PATH':os.environ['PATH'],'HOME':str(root/'home'),'XDG_CONFIG_HOME':str(root/'config'),'XDG_CACHE_HOME':str(root/'cache'),'XDG_DATA_HOME':str(root/'data'),'XDG_STATE_HOME':str(root/'xdg-state'),'BLACKBOX_CONFIG':str(config),'BLACKBOX_STATE_DIR':str(root/'state'),'BLACKBOX_DEFAULTS_DIR':str(repo/'system-defaults'),'BLACKBOX_VECTORS_PATH':str(root/'state/vectors'),'TRANSCRIPT_SEARCH_INDEX_PATH':str(root/'index'),'TRANSCRIPT_SEARCH_ROOTS':'throwaway='+str(root/'transcripts'),'TRANSCRIPT_SEARCH_CODEX_ROOT':str(root/'codex'),'BLACKBOX_REINDEX_INTERVAL_SECS':'999999','BLACKBOX_EDGE_INDEX_BOOT_REBUILD':'false','BBOX_BIND':'127.0.0.1','BBOX_PORT':str(port),'BLACKBOX_MCP_NAME':'mcp-audit-isolated','RUST_LOG':'blackbox=info'}
with (root/'genesis.log').open('w') as log:
 subprocess.run([str(repo/'target/debug/blackbox'),'project-catalog','genesis','--config',str(config),'--state-dir',str(root/'state')],env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
# Synthetic dispatches use an inert executable; no provider API is contacted.
env['BRO_HARNESS_BIN']='/bin/true'
log=(root/'daemon.log').open('w'); proc=subprocess.Popen([str(repo/'target/debug/blackboxd')],env=env,stdout=log,stderr=subprocess.STDOUT,cwd=root)
rows=[]; session=None; seq=0; url=f'http://127.0.0.1:{port}/mcp?surface=ops'
def rpc(method,params,notify=False):
 global seq,session
 seq+=1; payload={'jsonrpc':'2.0','method':method,'params':params}
 if not notify:payload['id']=seq
 headers={'Content-Type':'application/json','Accept':'application/json, text/event-stream','Mcp-Protocol-Version':'2025-06-18'}
 if session:headers['Mcp-Session-Id']=session
 with urllib.request.urlopen(urllib.request.Request(url,data=json.dumps(payload).encode(),headers=headers),timeout=45) as r:
  session=r.headers.get('Mcp-Session-Id') or session; raw=r.read().decode()
 if not raw:return {}
 events=[line[6:] for line in raw.splitlines() if line.startswith('data: ')]
 value=json.loads(events[-1] if events else raw)
 (root/f'rpc-{seq:03}.json').write_text(json.dumps(value,ensure_ascii=False,indent=2))
 return value

def call(name,args,expect_error=False):
 value=rpc('tools/call',{'name':name,'arguments':args})
 result=value.get('result',{}); text='\n'.join(c.get('text','') for c in result.get('content',[]) if c.get('type')=='text')
 error=bool(value.get('error') or result.get('isError'))
 size=len(json.dumps(result,ensure_ascii=False,separators=(',',':')).encode())
 assert error==expect_error,(name,list(args),str(value)[:1800])
 assert size<=65536,(name,size)
 rows.append({'tool':name,'case':args.get('action',args.get('detail','read')),'result_bytes':size,'expected_error':expect_error})
 try:body=json.loads(text)
 except ValueError:body=text
 return body
try:
 for attempt in range(300):
  if proc.poll() is not None:raise RuntimeError((root/'daemon.log').read_text()[-4000:])
  try:
   with urllib.request.urlopen(f'http://127.0.0.1:{port}/roster',timeout=.5):break
  except Exception:time.sleep(.2)
 else:raise RuntimeError('daemon did not bind')
 rpc('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'mcp-audit-isolated','version':'1'}})
 rpc('notifications/initialized',{},True)
 catalog=rpc('tools/list',{})['result']['tools']; names={t['name'] for t in catalog}; (root/'catalog.json').write_text(json.dumps(catalog,indent=2)); print('catalog',len(names),flush=True)
 providers=call('bro_providers',{'provider':'glm'}); models=providers['glm']['models']; assert any(m['id']=='glm-5.3-flash' for m in models); assert providers['glm']['defaultModel']=='glm-5.3'; assert isinstance(providers['glm']['peak_usage'],bool); print('Flash catalog and peak advisory PASS',flush=True)
 summaries=call('bro_providers',{})
 assert all(isinstance(summaries[p]['peak_usage'],bool) for p in ['glm','deepseek'])
 assert 'peak_usage' not in summaries['brodex']
 call('bro_dashboard',{})
 dispatch_args={'provider':'glm','prompt':'Synthetic peak advisory fixture','cwd':str(root),'request_key':'synthetic-peak-exec'}
 dispatched=call('bro_exec',dispatch_args)
 assert isinstance(dispatched['peak_usage'],bool),dispatched
 call('bro_wait',{'task_id':dispatched['taskId'],'timeout_seconds':10})
 replayed=call('bro_exec',dispatch_args)
 assert replayed['taskId']==dispatched['taskId'] and replayed['peak_usage']==dispatched['peak_usage'] and replayed['replayed'],replayed
 resume_args={'provider':'glm','session_id':dispatched['sessionId'],'prompt':'Synthetic continuation','cwd':str(root),'request_key':'synthetic-peak-resume'}
 resumed=call('bro_resume',resume_args)
 assert isinstance(resumed['peak_usage'],bool),resumed
 call('bro_wait',{'task_id':resumed['taskId'],'timeout_seconds':10})
 replayed=call('bro_resume',resume_args)
 assert replayed['taskId']==resumed['taskId'] and replayed['peak_usage']==resumed['peak_usage'] and replayed['replayed'],replayed
 print('provider discovery and synthetic dispatch/resume peak advisories PASS',flush=True)
 for name in ['bro_when_any','bro_when_all']:
  call(name,{'task_ids':['00000000-0000-0000-0000-000000000000'],'timeout_seconds':0},True)
 for name,args in [('bro_brofile',{'action':'list','scope':'typo'}),('bro_mcp',{'action':'list','scope':'typo'}),('bro_mcp',{'action':'list','pattern':'ignored'}),('bbox_embed_partitions',{'action':'explode'}),('bbox_thread',{'action':'get','detail':'typo'}),('bbox_packet',{'action':'typo'})]:
  if name in names:call(name,args,True)
 print('selector and wait refusals PASS',flush=True)
 # List before each synthetic creation; all writes stay in the isolated bundle.
 call('bro_brofile',{'action':'list_accounts'})
 account='synthetic-account'; secret='synthetic-secret-never-return'
 call('bro_brofile',{'action':'set_account','name':account,'env':{'CUSTOM_OPAQUE':secret,'API_TOKEN':secret}})
 inventory=call('bro_brofile',{'action':'list_accounts','body_limit':128}); collected=''; cursor=None
 for i in range(100):
  if i:inventory=call('bro_brofile',{'action':'list_accounts','body_limit':128,'cursor':cursor})
  body=inventory['body']; collected+=body['text']; cursor=body.get('next_cursor')
  if not cursor:break
 else:raise AssertionError('account cursor did not finish')
 assert secret not in collected and account in json.loads(collected); print('account redaction and exact inventory PASS',flush=True)
 call('bbox_thread_list',{})
 opened=call('bbox_thread',{'action':'open','topic':'Synthetic MCP audit fixture','name':'mcp-audit-fixture'})
 tid=re.search(r'thread-[0-9a-f]{8}',str(opened)).group(0)
 note='Exact synthetic note: '+('界\n" test '*1500)
 call('bbox_thread',{'action':'continue','id':tid,'note':note})
 summary=call('bbox_thread',{'action':'get','id':tid}); assert note not in json.dumps(summary,ensure_ascii=False)
 history=call('bbox_thread',{'action':'get','id':tid,'detail':'notes'}); print('history shape',list(history),flush=True)
 # The first note is the explicitly appended fixture; open carried no note.
 cursor=None; joined=''
 for i in range(500):
  args={'action':'get','id':tid,'detail':'note','note_index':1,'body_limit':512}
  if cursor:args['cursor']=cursor
  part=call('bbox_thread',args); body=part['body']; joined+=body['text']; cursor=body.get('next_cursor')
  if not cursor:break
 else:raise AssertionError('note cursor did not finish')
 assert json.loads(joined)['note']==note
 print('thread exact recovery PASS',len(note.encode()),'bytes',flush=True)
 for name,args in [('bbox_describe_schema',{}),('bbox_knowledge',{'query':'synthetic-no-match-audit'}),('bro_dashboard',{})]:
  if name in names:call(name,args)
 # Extended adversarial recovery cases for the reviewed integration.
 def exact(name,args,cursor_field='cursor',body_field='body'):
  joined=''; cursor=None
  for n in range(2000):
   request=dict(args)
   if cursor:request[cursor_field]=cursor
   page=call(name,request); body=page[body_field]; joined+=body['text']; cursor=body.get('next_cursor')
   if not cursor:return json.loads(joined)
  raise AssertionError('exact cursor did not finish: '+name)
 call('bro_mcp',{'action':'list'})
 server_name='synthetic-server-'+('界\n"'*700)
 call('bro_mcp',{'action':'add','name':server_name,'url':'https://unit.test/secret-path?token=synthetic-secret','headers':{'X-Custom':'synthetic-secret'}})
 listing=call('bro_mcp',{'action':'list'}); assert server_name not in str(listing)
 inventory=exact('bro_mcp',{'action':'list','body_limit':1024})
 assert server_name in inventory['servers'] and 'synthetic-secret' not in json.dumps(inventory)
 print('MCP exact server identity PASS',flush=True)
 call('bbox_artifact_list',{'kind':'agent'})
 filters=[f'tool_{n:04}_'+('界'*12) for n in range(300)]
 call('bbox_artifact_install',{'kind':'agent','artifact':{'kind':'agent','name':'synthetic-summary-agent','version':1,'manifest':{'description':'Synthetic audit fixture','when_to_use':['when testing exact MCP recovery'],'brofile_inline':{'provider':'glm','filters':{'allow':filters}},'filter_overlay':{'allow':filters,'disallow':[]}}}})
 summary=call('bro_agent_describe',{'agent':'synthetic-summary-agent'})
 assert summary['planes']['computed_merge']['status']=='computed'
 full=exact('bro_agent_describe',{'agent':'synthetic-summary-agent','detail_plane':'summary','body_limit':4096})
 assert full['planes']['computed_merge']['merged']['allow']==filters
 metadata=exact('bro_agent_describe',{'agent':'synthetic-summary-agent','detail_plane':'metadata','body_limit':512})
 assert metadata['name']=='synthetic-summary-agent'
 print('agent summary and installation metadata recovery PASS',flush=True)
 call('bro_allocator_probe',{'provider':'glm'},True)
 probe_text='Synthetic diagnostic: '+('界\n"'*1500)
 call('bro_allocator_probe',{'provider':'glm','raw_summary':probe_text})
 probe=exact('bro_allocator_probe',{'provider':'glm','body_limit':1024})
 assert probe_text in json.dumps(probe,ensure_ascii=False) or any(v==probe_text for v in probe.values()),probe
 call('bro_allocator_status',{'detail':'probes','body_limit':1024})
 print('allocator probe persistence and exact detail PASS',flush=True)
 preview_args={'pin_provider':'glm','pin_model':'glm-5.3-flash','pin_effort':'low','detail':'preview'}
 call('bro_allocator_probe',{'provider':'glm','credential_status':'present','quota_status':'exhausted','quota_confidence':'runtime_rate_limit','cooldown_ms':3600000,'five_hour_utilization':1.0,'raw_summary':'Synthetic runtime 429'})
 preview=exact('bro_allocator_status',preview_args)
 assert all(isinstance(row['peak_usage'],bool) for row in preview['candidates'])
 assert preview['candidates'][0]['exclusion_reason']=='quota_exhausted',preview
 call('bro_allocator_probe',{'provider':'glm','cooldown_until':1})
 preview=exact('bro_allocator_status',preview_args)
 assert all(isinstance(row['peak_usage'],bool) for row in preview['candidates'])
 candidate=preview['candidates'][0]
 assert candidate['eligible'] and preview['selected']['model']=='glm-5.3-flash',preview
 assert candidate['score_components']['quota_capacity']==0.5,preview
 assert candidate['probe']['quota_status']=='unknown' and candidate['probe']['runtime_observation_expired'],preview
 assert 'five_hour_utilization' not in candidate['probe'],preview
 call('bro_allocator_probe',{'provider':'glm','quota_confidence':'quota_probe'})
 preview=exact('bro_allocator_status',preview_args)
 assert all(isinstance(row['peak_usage'],bool) for row in preview['candidates'])
 assert preview['candidates'][0]['exclusion_reason']=='quota_exhausted',preview
 print('runtime quota cooldown expiry and authoritative quota refusal PASS',flush=True)
 call('bbox_packet_list',{})
 consequent='Synthetic large consequent: '+('界\n"'*4000)
 compiled=call('bbox_compile',{'domain':'synthetic-result-fixture','scope':'global','rules':[{'id':'always','antecedent':{'op':'True'},'classification':'pass','consequent':consequent}]})
 pid=re.search(r'packet-[0-9a-f]{8}',str(compiled)).group(0)
 result=call('bbox_apply',{'packet_id':pid,'entity':{}}); assert result['match'] is True and result['detail_limited'] is True
 recovered=exact('bbox_apply',{'packet_id':pid,'entity':{},'result_body_limit':1024},'result_cursor')
 assert recovered['prediction']['consequent']==consequent
 report=call('bbox_audit',{'packet_id':pid,'dataset':[{'entity':{'large':consequent},'expected':'different'}],'mismatch_detail':True})
 assert report['fidelity']==0
 recovered=exact('bbox_audit',{'packet_id':pid,'dataset':[{'entity':{},'expected':'different'}],'result_body_limit':1024},'result_cursor')
 assert recovered['fidelity']==0 and len(recovered['mismatches'])==1
 print('packet exact result and audit recovery PASS',flush=True)
 result={'revision':subprocess.check_output(['git','-C',str(repo),'rev-parse','HEAD'],text=True).strip(),'catalog_tools':len(names),'checks':rows,'max_result_bytes':max(r['result_bytes'] for r in rows),'thread_note_bytes':len(note.encode()),'passed':True}
 (root/'summary.json').write_text(json.dumps(result,indent=2)); print(json.dumps({k:v for k,v in result.items() if k!='checks'}),flush=True)
finally:
 proc.terminate()
 try:proc.wait(timeout=25)
 except subprocess.TimeoutExpired:proc.kill();proc.wait()
 log.close()
 print('isolated daemon stopped; evidence retained',root,flush=True)
