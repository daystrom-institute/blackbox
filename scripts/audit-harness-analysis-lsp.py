#!/usr/bin/env python3
"""Exercise every analysis.* and lsp.* binding through a real isolate process.

Uses disposable source roots, no daemon or model. Language servers are spawned
by isolate and shut down with that session. Receipts include assertions, server
results and disk postconditions. Run again against a rebuilt --isolate binary.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

FIXTURES = Path(__file__).resolve().parent / 'fixtures' / 'harness-analysis-lsp'
TOOLS = ['analysis.' + name for name in ('describe', 'cohesionClusters', 'references', 'fieldClassification', 'methodRegions', 'fieldInitializerClosure', 'implPartition', 'topLevelDeps')] + ['lsp.' + name for name in ('status', 'hover', 'definition', 'references', 'rename', 'assist', 'willRenameFiles', 'executeCommand')]
JAVA = 'src/main/java/com/acme/OrderLedger.java'
CONSTANTS = 'src/main/java/com/acme/Constants.java'


def span(root, file, needle, width=None):
    data = (root / file).read_bytes()
    needle = needle.encode()
    start = data.index(needle)
    return dict(file=file, byte_start=start, byte_end=start + (len(needle) if width is None else width), content_sha256=hashlib.sha256(data).hexdigest())


def compact_result(value):
    encoded = json.dumps(value)
    if len(encoded.encode()) <= 32768:
        return value
    return dict(result_body_omitted=True, serialized_bytes=len(encoded.encode()),
                keys=list(value) if isinstance(value, dict) else None,
                statement_region_summary=value.get('statement_region_summary') if isinstance(value, dict) else None)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--isolate', default=shutil.which('isolate'))
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--skip-lsp', action='store_true', help='Explicit syntax-only subset, not full coverage')
    args = parser.parse_args()
    binary = Path(args.isolate).resolve()
    rows = []
    started = time.time()
    with tempfile.TemporaryDirectory(prefix='harness-analysis-lsp-runtime-') as temporary:
        home = Path(temporary)
        def fixture(name):
            root = home / name
            shutil.copytree(FIXTURES, root)
            return root.resolve()

        root = fixture('analysis')
        def invoke(name, tool, inputs, check, cwd=None, env=None, timeout=90):
            try:
                proc = subprocess.run([str(binary), '--root', str(root), tool, '--args', json.dumps(inputs)], capture_output=True, text=True, timeout=timeout, cwd=cwd or home, env=env)
                try:
                    result = json.loads(proc.stdout)
                except json.JSONDecodeError:
                    result = None
                try:
                    passed = bool(check(result, proc))
                except (KeyError, IndexError, TypeError, AttributeError):
                    passed = False
                rows.append(dict(case=name, tool=tool, passed=passed, exit_code=proc.returncode, result=compact_result(result), stderr=proc.stderr))
            except subprocess.TimeoutExpired as error:
                rows.append(dict(case=name, tool=tool, passed=False, error='process deadline exceeded', deadline=error.timeout))
            print(name, 'PASS' if rows[-1]['passed'] else 'FAIL', flush=True)

        def success(predicate):
            return lambda result, proc: proc.returncode == 0 and result is not None and predicate(result)
        def refusal(fragment):
            return lambda result, proc: proc.returncode != 0 and fragment in proc.stderr and 'panicked at' not in proc.stderr

        for language in ('java', 'rust'):
            invoke('describe-references-' + language, 'analysis.describe', dict(analysis='references', language=language), success(lambda value, language=language: language.lower() in value['contract'].lower()))
        invoke('describe-unknown', 'analysis.describe', dict(analysis='absent'), refusal('unknown analysis'))
        invoke('cohesion-clusters', 'analysis.cohesionClusters', dict(file=JAVA), success(lambda value: value['cluster_count'] >= 2 and any(edge['from_method'] == 'summary' and edge['to_method'] == 'balance' for edge in value['cross_cluster_calls'])))
        invoke('cohesion-missing-file', 'analysis.cohesionClusters', dict(file='absent.java'), refusal('failed to read'))
        invoke('java-reference-counts', 'analysis.references', dict(symbols=['deposit', 'balance'], kinds=['method_invocation']), success(lambda value: value['counts_by_symbol'] == dict(deposit=1, balance=2) and value['total_usages'] == 3))
        invoke('rust-reference-counts', 'analysis.references', dict(symbols=['normalize'], language='rust'), success(lambda value: value['counts_by_symbol']['normalize'] == 5 and set(value['files_by_symbol']['normalize']) == {'src/lib.rs', 'src/receipt.rs'} and 'including declarations' in value['counting_scope']))
        invoke('references-empty', 'analysis.references', dict(symbols=[]), refusal('non-empty'))
        invoke('field-read-write-classification', 'analysis.fieldClassification', dict(file=JAVA, fields=['pending']), success(lambda value: value['fields'][0]['name'] == 'pending' and value['fields'][0]['writes'] >= 2 and value['fields'][0]['reads'] >= 2))
        invoke('field-unknown', 'analysis.fieldClassification', dict(file=JAVA, fields=['absent']), refusal('fields not found'))
        invoke('constant-transitive-closure', 'analysis.fieldInitializerClosure', dict(file=CONSTANTS, fields=['TOTAL']), success(lambda value: value['closure']['TOTAL'] == ['BASE', 'TAX']))
        (root/'Multi.java').write_text('class Multi { static final int BASE=1; static final int FIRST=BASE+1, SECOND=FIRST+2; }')
        invoke('constant-multiple-declarators', 'analysis.fieldInitializerClosure', dict(file='Multi.java', fields=['FIRST','SECOND']), success(lambda value: value['closure'] == {'FIRST':['BASE'], 'SECOND':['BASE','FIRST']}))
        (root/'Owners.java').write_text('class Second { static final int OTHER=2; static final int BASE=OTHER; } class First { static final int BASE=1; static final int VALUE=BASE; }')
        invoke('constant-owner-isolation', 'analysis.fieldInitializerClosure', dict(file='Owners.java', fields=['VALUE']), success(lambda value: value['closure']['VALUE'] == ['BASE']))
        invoke('constant-owner-ambiguity', 'analysis.fieldInitializerClosure', dict(file='Owners.java', fields=['BASE']), refusal('className'))
        invoke('constant-unknown', 'analysis.fieldInitializerClosure', dict(file=CONSTANTS, fields=['ABSENT']), refusal('fields not found'))
        invoke('constant-nonstatic-disclosed', 'analysis.fieldInitializerClosure', dict(file=CONSTANTS, fields=['count']), success(lambda value: value['skipped_non_constants'] == ['count']))
        invoke('method-live-outs-and-cap', 'analysis.methodRegions', dict(file=JAVA, method='summary', statementLimit=2), success(lambda value: value['statement_region_summary']['omitted_count'] == 3 and value['statement_regions'][0]['live_outs'][0]['name'] == 'total'))
        invoke('method-absent', 'analysis.methodRegions', dict(file=JAVA, method='absent'), refusal('not found'))
        invoke('impl-partition-multiple-blocks', 'analysis.implPartition', dict(file='src/lib.rs', implName='Ledger'), success(lambda value: {'deposit', 'balance', 'receipt'} <= {method['name'] for method in value['methods']} and any(edge['from'] == 'method:deposit' and edge['to'] == 'field:pending' and edge['kind'] == 'writes' for edge in value['edges'])))
        invoke('impl-absent', 'analysis.implPartition', dict(file='src/lib.rs', implName='Absent'), refusal('no impl block'))
        invoke('top-level-relative-root', 'analysis.topLevelDeps', dict(file='src/lib.rs', projectDir='src'), success(lambda value: any(ref['item'] == 'normalize' and ref['path'] == 'src/receipt.rs' for ref in value['external_references'])))
        invoke('top-level-unknown-project', 'analysis.topLevelDeps', dict(file='src/lib.rs', projectDir='missing-directory'), refusal('directory'))
        (root / 'Large.java').write_text('class Large { void render() {\n' + ''.join(f'int value{i} = {i};\n' for i in range(3000)) + '} }\n')
        invoke('reduction-overflow-refusal', 'analysis.methodRegions', dict(file='Large.java', method='render'), refusal('byte limit'))
        invoke('reduction-overflow-recovery', 'analysis.methodRegions', dict(file='Large.java', method='render', statementLimit=1), success(lambda value: value['statement_region_summary']['omitted_count'] == 2999))
        (root / 'Large.java').unlink()

        if not args.skip_lsp:
            def cell(name, fixture_name, source, assertions):
                nonlocal root
                root = fixture(fixture_name)
                body = source(root)
                try:
                    proc = subprocess.run([str(binary), '--root', str(root), '--cell-timeout', '180', '--cell', body], capture_output=True, text=True, timeout=210)
                    events = [json.loads(line) for line in proc.stdout.splitlines() if line.startswith('{')]
                    index = {event['name']: event for event in events}
                    for label, tool, check in assertions:
                        try:
                            passed = proc.returncode == 0 and check(index, root)
                        except (KeyError, IndexError, TypeError, AttributeError):
                            passed = False
                        rows.append(dict(case=name + '-' + label, tool=tool, passed=bool(passed), exit_code=proc.returncode, events=events, stderr=proc.stderr))
                        print(rows[-1]['case'], 'PASS' if passed else 'FAIL', flush=True)
                except subprocess.TimeoutExpired:
                    for label, tool, check in assertions:
                        rows.append(dict(case=name + '-' + label, tool=tool, passed=False, error='cell process deadline exceeded'))

            for language in ('rust', 'java'):
                def suite(root, language=language):
                    file, needle, width = ('src/lib.rs', 'normalize(amount)', 9) if language == 'rust' else (JAVA, 'balance()', 7)
                    point = span(root, file, needle, width)
                    old, new = ('src/receipt.rs', 'src/invoice.rs') if language == 'rust' else (JAVA, JAVA.replace('OrderLedger', 'AccountLedger'))
                    calls = [('status.before', 'status', dict(language=language)), ('hover', 'hover', dict(span=point, wait_ready_ms=30000)), ('definition', 'definition', dict(span=point, wait_ready_ms=30000)), ('references', 'references', dict(span=point, wait_ready_ms=30000)), ('references.cap', 'references', dict(span=point, limit=1, wait_ready_ms=30000)), ('assist', 'assist', dict(span=point, wait_ready_ms=30000)), ('executeCommand', 'executeCommand', dict(language=language, command='audit.unknown.command' if language == 'rust' else 'java.project.getAll')), ('willRenameFiles', 'willRenameFiles', dict(language=language, renames=[dict(oldFile=old, newFile=new)]))]
                    return 'const calls=' + json.dumps(calls) + ';for(const [name,method,args] of calls){try{text({name,result:await lsp[method](args)})}catch(e){text({name,error:String(e)})}}\n' + 'try{const result=await lsp.rename(' + json.dumps(dict(span=point, newName='renamedAmount', wait_ready_ms=30000)) + ');text({name:"rename",result});const es=await edits.begin();await edits.merge({es,changes:result.changes});text({name:"rename.apply",result:await edits.apply({es})});try{await lsp.hover(' + json.dumps(dict(span=point)) + ');text({name:"stale",error:"accepted stale span"})}catch(e){text({name:"stale",error:String(e)})}}catch(e){text({name:"rename",error:String(e)})};text({name:"status.after",result:await lsp.status(' + json.dumps(dict(language=language)) + ')});'
                def renamed_disk(index, root, language=language):
                    a, b = ('src/lib.rs', 'src/receipt.rs') if language == 'rust' else (JAVA, 'src/main/java/com/acme/ReceiptPrinter.java')
                    return index['rename.apply']['result']['applied'] and index['rename.apply']['result']['semantic_status'] == 'lsp_verified' and all('renamedAmount' in (root / path).read_text() for path in (a, b))
                cell(language, language, suite, [
                    ('status-cold-ready', 'lsp.status', lambda i, r: i['status.before']['result']['state'] == 'not_started' and i['status.after']['result']['state'] == 'ready'),
                    ('hover', 'lsp.hover', lambda i, r: bool(i['hover']['result']['contents'])),
                    ('definition-anchored', 'lsp.definition', lambda i, r: i['definition']['result']['locations'][0]['anchored']),
                    ('references-cross-file', 'lsp.references', lambda i, r: len({loc['span']['file'] for loc in i['references']['result']['locations']}) == 2),
                    ('references-cap', 'lsp.references', lambda i, r: i['references.cap']['result']['truncated'] and i['references.cap']['result']['returned_count'] == 1),
                    ('assist-menu', 'lsp.assist', lambda i, r: bool(i['assist']['result']['actions'])),
                    ('executeCommand', 'lsp.executeCommand', (lambda i, r: 'unknown request' in i['executeCommand']['error']) if language == 'rust' else (lambda i, r: bool(i['executeCommand']['result']['result']))),
                    ('file-rename-edits', 'lsp.willRenameFiles', lambda i, r: i['willRenameFiles']['result']['edit_count'] >= 1),
                    ('rename-disk-lineage', 'lsp.rename', renamed_disk),
                    ('rename-old-span-refused', 'lsp.hover', lambda i, r: 'stale_span' in i['stale']['error']),
                ])

            def assist_source(root):
                point = span(root, 'src/lib.rs', 'if flag { true } else { false }')
                return 'const span=' + json.dumps(point) + ';const menu=await lsp.assist({span,wait_ready_ms:30000});text({name:"menu",result:menu});const action=menu.actions.find(a=>a.title==="Extract into function");try{const selected=await lsp.assist({span,select:action.index,limit:1,wait_ready_ms:30000});text({name:"selected",result:selected});const es=await edits.begin();await edits.merge({es,changes:selected.changes});text({name:"applied",result:await edits.apply({es})})}catch(e){text({name:"selected",error:String(e)})}'
            cell('rust-assist', 'rust-assist', assist_source, [('original-index-beyond-menu-cap-applies', 'lsp.assist', lambda i, r: i['applied']['result']['applied'] and i['applied']['result']['semantic_status'] == 'lsp_verified' and (r/'src/lib.rs').read_text() != (FIXTURES/'src/lib.rs').read_text())])

            def java_assist_source(root):
                point = span(root, JAVA, 'summary(int tax)', 7)
                return 'const span=' + json.dumps(point) + ';const menu=await lsp.assist({span,wait_ready_ms:30000});text({name:"menu",result:menu});const action=menu.actions.find(a=>a.title==="Change modifiers to final where possible");try{const selected=await lsp.assist({span,select:action.index,wait_ready_ms:30000});text({name:"selected",result:selected});const es=await edits.begin();await edits.merge({es,changes:selected.changes});text({name:"applied",result:await edits.apply({es})})}catch(e){text({name:"selected",error:String(e)})}'
            cell('java-assist', 'java-assist', java_assist_source, [('resolved-action-applies', 'lsp.assist', lambda i, r: i['applied']['result']['applied'] and i['applied']['result']['semantic_status'] == 'lsp_verified' and 'final int tax' in (r/JAVA).read_text())])

            for language in ('rust', 'java'):
                def move_source(root, language=language):
                    old, new = ('src/receipt.rs', 'src/invoice.rs') if language == 'rust' else (JAVA, JAVA.replace('OrderLedger', 'AccountLedger'))
                    return 'const old=' + json.dumps(old) + ',target=' + json.dumps(new) + ';const result=await lsp.willRenameFiles(' + json.dumps(dict(language=language,renames=[dict(oldFile=old,newFile=new)])) + ');text({name:"move.plan",result});const first=await edits.begin();await edits.merge({es:first,changes:result.changes});text({name:"move.references",result:await edits.apply({es:first})});const facts=await code.items({file:old});const source=await code.read({span:{file:old,byte_start:0,byte_end:facts.source_len,content_sha256:facts.content_sha256}});const second=await edits.begin();await edits.createFile({es:second,path:target,content:source.text});await edits.deleteFile({es:second,path:old,contentSha256:facts.content_sha256});text({name:"move.files",result:await edits.apply({es:second})});'
                def moved(index, root, language=language):
                    old, new = ('src/receipt.rs', 'src/invoice.rs') if language == 'rust' else (JAVA, JAVA.replace('OrderLedger', 'AccountLedger'))
                    caller = 'src/lib.rs' if language == 'rust' else 'src/main/java/com/acme/ReceiptPrinter.java'
                    expected = 'pub mod invoice;' if language == 'rust' else 'AccountLedger'
                    return index['move.references']['result']['applied'] and index['move.references']['result']['semantic_status'] == 'lsp_verified' and index['move.files']['result']['applied'] and not (root/old).exists() and (root/new).is_file() and expected in (root/caller).read_text()
                cell(language + '-move', language + '-move', move_source, [('references-and-file-move-applied', 'lsp.willRenameFiles', moved)])

            root = fixture('lsp-errors')
            (root / 'unicode.rs').write_text('pub fn label() { let text = "😀"; }\n')
            point = span(root, 'unicode.rs', '😀', 0)
            point['byte_start'] += 1
            point['byte_end'] += 1
            for tool in ('lsp.hover', 'lsp.rename', 'lsp.definition', 'lsp.references', 'lsp.assist'):
                inputs = dict(span=point, wait_ready_ms=0)
                if tool == 'lsp.rename':
                    inputs['newName'] = 'renamed'
                invoke(tool + '-split-utf8', tool, inputs, refusal('invalid_span'))
            (root/'invalid.rs').write_bytes(b'pub fn sample() { let text = "\xff"; }')
            bad = dict(file='invalid.rs', byte_start=0, byte_end=3, content_sha256=hashlib.sha256((root/'invalid.rs').read_bytes()).hexdigest())
            invoke('lsp-invalid-source', 'lsp.hover', dict(span=bad, wait_ready_ms=0), refusal('invalid_utf8'))
            invoke('lsp-status-language-conflict', 'lsp.status', dict(file='src/lib.rs', language='java'), refusal('infers'))
            invoke('lsp-file-renames-empty', 'lsp.willRenameFiles', dict(renames=[]), refusal('must not be empty'))
            point = span(root, 'src/lib.rs', 'normalize(amount)', 9)
            for language, file, needle in [('rust','src/lib.rs','normalize'),('java',JAVA,'balance')]:
                controlled = os.environ.copy()
                controlled['BRO_LSP_RUST_ANALYZER_BIN' if language == 'rust' else 'BRO_LSP_JDTLS_BIN'] = str(home/'missing-server')
                invoke('explicit-unavailable-' + language, 'lsp.hover', dict(span=span(root,file,needle),wait_ready_ms=0), refusal('lsp_unavailable'), env=controlled)

        covered = sorted({row['tool'] for row in rows})
        report = dict(binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), elapsed_seconds=round(time.time()-started,2), fixture='scripts/fixtures/harness-analysis-lsp', full_surface=not args.skip_lsp, covered_tools=covered, missing_tools=sorted(set(TOOLS)-set(covered)), passed=sum(row['passed'] for row in rows), failed=sum(not row['passed'] for row in rows), cases=rows)
        # Runtime roots and server links are disposable evidence, not machine identity.
        serialized = json.dumps(report, indent=2).replace(str(home), '<fixture-root>')
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(serialized + '\n')
        print(json.dumps({key:report[key] for key in ('passed','failed','covered_tools','missing_tools')}))
        return int(bool(report['failed']))

if __name__ == '__main__':
    raise SystemExit(main())
