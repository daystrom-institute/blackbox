#!/usr/bin/env python3
"""Exercise installed Java bindings, apply returned proposals, and compile fixtures."""
import argparse, hashlib, json, os, pathlib, re, shutil, subprocess, tempfile

PAR=argparse.ArgumentParser(description="Run Java tool calls and edits in private temporary fixtures. --verify checks compile, behavior, selected contracts, and retirement. No model/provider or shared daemon is used."); PAR.add_argument('--isolate',default=shutil.which('isolate')); PAR.add_argument('--output'); PAR.add_argument('--case',action='append'); PAR.add_argument('--compact-layout',action='store_true'); PAR.add_argument('--verify',action='store_true'); ARGS=PAR.parse_args()
if not ARGS.isolate: PAR.error('isolate is unavailable; pass --isolate')
ARGS.isolate=str(pathlib.Path(ARGS.isolate).resolve())
OUT=pathlib.Path(ARGS.output or tempfile.mkdtemp(prefix='java-binding-audit-')).resolve(); OUT.mkdir(parents=True,exist_ok=True)
HOME_DIR=OUT/'home'; HOME_DIR.mkdir(exist_ok=True)
ENV={'PATH':os.environ['PATH'],'HOME':str(HOME_DIR),'BRO_HOME':str(HOME_DIR/'bro'),'CODEX_HOME':str(HOME_DIR/'codex')}
JAVAC=shutil.which('javac'); JAVA=shutil.which('java')
if os.environ.get('JAVA_HOME'): ENV['JAVA_HOME']=os.environ['JAVA_HOME']
RECEIPTS=[]
INVENTORY=subprocess.check_output([ARGS.isolate,'--list'],env=ENV,text=True).splitlines()
(OUT/'inventory.json').write_text(json.dumps(INVENTORY,indent=2)+'\n')
PRELUDE=r"""
async function probe(name,args) { try { const value=await java[name](args); records.push({name:'java.'+name,args,value}); return value; } catch(error) { records.push({name:'java.'+name,args,error:String(error)}); throw error; } }
async function apply(proposal) { const es=await edits.begin(); for(const c of proposal.creates||[]) await edits.createFile({es,path:c.path,content:c.content}); for(const d of proposal.deletes||[]) await edits.deleteFile({es,path:d.path,contentSha256:d.content_sha256}); if(proposal.changes?.length) await edits.merge({es,changes:proposal.changes}); const applied=await edits.apply({es}); records.push({name:'edits.apply',value:applied}); return applied; }
let records=[];
"""

def command(args,cwd,timeout=35):
 try:
  r=subprocess.run(args,cwd=cwd,env=ENV,text=True,capture_output=True,timeout=timeout)
  return {'returncode':r.returncode,'stdout':r.stdout,'stderr':r.stderr}
 except subprocess.TimeoutExpired as e:
  return {'timeout':timeout,'stdout':(e.stdout or b'').decode() if isinstance(e.stdout,bytes) else e.stdout,'stderr':(e.stderr or b'').decode() if isinstance(e.stderr,bytes) else e.stderr}

def compile_java(root):
 sources=sorted(str(p.relative_to(root)) for p in root.rglob('*.java'))
 if not sources: return {'returncode':0,'note':'no Java source in metadata-only scenario'}
 if not JAVAC: return {'unavailable':'javac'}
 classes=root/'classes'
 if classes.exists(): shutil.rmtree(classes)
 classes.mkdir()
 result=command([JAVAC,'-d',str(classes),*sources],root)
 if result.get('returncode')==0 and (root/'AuditMain.java').exists(): result['run']=command([JAVA,'-cp',str(classes),'AuditMain'],root)
 return result

