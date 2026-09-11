#!/usr/bin/env python3
"""Exercise all installed rust.* bindings, their edits recipes, and rustc outcomes.

No daemon, model, external dependency download, or operator-authority grant.
Every fixture and HOME/XDG state directory belongs to this invocation. Compiler
checks use a directly installed rustc, not this repository's Cargo workspace.
Use --isolate to compare a rebuilt binary; --only selects named cases.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

TOOLS = ["describe", "extractImplMethods", "extractItems", "extractTrait", "fixRound",
         "inlineModToFile", "liftToFree", "migrateErrorType", "migrateTypeUsages",
         "moduleWiring", "moveStructFields", "organizeImports", "rewriteModuleCallers",
         "setVisibility", "updateCallers"]
PRELUDE = """
async function apply(r) {
  const es = await edits.begin();
  await edits.merge({es, changes:r.changes || []});
  for (const c of r.creates || []) await edits.createFile({es,path:c.path,content:c.content});
  const result = await edits.apply({es});
  text({applied:result});
  if (!result.applied) throw Error('proposal did not apply: '+JSON.stringify(result));
  return result;
}
async function propose(name, args) {
  const r = await rust[name](args); text({tool:name,proposal:r}); await apply(r); return r;
}
async function refuse(name, args, fragment) {
  try { await rust[name](args); } catch (error) {
    const message=String(error); text({tool:name,refusal:message});
    if (!message.includes(fragment)) throw Error('unexpected refusal: '+message);
    return;
  }
  throw Error('expected refusal: '+name);
}
"""


def invoke(name, args):
    return "await propose(" + json.dumps(name) + "," + json.dumps(args) + ");\n"


def test(body):
    return "\n#[test] fn runtime_contract() { " + body + " }\n"


def cases(rustc):
    yield "describe", {"src/lib.rs":""}, (
        "for(const name of " + json.dumps(TOOLS[1:]) + ") {"
        "const r=await rust.describe({transform:name});"
        "if(!r.contract.includes('rust.'+name)) throw Error('wrong contract '+name);"
        "text({tool:'describe',transform:name,contract_bytes:r.contract.length});}"
        "await refuse('describe',{transform:'missingTransform'},'unknown transform');"), None
    yield "extract_items", {
        "src/lib.rs":"mod math; pub fn run()->u32 { math::score(3) }" + test("assert_eq!(run(),4);"),
        "src/math.rs":"pub fn score(n:u32)->u32 { n+1 }\npub fn other()->u32 { score(2) }\n",
    }, invoke("extractItems", dict(source="src/math.rs", target="src/math/scoring.rs", itemNames=["score"])) + (
        "await refuse('extractItems',{source:'src/math.rs',target:'src/math/scoring.rs',itemNames:['score']},'rust.extractItems');"), None
    yield "extract_visibility", {
        "src/lib.rs":"mod math;" + test("assert_eq!(math::score(),7); assert_eq!(math::internal(),3); assert_eq!(math::State { value:9 }.value,9);"),
        "src/math.rs":"pub fn score()->u32 { 7 }\npub(crate) fn internal()->u32 { 3 }\npub(super) struct State { pub value:u32 }\n",
    }, invoke("extractItems", dict(source="src/math.rs", target="src/math/scoring.rs", itemNames=["score", "internal", "State"])), None
    yield "inline_mod", {
        "src/lib.rs":"mod service;" + test("assert_eq!(service::nested::value(),7);"),
        "src/service.rs":"pub mod nested { pub fn value()->u32 { 7 } }\n",
    }, invoke("inlineModToFile", dict(source="src/service.rs", moduleName="nested")), None
    yield "wiring", {
        "src/lib.rs":"pub fn run()->u32 { widget::value() }" + test("assert_eq!(run(),7);"),
        "src/widget.rs":"pub fn value()->u32 { 7 }\n",
    }, invoke("moduleWiring", dict(source="src/lib.rs", action="add_mod", moduleName="widget")), None
    yield "visibility", {
        "src/lib.rs":"mod widget;" + test("assert_eq!(widget::value(),7);"),
        "src/widget.rs":"const fn value()->u32 { 7 }\n",
    }, invoke("setVisibility", dict(source="src/widget.rs", visibility="pub(crate)", itemNames=["value"])), None
    for existing in [False, True]:
        files = {
            "src/lib.rs":"pub const OFFSET:u32=3; mod widget;" + test("assert_eq!(widget::Widget.score(),3);"),
            "src/widget.rs":"mod methods;\npub struct Widget;\nimpl Widget {\n    pub fn score(&self)->u32 { super::OFFSET }\n}\n",
        }
        if existing:
            files["src/widget/methods.rs"] = ""
        yield "impl_existing" if existing else "impl_create", files, invoke("extractImplMethods", dict(
            source="src/widget.rs", target="src/widget/methods.rs", item_names=["score"],
            impl_name="Widget", target_prelude="use super::Widget;")), None
    yield "imports", {
        "src/lib.rs":"mod values; mod worker;" + test("assert_eq!(worker::value(),3);"),
        "src/values.rs":"pub struct Used; pub struct Unused;\n",
        "src/worker.rs":"use crate::values::*; pub fn value()->u32 { let _ = Used; 3 }\n",
    }, invoke("organizeImports", dict(source="src/worker.rs")), None
    yield "compact_fields", {
        "src/lib.rs":"mod state; mod owner;" + test('let x=state::State {label:String::new(),count:7}; assert_eq!(x.count,7); let _=owner::Owner {name:String::new()};'),
        "src/state.rs":"pub struct State { pub label:String }\n",
        "src/owner.rs":"pub struct Owner { pub count:u32, pub name:String }\n",
    }, invoke("moveStructFields", dict(source="src/owner.rs", target="src/state.rs", structName="Owner", itemNames=["count"])), None
    yield "field_attributes", {
        "src/lib.rs":"mod owner; mod state;" + test('let _=owner::Owner {name:String::new()}; let _=state::State {label:String::new()};'),
        "src/owner.rs":"pub struct Owner {\n    #[cfg(any())]\n    pub count:u32,\n    pub name:String,\n}\n",
        "src/state.rs":"pub struct State {\n    pub label:String,\n}\n",
    }, invoke("moveStructFields", dict(source="src/owner.rs", target="src/state.rs", structName="Owner", itemNames=["count"])), None
    yield "update_callers", {
        "src/lib.rs":"mod state; mod owner;" + test('let x=owner::Owner {state:state::State {count:7}}; assert_eq!(x.count(),7);'),
        "src/state.rs":"pub struct State { pub count:u32 }\n",
        "src/owner.rs":"pub struct Owner { pub state:crate::state::State } impl Owner { pub fn count(&self)->u32 { self.count } }\n",
    }, invoke("updateCallers", dict(source="src/owner.rs", structName="Owner", target="src/state.rs", delegateType="State", delegateField="state", itemNames=["count"])), None
    yield "trait", {
        "src/lib.rs":"mod store; mod store_api; pub fn run()->u32 { store::Store.value() }" + test("assert_eq!(run(),4);"),
        "src/store.rs":"pub struct Store;\nimpl Store {\n    pub fn value(&self)->u32 { 4 }\n}\n",
    }, invoke("extractTrait", dict(source="src/store.rs", target="src/store_api.rs", implName="impl Store", traitName="StoreApi", itemNames=["value"])), (
        invoke("moduleWiring", dict(source="src/lib.rs", action="add_use", usePath="crate::store_api::StoreApi")))
    yield "async_trait_report", {
        "src/lib.rs":"mod store; mod api; fn requires_dyn(_: &dyn api::Api) {}\n",
        "src/store.rs":"pub struct Store;\nimpl Store {\n    pub async fn value(&self)->u32 { 4 }\n}\n",
    }, (
        "const r=await rust.extractTrait({source:'src/store.rs',target:'src/api.rs',implName:'impl Store',traitName:'Api',itemNames:['value']});"
        "text({tool:'extractTrait',proposal:r}); if(r.dyn_compatible!==false || r.object_safety_report.dyn_compatible!==false) throw Error('false dyn compatibility claim'); await apply(r);"
    ), None
    yield "sized_trait_report", {
        "src/lib.rs":"mod store; mod api; fn accepts_dyn(_: &dyn api::Api) {}" + test("accepts_dyn(&store::Store);"),
        "src/store.rs":"pub struct Store;\nimpl Store {\n    pub fn consume(self)->Self { self }\n}\n",
    }, (
        "const r=await rust.extractTrait({source:'src/store.rs',target:'src/api.rs',implName:'impl Store',traitName:'Api',itemNames:['consume']});"
        "text({tool:'extractTrait',proposal:r}); if(r.dyn_compatible!==true || r.object_safety_report.dyn_compatible!==true) throw Error('Self:Sized exemption lost'); await apply(r);"
    ), None
    yield "lift", {
        "src/lib.rs":"mod helper; mod free; pub fn run()->u32 { helper::Helper::value(2) }" + test("assert_eq!(run(),3);"),
        "src/helper.rs":"pub struct Helper;\nimpl Helper {\n    pub fn value(n:u32)->u32 { n+1 }\n}\n",
    }, invoke("liftToFree", dict(source="src/helper.rs", target="src/free.rs", itemNames=["value"])), (
        "const facts=await code.items({file:'src/lib.rs'}); const span=facts.items.find(x=>x.name==='run').span;"
        "const es=await edits.begin(); await edits.replace({es,span,text:'pub fn run()->u32 { free::value(2) }'});"
        "const result=await edits.apply({es}); text({recovery:result}); if(!result.applied) throw Error('recovery failed');")
    yield "error_type", {
        "src/lib.rs":"mod work;" + test('assert!(work::run());'),
        "src/work.rs":'enum Old { Bad } enum New { Bad } fn work()->Result<(),Old> { let _message="Old::Bad"; Err(Old::Bad) }\npub fn run()->bool { work().is_err() }\n',
    }, invoke("migrateErrorType", dict(source="src/work.rs", oldText="Old", newText="New", itemNames=["work"], errorMapping={"Bad":"Bad"})), None
    yield "module_callers", {
        "src/lib.rs":"mod old; mod new; mod caller;" + test('assert_eq!(caller::run(),4); assert_eq!(caller::EXPECTED,"old::moved");'),
        "src/old.rs":"pub fn moved()->u32 { 1 }\n",
        "src/new.rs":"pub fn moved()->u32 { 2 }\n",
        "src/caller.rs":'use crate::old::moved; pub fn run()->u32 { crate::old::moved()+moved() }\npub const EXPECTED:&str="old::moved"; // old::moved is prose\n',
    }, invoke("rewriteModuleCallers", dict(project_dir=".", module_name="old", target_prelude="new", item_names=["moved"], skip_files=["src/old.rs", "src/new.rs"])), None
    yield "authority_refusals", {
        "src/lib.rs":"#[repr(C)] struct Owner { count:u32 } struct State {}\npub enum Old { Bad } pub enum New { Bad } pub fn work()->Result<(),Old> { Err(Old::Bad) }\n",
    }, (
        "await refuse('moveStructFields',{source:'src/lib.rs',target:'src/lib.rs',structName:'Owner',itemNames:['count']},'repr_unacknowledged');"
        "await refuse('migrateErrorType',{source:'src/lib.rs',oldText:'Old',newText:'New',itemNames:['work']},'public_api_change_unacknowledged');"
        "await refuse('migrateTypeUsages',{source:'src/lib.rs',moduleName:'Old',replacementKind:'bareConcrete',newText:'New'},'public_api_change_unacknowledged');"
        "await refuse('organizeImports',{source:'src/lib.rs',mode:'organize'},'lsp.assist');"), None
    yield "fixround", {"src/lib.rs":"pub fn value()->u32 { let mut x=1; x+2 }" + test("assert_eq!(value(),3);")}, (
        "const gate=await build.gate({command:" + json.dumps(str(rustc) + " --edition=2021 --crate-type=lib --error-format=json src/lib.rs -o fixture.rlib") + ",anchor_spans:true});"
        "text({gate}); if(!gate.diagnostics.length) throw Error('compiler diagnostic disappeared');"
        "const r=await rust.fixRound({diagnostics:gate.diagnostics}); text({tool:'fixRound',proposal:r});"
        "if(!r.changes.length) throw Error('machine-applicable diagnostic produced no changes');"
        "const result=await apply(r); if(result.semantic_status!=='compiler_suggested') throw Error('lost compiler lineage');"), None
    yield "fixround_raw", {"src/lib.rs":"pub fn value()->u32 { let mut x=1; x+2 }" + test("assert_eq!(value(),3);")}, (
        "const process=await tools.shell_run({command:" + json.dumps(str(rustc) + " --edition=2021 --crate-type=lib --error-format=json src/lib.rs -o fixture.rlib") + ",yield_time_ms:5000});"
        "const r=await rust.fixRound({raw_json:process.stderr}); text({tool:'fixRound',proposal:r});"
        "if(r.changes.length || !r.findings.some(x=>x.finding==='unanchored_suggestion')) throw Error('raw diagnostics must disclose missing content-hash authority');"), None


def run(args):
    isolate = Path(args.isolate or shutil.which("isolate")).resolve()
    rustc = Path(subprocess.check_output(["rustup", "which", "rustc"], text=True).strip())
    root = Path(args.output_dir or tempfile.mkdtemp(prefix="rust-bindings-runtime-")).resolve()
    root.mkdir(parents=True, exist_ok=True)
    env = {k:v for k,v in os.environ.items() if not k.startswith(("BRO_", "BLACKBOX_", "BBOX_", "ANTHROPIC_", "OPENAI_"))}
    for key, leaf in [("HOME","home"), ("XDG_CONFIG_HOME","config"), ("XDG_STATE_HOME","state"), ("BRO_HOME","bro")]:
        path = root / leaf
        path.mkdir(exist_ok=True)
        env[key] = str(path)
    inventory = subprocess.check_output([str(isolate), "--list"], env=env, text=True)
    actual = sorted(line for line in inventory.splitlines() if line.startswith("rust."))
    assert actual == sorted("rust." + name for name in TOOLS), actual
    receipts = []
    for name, files, cell, recovery in cases(rustc):
        if args.only and name not in args.only.split(","):
            continue
        case_root = root / name
        case_root.mkdir(exist_ok=False)
        for path, body in files.items():
            dest = case_root / path
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(body)
        (case_root / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="0.1.0"\nedition="2021"\n')
        def execute(source, label):
            path = case_root / (label + ".js")
            path.write_text(PRELUDE + source)
            p = subprocess.run([str(isolate), "--root", str(case_root), "--cell-file", str(path)], env=env, capture_output=True, text=True, timeout=60)
            return {"exit":p.returncode, "stdout":p.stdout, "stderr":p.stderr}
        def compile_and_run():
            binary = case_root / "fixture-tests"
            p = subprocess.run([str(rustc), "--edition=2021", "-Awarnings", "--test", "src/lib.rs", "-o", str(binary)], cwd=case_root, env=env, capture_output=True, text=True, timeout=30)
            result = {"compile_exit":p.returncode, "diagnostics":p.stderr}
            if p.returncode == 0:
                p = subprocess.run([str(binary)], cwd=case_root, env=env, capture_output=True, text=True, timeout=10)
                result.update(run_exit=p.returncode, output=p.stdout+p.stderr)
            return result
        receipt = {"case":name, "invocation":execute(cell, "probe")}
        receipt["before_recovery"] = compile_and_run()
        if recovery:
            receipt["recovery"] = execute(recovery, "recovery")
            receipt["after_recovery"] = compile_and_run()
        final = receipt.get("after_recovery", receipt["before_recovery"])
        passed = receipt["invocation"]["exit"] == 0 and final.get("run_exit") == 0
        if recovery:
            passed = passed and receipt["recovery"]["exit"] == 0 and receipt["before_recovery"]["compile_exit"] != 0
        after = {str(p.relative_to(case_root)):p.read_text() for p in case_root.rglob("*.rs")}
        if name == "async_trait_report":
            passed = receipt["invocation"]["exit"] == 0 and final["compile_exit"] != 0 and "E0038" in final["diagnostics"]
        if name == "error_type":
            passed = passed and '"Old::Bad"' in after["src/work.rs"] and "Err(New::Bad)" in after["src/work.rs"]
        if name == "module_callers":
            passed = passed and "// old::moved is prose" in after["src/caller.rs"]
        if name == "authority_refusals":
            passed = passed and after == files
        receipt.update(passed=passed, after=after)
        receipts.append(receipt)
        report = {"isolate_sha256":hashlib.sha256(isolate.read_bytes()).hexdigest(), "inventory":actual, "receipts":receipts}
        # Normalize fixture and toolchain locations before durable evidence export.
        serialized = json.dumps(report, indent=2).replace(str(root), "$FIXTURES").replace(str(rustc.parent.parent), "$RUST_TOOLCHAIN")
        (root / "receipts.json").write_text(serialized + "\n")
        print(("PASS" if passed else "FAIL") + " " + name, flush=True)
    print("Receipts: " + str(root / "receipts.json"))
    return 0 if all(r["passed"] for r in receipts) else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--isolate")
    parser.add_argument("--output-dir")
    parser.add_argument("--only", help="comma-separated case names")
    raise SystemExit(run(parser.parse_args()))
