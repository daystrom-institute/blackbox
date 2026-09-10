#!/usr/bin/env python3
"""Executable regressions from the hands-on isolate audit; no model benchmark."""
import argparse
import json
import re
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--isolate", default="isolate")
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
source = "crates/bro-harness/src/bin/isolate.rs"


def invoke(*argv, ok=True):
    result = subprocess.run([args.isolate, "--root", str(root), *argv],
                            capture_output=True, text=True, timeout=30)
    assert (result.returncode == 0) == ok, result.stdout + result.stderr
    return result.stdout if ok else result.stderr


for cell in [False, True]:
    input_args = {"file_path": source, "start_line": 120}
    call = (["--cell", f"text(await tools.file_read({json.dumps(input_args)}));"] if cell
            else ["file_read", "--args", json.dumps(input_args)])
    result = invoke("--tool-defaults", json.dumps({"default:file_read.max_lines": 3}), *call)
    expected = "\n".join((root / source).read_text().splitlines()[119:122])
    assert expected in result and "continue SAME file_path with start_line=123" in result, result
    result = invoke("--tool-defaults", json.dumps({"default:file_read.max_lines": "3"}), *call, ok=False)
    assert "nothing executed" in result, result
    print(f"PASS {'cell' if cell else 'direct'} typed defaults and invalid-policy refusal")

result = invoke("--cell", 'text(ALL_TOOLS.find(t=>t.name==="content_search").declaration);')
assert 'mode?: ("content" | "files" | "count");' in result, result
result = invoke("--cell", 'text(ALL_TOOLS.find(t=>t.name==="shell_poll").declaration);')
assert "output_filter?: unknown" not in result and "stdout?: (string | Array<string>)" in result, result
print("PASS declarations resolve actual built-in schema references")

result = invoke("content_search", "--args", json.dumps({"pattern":"tool.defaults|tool_defaults", "path":source, "context_lines":2}))
coordinates = re.findall(r"^" + re.escape(source) + r":(\d+)[:-]", result, re.MULTILINE)
assert coordinates and len(coordinates) == len(set(coordinates)), result
print("PASS overlapping search context emits each source line once")

result = invoke("--cell", """
let page = await tools.shell_run({command:"git ls-files crates/bro-harness/src",max_output_tokens:100,yield_time_ms:1000});
let output = page.stdout;
let pages = 1;
while (page.running || page.output_pending) {
    if (pages++ > 100) throw new Error("output paging did not finish");
    page = await tools.shell_poll({session_id:page.session_id,max_output_tokens:100,yield_time_ms:1000});
    output += page.stdout;
}
text({output,pages,exit_code:page.exit_code});
""")
page = json.loads(result.split("Output:\n", 1)[1])
expected = subprocess.check_output(["git", "ls-files", "crates/bro-harness/src"], cwd=root, text=True)
assert page["output"] == expected and page["pages"] > 1 and page["exit_code"] == 0, page
print("PASS shell output drains exactly after exit")