def scenario(name,files,body):
 if ARGS.case and name not in ARGS.case: return
 root=OUT/name
 if root.exists(): raise RuntimeError(f'refusing to overwrite existing scenario {root}')
 root.mkdir()
 if not ARGS.compact_layout:
  files={p:s.replace('{ ', '{\n    ').replace('; ', ';\n    ').replace('} ', '}\n').replace(' }', '\n}') for p,s in files.items()}
  body=body.replace('int doubled = x * 2; int result = doubled + 1;', 'int doubled = x * 2;\\n    int result = doubled + 1;')
 for path,content in files.items():
  dest=root/path; dest.parent.mkdir(parents=True,exist_ok=True); dest.write_text(content)
 before={p:hashlib.sha256(s.encode()).hexdigest() for p,s in files.items()}
 baseline=compile_java(root)
 source=PRELUDE+'\ntry {\n'+body+'\n} catch(error) { records.push({fatal:String(error)}); }\ntext(records);'
 (root/'cell.js').write_text(source)
 result=command([ARGS.isolate,'--root',str(root),'--cell-timeout','30','--cell-file',str(root/'cell.js')],root,40)
 records=[]
 for line in result.get('stdout','').splitlines():
  if line.startswith('['):
   try: records=json.loads(line)
   except json.JSONDecodeError: pass
 after={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in root.rglob('*.java')}
 receipt={'case':name,'before':before,'baseline':baseline,'invocation':result,'records':records,'after':after,'compiled':compile_java(root)}
 (root/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n'); RECEIPTS.append(receipt)
 print(json.dumps({'case':name,'baseline':baseline.get('returncode'),'calls':[{k:r[k] for k in ('name','error','fatal') if k in r}|({'blocked':r['value'].get('blocked'),'applied':r['value'].get('applied')} if isinstance(r.get('value'),dict) else {}) for r in records],'compiled':receipt['compiled'],'timeout':result.get('timeout')}),flush=True)

CALC={'Calc.java':'public class Calc { public int add(int x, int y) { return x + y; } }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a) { System.out.println(new Calc().add(2,3)); } }\n'}
scenario('signature',CALC,r"""const args={file:'Calc.java',methodName:'add',files:['Calc.java','AuditMain.java'],targetParams:[{sourceName:'x',name:'left'},{sourceName:'y',name:'right'}]}; const p=await probe('changeSignaturePreview',args); try{await probe('changeSignature',{...args,methodRef:'stale'});}catch(_){} try{await probe('changeSignature',{...args,methodRef:p.method_ref});}catch(_){} await apply(await probe('changeSignature',{...args,methodRef:p.method_ref,acknowledgeSyntaxOnlyCallSites:true}));""")
scenario('encapsulate',{'Counter.java':'public class Counter { public int count; public int current(){ return count; } }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a) { Counter c=new Counter(); System.out.println(c.count); } }\n'},r"""const args={file:'Counter.java',fieldName:'count',files:['Counter.java','AuditMain.java']}; const p=await probe('encapsulateFieldPreview',args); try{await probe('encapsulateField',{...args,fieldRef:'stale'});}catch(_){} await apply(await probe('encapsulateField',{...args,fieldRef:p.field_ref,rewriteReferences:'files',acknowledgeSyntaxOnlyReferences:true}));""")
scenario('factory',{'Order.java':'public class Order { public final int id; public Order(int id){this.id=id;} }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a) { System.out.println(new Order(7).id); } }\n'},r"""const args={file:'Order.java',files:['Order.java','AuditMain.java']};const p=await probe('replaceConstructorWithFactoryPreview',args);try{await probe('replaceConstructorWithFactory',{...args,constructorRef:'stale'});}catch(_){} await apply(await probe('replaceConstructorWithFactory',{...args,constructorRef:p.constructor_ref,acknowledgeSyntaxOnlyCallSites:true}));""")
scenario('inline',{'Calc.java':'public class Calc { private int add(int x,int y){ return x+y; } public int value(){ return add(2,3); } }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){System.out.println(new Calc().value());} }\n'},r"""const args={file:'Calc.java',methodName:'add'};const p=await probe('inlineMethodPreview',args);try{await probe('inlineMethod',{...args,methodRef:'stale'});}catch(_){}await apply(await probe('inlineMethod',{...args,methodRef:p.method_ref}));""")
scenario('migrate',{'Service.java':'interface Api { int value(); } class Impl implements Api { public int value(){return 9;} } public class Service { Impl field=new Impl(); public Impl get(){return field;} }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){System.out.println(new Service().get().value());} }\n'},r"""const args={file:'Service.java',oldType:'Impl',newType:'Api'};const p=await probe('migrateTypeUsagesPreview',args);try{await probe('migrateTypeUsages',{...args,migrationRef:'stale'});}catch(_){}await apply(await probe('migrateTypeUsages',{...args,migrationRef:p.migration_ref}));""")
scenario('interface',CALC,r"""const p=await probe('pullUpPreview',{file:'Calc.java'});const refs=p.candidates.filter(c=>!c.blockers.length).map(c=>c.ref);try{await probe('extractInterface',{file:'Calc.java',target:'CalcApi.java',typeName:'CalcApi',memberRefs:['stale']});}catch(_){}await apply(await probe('extractInterface',{file:'Calc.java',target:'CalcApi.java',typeName:'CalcApi',memberRefs:refs}));""")
scenario('pullup',CALC|{'CalcApi.java':'public interface CalcApi {}\n'},r"""const p=await probe('pullUpPreview',{file:'Calc.java'});try{await probe('pullUpMembers',{file:'Calc.java',target:'CalcApi.java',memberRefs:['stale']});}catch(_){}await apply(await probe('pullUpMembers',{file:'Calc.java',target:'CalcApi.java',memberRefs:p.candidates.filter(c=>!c.blockers.length).map(c=>c.ref)}));""")
scenario('pushdown',{'Base.java':'public class Base { public int value(){return 5;} }\n','Child.java':'public class Child extends Base {}\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){System.out.println(new Child().value());} }\n'},r"""const args={file:'Base.java',target:'Child.java'};const p=await probe('pushDownMembersPreview',args);try{await probe('pushDownMembers',{...args,memberRefs:['stale']});}catch(_){}await apply(await probe('pushDownMembers',{...args,memberRefs:p.candidates.filter(c=>!c.blockers.length).map(c=>c.ref)}));""")
scenario('movefield',{'Source.java':'public class Source { public int count=5; }\n','Target.java':'public class Target {}\n'},r"""const args={file:'Source.java',target:'Target.java',memberNames:['count'],memberKind:'field'};const p=await probe('moveMemberPreview',args);try{await probe('moveMember',{...args,memberRefs:['stale']});}catch(_){}await apply(await probe('moveMember',{...args,memberRefs:p.members.map(m=>m.ref)}));""")
scenario('moveconstant',{'Source.java':'public class Source { public static final int LIMIT=5; }\n','Target.java':'public class Target {}\n'},r"""const args={file:'Source.java',target:'Target.java',memberNames:['LIMIT'],memberKind:'constant'};const p=await probe('moveMemberPreview',args);try{await probe('moveMember',{...args,memberRefs:['stale']});}catch(_){}await apply(await probe('moveMember',{...args,memberRefs:p.members.map(m=>m.ref)}));""")
scenario('movemethod',{'Source.java':'public class Source { public static int value(){return 5;} }\n','Target.java':'public class Target {}\n'},r"""const args={file:'Source.java',target:'Target.java',memberNames:['value'],memberKind:'method'};const p=await probe('moveMemberPreview',args);try{await probe('moveMember',{...args,memberRefs:['stale']});}catch(_){}await apply(await probe('moveMember',{...args,memberRefs:p.members.map(m=>m.ref)}));""")
INJECT={'jakarta/inject/Inject.java':'package jakarta.inject; @java.lang.annotation.Target({java.lang.annotation.ElementType.FIELD,java.lang.annotation.ElementType.CONSTRUCTOR}) public @interface Inject {}\n'}
scenario('inject',INJECT|{'Service.java':'import jakarta.inject.Inject; public class Service { @Inject private Runnable dependency; public void run(){dependency.run();} }\n'},r"""const p=await probe('fieldInjectToConstructorPreview',{file:'Service.java'});try{await probe('fieldInjectToConstructor',{file:'Service.java',fieldRefs:['stale']});}catch(_){}await apply(await probe('fieldInjectToConstructor',{file:'Service.java',fieldRefs:p.fields.filter(f=>!f.blockers?.length).map(f=>f.ref)}));""")
scenario('unusedctor',INJECT|{'Service.java':'import jakarta.inject.Inject; public class Service { @Inject public Service(Runnable unused) {} }\n'},r"""await apply(await probe('removeUnusedConstructorParams',{file:'Service.java'}));""")
scenario('extract',{'Service.java':'public class Service { private int count=0; public int next(){return ++count;} public int unrelated(){return 42;} }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){Service s=new Service();System.out.println(s.next()+s.next());} }\n'},r"""await probe('extractClassPreviewPlan',{file:'Service.java',methods:['next'],moveFields:['count']});await apply(await probe('extractClass',{file:'Service.java',target:'Counter.java',delegateField:'counter',methods:['next'],moveFields:['count'],wrappers:true}));try{await probe('extractClass',{file:'Service.java',target:'Counter.java',delegateField:'counter',methods:['next']});}catch(_){}""")
scenario('wrappers',{'Service.java':'public class Service { private final Helper helper=new Helper(); public int value(){return increment(4);} }\n','Helper.java':'public class Helper { public int increment(int x){return x+1;} }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){System.out.println(new Service().value());} }\n'},r"""await apply(await probe('synthesizeHelperWrappers',{file:'Service.java',target:'Helper.java',delegateField:'helper',methods:['increment']}));""")
scenario('extractblock',{'Calc.java':'public class Calc { public int value(int x){ int doubled = x * 2; int result = doubled + 1; return result; } }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){System.out.println(new Calc().value(2));} }\n'},r"""await apply(await probe('extractMethodCodeBlock',{file:'Calc.java',oldText:'int doubled = x * 2; int result = doubled + 1;',methodName:'calculate'}));""")
scenario('rename',CALC,r"""await apply(await probe('renameSymbol',{oldName:'add',newName:'sum',file:'Calc.java'}));""")
scenario('addimport',{'Order.java':'package fixture; public class Order {}\n'},r"""await apply(await probe('addImport',{file:'Order.java',imports:['java.util.List']}));await probe('addImport',{file:'Order.java',imports:['java.util.List']});try{await probe('addImport',{file:'Order.java',imports:['java.util.List','java.awt.List']});}catch(_){}""")
DIRTY={'Order.java':'package fixture;\nimport java.util.Set;\nimport java.util.List;\npublic class Order {   \n public List<String> names;   \n\n\n\n}\n'}
scenario('imports',DIRTY,r"""try{await probe('organizeImports',{files:['Missing.java']});}catch(_){}await apply(await probe('organizeImports',{files:['Order.java']}));""")
scenario('whitespace',DIRTY,r"""try{await probe('normalizeWhitespace',{files:[]});}catch(_){}await apply(await probe('normalizeWhitespace',{files:['Order.java']}));""")
scenario('hygiene',DIRTY,r"""try{await probe('hygiene',{files:['Order.java'],imports:false,whitespace:false});}catch(_){}await apply(await probe('hygiene',{files:['Order.java']}));""")
scenario('moveclass',{'src/fixture/Order.java':'package fixture; public class Order {}\n'},r"""await probe('moveClass',{file:'src/fixture/Order.java',targetPackage:'relocated'});""")
scenario('movepackage',{'src/fixture/Order.java':'package fixture; public class Order {}\n'},r"""await probe('movePackage',{oldPackage:'fixture',targetPackage:'relocated',files:['src/fixture/Order.java']});""")
scenario('describe',{},r"""await probe('describe',{transform:'extractClass'});try{await probe('describe',{transform:'missingTransform'});}catch(_){}""")
scenario('signature-added',CALC,r"""const args={file:'Calc.java',methodName:'add',files:['Calc.java','AuditMain.java'],targetParams:[{name:'x'},{name:'y'},{name:'extra',type:'int',defaultValue:'0'}]};const p=await probe('changeSignaturePreview',args);await apply(await probe('changeSignature',{...args,methodRef:p.method_ref,acknowledgeSyntaxOnlyCallSites:true}));""")
scenario('signature-literals',{'Calc.java':'public class Calc { public String label(String x){return "x="+x;} }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){System.out.println(new Calc().label("value"));} }\n'},r"""const args={file:'Calc.java',methodName:'label',targetParams:[{sourceName:'x',name:'input'}]};const p=await probe('changeSignaturePreview',args);await apply(await probe('changeSignature',{...args,methodRef:p.method_ref,acknowledgeSyntaxOnlyCallSites:true}));""")
scenario('staticfield',{'Counter.java':'public class Counter { public static int count=1; }\n'},r"""const args={file:'Counter.java',fieldName:'count'};const p=await probe('encapsulateFieldPreview',args);await apply(await probe('encapsulateField',{...args,fieldRef:p.field_ref,acknowledgeSyntaxOnlyReferences:true}));""")
scenario('unusedctor-callers',INJECT|{'Service.java':'import jakarta.inject.Inject; public class Service { @Inject public Service(Runnable unused) {} }\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){new Service(null);System.out.println("ready");} }\n'},r"""await apply(await probe('removeUnusedConstructorParams',{file:'Service.java'}));""")
ECLIPSE={'.project':'<projectDescription><name>fixture</name><buildSpec><buildCommand><name>org.eclipse.jdt.core.javabuilder</name></buildCommand></buildSpec><natures><nature>org.eclipse.jdt.core.javanature</nature></natures></projectDescription>', '.classpath':'<classpath><classpathentry kind="src" path="src"/><classpathentry kind="con" path="org.eclipse.jdt.launching.JRE_CONTAINER"/><classpathentry kind="output" path="classes"/></classpath>', 'src/fixture/Order.java':'package fixture; public class Order {}\n','src/relocated/Marker.java':'package relocated; public class Marker {}\n','src/fixture/Invoice.java':'package fixture; public class Invoice { public Order order(){return new Order();} }\n','src/client/Caller.java':'package client; import fixture.Order; public class Caller { public Order order(){return new Order();} }\n'}
scenario('moveclass-project',ECLIPSE,r"""await apply(await probe('moveClass',{file:'src/fixture/Order.java',targetPackage:'relocated'}));""")
scenario('movepackage-project',ECLIPSE,r"""await apply(await probe('movePackage',{oldPackage:'fixture',targetPackage:'relocated',files:['src/fixture/Order.java','src/fixture/Invoice.java']}));""")
scenario('annotated-interface',{'Calc.java':'public class Calc { @Deprecated public int add(int x,int y){return x+y;} }\n'},r"""const p=await probe('pullUpPreview',{file:'Calc.java'});await apply(await probe('extractInterface',{file:'Calc.java',target:'CalcApi.java',typeName:'CalcApi',memberRefs:p.candidates.map(c=>c.ref)}));""")
scenario('unusedctor-noinject',{'Service.java':'public class Service { public Service(Runnable unused) {} }\n'},r"""await probe('removeUnusedConstructorParams',{file:'Service.java'});""")
scenario('extractblock-control',{'Calc.java':'public class Calc { public int value(int x){ if(x<0) return -1; return x; } }\n'},r"""await probe('extractMethodCodeBlock',{file:'Calc.java',oldText:'if(x<0) return -1;',methodName:'guard'});""")
scenario('inline-state',{'Calc.java':'public class Calc { private int count; private int next(){return ++count;} public int value(){return next();} }\n'},r"""const args={file:'Calc.java',methodName:'next'};const p=await probe('inlineMethodPreview',args);await probe('inlineMethod',{...args,methodRef:p.method_ref||'unavailable'});""")
scenario('signature-overload',{'Calc.java':'public class Calc { public int add(int x){return x;} public int add(int x,int y){return x+y;} }\n'},r"""const args={file:'Calc.java',methodName:'add',targetParams:[{name:'x'}]};const p=await probe('changeSignaturePreview',args);await probe('changeSignature',{...args,methodRef:p.method_ref||'unavailable'});""")
scenario('movemethod-copy',{'Source.java':'public class Source { public static int value(){return 5;} }\n','Target.java':'public class Target {}\n','AuditMain.java':'public class AuditMain { public static void main(String[] a){System.out.println(Source.value());} }\n'},r"""const args={file:'Source.java',target:'Target.java',memberNames:['value'],memberKind:'method',keepCopy:true};const p=await probe('moveMemberPreview',args);await apply(await probe('moveMember',{...args,memberRefs:p.members.map(m=>m.ref)}));""")
scenario('inject-existing',INJECT|{'Service.java':'import jakarta.inject.Inject; public class Service { @Inject private Runnable dependency; private final String name; public Service(String name) { this.name = name; } public void run(){dependency.run();} }\n'},r"""const p=await probe('fieldInjectToConstructorPreview',{file:'Service.java'});await apply(await probe('fieldInjectToConstructor',{file:'Service.java',fieldRefs:p.fields.map(f=>f.ref)}));""")
COLUMN={
 'com/vaadin/flow/function/ValueProvider.java':'package com.vaadin.flow.function; public interface ValueProvider<T,V> { V apply(T value); }\n',
 'com/vaadin/flow/component/grid/ColumnTextAlign.java':'package com.vaadin.flow.component.grid; public enum ColumnTextAlign { START,CENTER,END }\n',
 'com/vaadin/flow/component/grid/Grid.java':"package com.vaadin.flow.component.grid; import com.vaadin.flow.function.ValueProvider; public class Grid<T> { public Column<T> addColumn(ValueProvider<T,?> p){return new Column<>();} public static class Column<T> { public Column<T> setKey(String x){return this;} public Column<T> setHeader(String x){return this;} public Column<T> setAutoWidth(boolean x){return this;} public Column<T> setTextAlign(ColumnTextAlign x){return this;} } }\n",
 'fixture/View.java':"package fixture;\nimport com.vaadin.flow.component.grid.Grid;\nimport com.vaadin.flow.component.grid.ColumnTextAlign;\npublic class View { public record Row(String name) {} public void input(Grid<Row> grid) { grid.addColumn(Row::name).setKey(\"name\").setHeader(\"Name\").setTextAlign(ColumnTextAlign.START); } public void output(Grid<Row> grid) { grid.addColumn(Row::name).setKey(\"name\").setHeader(\"Name\").setTextAlign(ColumnTextAlign.START); } }\n"
}
scenario('column',COLUMN,r"""await apply(await probe('extractColumnSpec',{file:'fixture/View.java',methods:['input','output'],target:'fixture/ColumnSpec.java'}));""" if 'java.extractColumnSpec' in INVENTORY else r"""await probe('describe',{transform:'extractColumnSpec'});""")
(OUT/'summary.json').write_text(json.dumps(RECEIPTS,indent=2)+'\n')
coverage=[]
for receipt in RECEIPTS:
 for record in receipt['records']:
  if not str(record.get('name','')).startswith('java.'): continue
  value=record.get('value',{})
  if not isinstance(value,dict): value={}
  error=record.get('error')
  if error: error=error.replace(str(OUT),'<fixtures>').replace(chr(0x2014),'-')
  coverage.append({'tool':record['name'],'case':receipt['case'],'error':error,
   'blocked':value.get('blocked'), 'change_count':len(value.get('changes',[])),
   'create_count':len(value.get('creates',[])), 'provenance':value.get('provenance'),
   'applied':[r['value'].get('applied') for r in receipt['records'] if r.get('name')=='edits.apply'],
   'javac':receipt['compiled'].get('returncode'),
   'baseline_stdout':receipt['baseline'].get('run',{}).get('stdout'),
   'final_stdout':receipt['compiled'].get('run',{}).get('stdout')})
(OUT/'coverage.json').write_text(json.dumps(coverage,indent=2)+'\n')
print('ARTIFACTS '+str(OUT))
if ARGS.verify:
 failures=[]
 expected_refusals={'moveclass','movepackage','unusedctor-callers','extractblock-control','inline-state','signature-overload','movemethod-copy'}
 for receipt in RECEIPTS:
  name=receipt['case']; records=receipt['records']; compiled=receipt['compiled']
  if receipt['invocation'].get('timeout') or not records: failures.append(name+': missing/bounded invocation result')
  if compiled.get('returncode')!=0: failures.append(name+': javac failed: '+compiled.get('stderr',''))
  expected_refusal=name in expected_refusals
  errors=[r for r in records if r.get('fatal') or r.get('error')]
  blocked=[r for r in records if isinstance(r.get('value'),dict) and r['value'].get('blocked')]
  if expected_refusal:
   if not errors and not blocked: failures.append(name+': expected an explicit refusal')
   if receipt['before']!={k:v for k,v in receipt['after'].items() if k in receipt['before']}: failures.append(name+': refusal changed original fixture')
  else:
   if any(r.get('fatal') for r in records): failures.append(name+': unrecovered tool error')
   if any(r.get('name')=='edits.apply' and not r['value'].get('applied') for r in records): failures.append(name+': edit proposal refused at apply')
  baseline_run=receipt['baseline'].get('run',{}); final_run=compiled.get('run',{})
  if final_run and final_run.get('returncode')!=0: failures.append(name+': Java execution failed')
  if name=='wrappers' and final_run.get('stdout')!='5\n': failures.append(name+': generated wrapper behavior incorrect')
  if baseline_run.get('returncode')==0 and final_run.get('stdout')!=baseline_run.get('stdout'): failures.append(name+': observable Java output changed')
  if name=='inject-existing':
   text=(OUT/name/'Service.java').read_text()
   if not re.search(r'@Inject\s+public Service\(',text): failures.append(name+': promoted constructor lacks injection annotation')
  if name in {'interface','pullup','annotated-interface'}:
   target=(OUT/name/'CalcApi.java').read_text()
   if not re.search(r'int\s+add\s*\(',target): failures.append(name+': target omits selected method contract')
  if name in {'moveclass-project','movepackage-project'}:
   if (OUT/name/'src/fixture/Order.java').exists() or not (OUT/name/'src/relocated/Order.java').exists(): failures.append(name+': relocation paths wrong')
 if not ARGS.case:
  observed={r.get('name') for receipt in RECEIPTS for r in receipt['records']}
  missing={n for n in INVENTORY if n.startswith('java.')}-observed
  if missing: failures.append('unexercised Java bindings: '+','.join(sorted(missing)))
 if 'java.extractColumnSpec' in INVENTORY: failures.append('broken extractColumnSpec is still callable')
 (OUT/'verification.json').write_text(json.dumps({'passed':not failures,'failures':failures},indent=2)+'\n')
 if failures: raise SystemExit('\n'.join(failures))
 print('VERIFIED '+str(len(RECEIPTS))+' Java scenarios')
