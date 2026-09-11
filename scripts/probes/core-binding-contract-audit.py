#!/usr/bin/env python3
"""Exercise real isolate tools, mutations and compiler output; no model benchmark."""
import argparse
import http.server
import json
import shutil
import subprocess
import tempfile
import threading
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--isolate', default='isolate')
args = parser.parse_args()
binary = str(Path(shutil.which(args.isolate) or args.isolate).resolve())
receipt = []

def run(root, cell, label):
    proc = subprocess.run([binary, '--root', str(root), '--cell', cell], capture_output=True, text=True, timeout=60)
    assert proc.returncode == 0, f'{label}: {proc.stdout}\n{proc.stderr}'
    assert 'PASS' in proc.stdout, f'{label}: {proc.stdout}'
    receipt.append(label)
    print(f'PASS {label}', flush=True)

PRE = '''function check(value, context) { if (!value) throw new Error(context); }
async function refuses(f, pattern) { try { await f(); } catch(e) { check(pattern.test(String(e)), String(e)); return; } throw new Error("expected refusal"); }
'''

with tempfile.TemporaryDirectory(prefix='isolate-core-audit-') as tmp:
    root = Path(tmp).resolve()
    (root / 'src').mkdir()
    (root / 'src/lib.rs').write_text('pub fn total(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn label() -> &\'static str { "item 🦀" }\n')
    (root / 'Order.java').write_text('public class Order {\n    private int count = 2;\n    public int total() { return count; }\n}\n')
    def git(*command):
        return subprocess.check_output(['git', *command], cwd=root, stderr=subprocess.STDOUT, text=True)
    git('init', '-q'); git('config','user.name','Fixture'); git('config','user.email','fixture@example.invalid')
    git('add','src/lib.rs','Order.java'); git('commit','-qm','Initial fixture')
    run(root, PRE + '''
const files = await code.files({dir:'.'});
check(JSON.stringify(files).includes('src/lib.rs'), 'code.files missing source');
const inventory = await code.items({file:'src/lib.rs'});
const total = inventory.items.find(i=>i.name==='total');
check(total?.span.content_sha256.length===64,'items must anchor source');
check((await code.read({span:total.span})).text.includes('a + b'),'read item');
check(JSON.stringify(await code.signature({span:total.span})).includes('total'),'signature');
const lines = await code.readLines({file:'src/lib.rs',startLine:1,endLine:3});
check(lines.text==='pub fn total(a: u32, b: u32) -> u32 {\\n    a + b\\n}\\n','exact line read');
const union = await code.spanUnion({spans:[total.span,lines.span]});
check(JSON.stringify(union).includes(total.span.content_sha256),'spanUnion provenance');
check(JSON.stringify(await code.query({file:'src/lib.rs',query:'(function_item name: (identifier) @name)'})).includes('total'),'query source');
check(JSON.stringify(await code.fields({file:'Order.java'})).includes('count'),'Java fields');
await refuses(()=>code.items({file:'src/lib.rs',files:['Order.java']}), /exactly|either|both/);
await refuses(()=>code.read({span:{...total.span,content_sha256:'0'.repeat(64)}}),/stale|hash/);
await refuses(()=>code.read({span:{...total.span,byte_end:999999}}),/range|bound|length/);
await refuses(()=>code.query({file:'src/lib.rs',query:'(not_a_real_node) @x'}),/query|node/i);
await refuses(()=>code.spanUnion({spans:[total.span,{...total.span,file:'Order.java'}]}),/file|same/);
text('PASS structural facts and invalid addresses');
''', 'all eight code tools and address failures')
    run(root, PRE + '''
check((await tools.file_read({file_path:'src/lib.rs',start_line:2,max_lines:1})).includes('    a + b'),'file read');
check((await tools.smart_read({file_path:'src/lib.rs',start_line:2,max_lines:1})).includes('    a + b'),'read alias');
await tools.file_write({file_path:'notes.txt',content:'first\\nrepeat repeat\\n'});
await refuses(()=>tools.file_edit({file_path:'notes.txt',old_string:'repeat',new_string:'once'}),/unique|multiple|2/);
await tools.file_edit({file_path:'notes.txt',old_string:'repeat',new_string:'once',replace_all:true});
await tools.apply_patch({source:'*** Begin Patch\\n*** Update File: notes.txt\\n@@\\n-first\\n+changed\\n*** End Patch'});
check((await tools.file_read({file_path:'notes.txt'})).includes('changed\\nonce once'),'edit and patch mutation');
check(JSON.stringify(await tools.list_dir({path:'.',limit:1})).includes('offset'),'directory continuation');
check(JSON.stringify(await tools.glob({pattern:'**/*.rs'})).includes('src/lib.rs'),'glob');
check(JSON.stringify(await tools.content_search({pattern:'total',path:'.'})).includes('Order.java'),'search');
await refuses(()=>tools.file_read({file_path:'missing.txt'}),/exist|such|not found/i);
await refuses(()=>tools.content_search({pattern:'[',path:'.'}),/regex|pattern|parse/i);
text('PASS ordinary file surfaces');
''', 'file_read smart_read file_write file_edit apply_patch list_dir glob content_search')
    run(root, PRE + '''
let es = await edits.begin({});
const item = (await code.items({file:'src/lib.rs'})).items.find(i=>i.name==='total');
await edits.replace({es,span:item.span,text:'pub fn total(a: u32, b: u32) -> u32 { a + b + 1 }'});
await edits.createFile({es,path:'created.rs',content:'pub fn created() -> u32 { 7 }\\n'});
check((await edits.apply({es})).applied,'apply replacement/create');
await refuses(()=>edits.apply({es}),/unknown|consum|not found/i);
es = await edits.begin({});
let line = await code.readLines({file:'src/lib.rs',startLine:1,endLine:1});
await edits.insertBefore({es,span:line.span,text:'// before\\n'});
await edits.insertAfter({es,span:line.span,text:'// after\\n'});
check((await edits.apply({es})).applied,'inserts');
es = await edits.begin({});
await edits.replaceText({es,file:'src/lib.rs',find:'a + b + 1',replace:'a + b + 2'});
let comment = await code.readLines({file:'src/lib.rs',startLine:1,endLine:1});
await edits.delete({es,span:comment.span});
let created = await code.readLines({file:'created.rs',startLine:1,endLine:1});
await edits.deleteFile({es,path:'created.rs',contentSha256:created.span.content_sha256});
check((await edits.apply({es})).applied,'text replace/delete/file delete');
es = await edits.begin({});
let current = (await code.items({file:'src/lib.rs'})).items.find(i=>i.name==='total');
await edits.merge({es,changes:[{span:current.span,new_text:'pub fn total(a: u32, b: u32) -> u32 { a + b + 3 }'}]});
check((await edits.apply({es})).applied,'merge');
es = await edits.begin({});
current = (await code.items({file:'src/lib.rs'})).items.find(i=>i.name==='total');
await edits.replace({es,span:current.span,text:'pub fn {'});
const rollback = await edits.apply({es,validations:['tree_sitter_no_errors']});
check(!rollback.applied,'bad syntax accepted');
check((await code.read({span:current.span})).text.includes('a + b + 3'),'rollback source changed');
text('PASS edit algebra and rollback');
''', 'all ten edit tools with source verification and parse rollback')
    # Confirm applied source compiles outside the harness too.
    rustc = shutil.which('rustc')
    subprocess.run([rustc,'--crate-type=lib','src/lib.rs','-o','fixture.rlib'],cwd=root,check=True,capture_output=True)
    (root / 'warning.rs').write_text('pub fn value() -> u32 { let mut value = 1; value + 2 }\n')
    run(root, PRE + f'''
let result = await build.gate({{command:{json.dumps(rustc + ' --crate-type=lib --error-format=json warning.rs -o warning.rlib')},anchor_spans:true}});
check(result.ok && result.diagnostics_complete,'gate failed');
check(result.counts.warnings > 0 && result.diagnostics.some(d=>d.code==='unused_mut' && d.suggestions.some(s=>s.replacement==='')),'raw rustc warning/suggestion lost');
result = await build.gate({{command:'printf "build failed\\\\n" >&2; exit 3'}});
check(!result.ok && result.exit_code===3 && result.diagnostics.length>0,'generic failure lost');
text('PASS compiler diagnostics');
''', 'build.gate real rustc warnings, suggested edits and generic failure')
    (root / 'foreign.txt').write_text('unrelated staged edit\n')
    git('add','foreign.txt')
    run(root, PRE + '''await refuses(()=>tools.git_commit({message:'Must refuse foreign index',paths:['src/lib.rs']}),/staged|index|unrelated/i);text('PASS foreign stage refusal');''', 'git_commit preserves unrelated staged work')
    assert git('diff','--cached','--name-only').strip() == 'foreign.txt'
    git('reset','-q','HEAD','--','foreign.txt')
    run(root, PRE + '''
check(JSON.stringify(await tools.git_status({})).includes('src/lib.rs'),'git status');
check(JSON.stringify(await tools.git_diff({})).includes('a + b + 3'),'git diff');
check(JSON.stringify(await tools.git_log({})).includes('Initial fixture'),'git log');
check(JSON.stringify(await tools.git_show({rev:'HEAD:src/lib.rs'})).includes('a + b'),'git show');
await tools.git_commit({message:'Apply reviewed source edit',paths:['src/lib.rs']});
await refuses(()=>tools.git_commit({message:'No broad staging',paths:['.']}),/literal|directory|file|path/i);
check(JSON.stringify(await tools.sandbox_status({})).includes('root'),'sandbox status');
check(JSON.stringify(await tools.sandbox_grounding({})).includes('launch'),'sandbox grounding');
await tools.todo_write({items:[{task:'compile source',status:'completed'}]});
text('PASS git and session tools');
''', 'five Git tools, sandbox tools and todo_write')
    assert git('show','--format=','--name-only','HEAD').strip() == 'src/lib.rs'
    run(root, PRE + '''
let result = await tools.shell_run({command:"i=0; while [ $i -lt 300 ]; do echo row-$i; i=$((i+1)); done",max_output_tokens:40,yield_time_ms:1000});
let output = result.stdout, pages=1;
while(result.running || result.output_pending) { check(pages++<100,'paging stuck'); result=await tools.shell_poll({session_id:result.session_id,max_output_tokens:40,yield_time_ms:1000}); output+=result.stdout; }
check(output===Array.from({length:300},(_,i)=>'row-'+i+'\\n').join(''),'shell lost/duplicated bytes');
const background = await tools.shell_run({command:'sleep 30',yield_time_ms:1});
check(background.running,'expected running shell');
check(JSON.stringify(await tools.shell_list({})).includes(background.session_id),'session absent');
const killed = await tools.shell_kill({session_id:background.session_id,signal:'term',grace_ms:50});
check(!killed.running,'kill did not stop shell');
text('PASS shell lifecycle');
''', 'four shell tools, exact post-exit drain and process termination')

    bodies = {'/source':('text/plain; charset=utf-8',b'pub fn identity<T>(value: T) -> T {\n    value\n}\n'),
              '/html':('text/html',b'<p>Hello &amp; <b>world</b></p>'),
              '/unicode':('application/json',json.dumps({'source':'🦀 x\n'*5000},ensure_ascii=False).encode()),
              '/binary':('application/octet-stream',b'\x00\xff'),
              '/oversize':('text/plain',b'x'*(2*1024*1024+1))}
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            media, body = bodies.get(self.path, ('text/plain',b'missing'))
            self.send_response(200 if self.path in bodies else 404)
            self.send_header('Content-Type',media); self.send_header('Content-Length',str(len(body))); self.end_headers()
            try: self.wfile.write(body)
            except (BrokenPipeError,ConnectionResetError): pass
        def log_message(self,*a): pass
    server = http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
    thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    base=f'http://127.0.0.1:{server.server_port}'
    run(root,PRE+f'''
const base={json.dumps(base)};
check(await tools.web_fetch({{url:base+'/source'}})==={json.dumps(bodies['/source'][1].decode())},'source altered');
check(await tools.web_fetch({{url:base+'/html'}})==='Hello & world','HTML extraction');
let start=0, hash, output='', pages=0;
while(true) {{
const page=await tools.web_fetch({{url:base+'/unicode',start_char:start,expected_sha256:hash}});
const marker=page.match(/\\n\\[page truncated; continue SAME url with start_char=(\\d+) and expected_sha256="([a-f0-9]+)"\\]$/);
if(!marker){{output+=page;break;}}
output+=page.slice(0,marker.index); start=Number(marker[1]); hash=marker[2];check(pages++<100,'web paging stuck');
}}
check(JSON.parse(output).source==='🦀 x\\n'.repeat(5000) && pages>1,'web paging lost bytes');
await refuses(()=>tools.web_fetch({{url:base+'/source',start_char:1,expected_sha256:'0'.repeat(64)}}),/changed/);
await refuses(()=>tools.web_fetch({{url:base+'/binary'}}),/media type/);
await refuses(()=>tools.web_fetch({{url:base+'/oversize'}}),/2 MiB/);
await refuses(()=>tools.web_fetch({{url:base+'/missing'}}),/404/);
text('PASS web content integrity');
''','web_fetch exact code, HTML, Unicode paging, changed-source and format refusals')
    server.shutdown();server.server_close();thread.join()
print(json.dumps({'passed':receipt},indent=2))
