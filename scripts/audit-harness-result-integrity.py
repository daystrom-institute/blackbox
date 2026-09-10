#!/usr/bin/env python3
"""Exercise provider and terminal integrity using an isolated local Chat fixture."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--harness", required=True)
parser.add_argument("--check", action="store_true")
args = parser.parse_args()
root = Path(tempfile.mkdtemp(prefix="harness-result-integrity-")).resolve()
requests = []
case = ""

def call(name, arguments, ident="call-1", index=0):
    return {"index": index, "id": ident, "type": "function", "function": {"name": name, "arguments": arguments}}

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *unused):
        pass

    def do_POST(self):
        requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
        delta, finish = {"content": "fixture complete"}, "stop"
        if case in ("malformed", "null", "array", "truncated-call", "length-call", "valid-call"):
            arguments = {"malformed": '{"file_path":', "null": "null", "array": "[]"}.get(case, "{}")
            delta = {"tool_calls": [call("file_write", arguments)]}
            finish = "length" if case == "length-call" else "tool_calls"
        elif case in ("nested-null", "nested-array"):
            value = "null" if case == "nested-null" else "[]"
            delta = {"tool_calls": [call("exec", json.dumps({"source": f"text(await tools.file_write({value}));"}))]}
            finish = "tool_calls"
        elif case == "length-text":
            delta, finish = {"content": "unfinished response"}, "length"
        elif case in ("invalid-final", "duplicate-final", "sibling-final", "valid-final"):
            arguments = '{"ok":"invalid"}' if case == "invalid-final" else '{"ok":true}'
            calls = [call("final_result", arguments)]
            if case == "duplicate-final":
                calls.append(call("final_result", '{"ok":true}', "call-2", 1))
            if case == "sibling-final":
                calls.insert(0, call("file_write", "{}", "call-2", 1))
            delta, finish = {"tool_calls": calls}, "tool_calls"
        if case == "valid-call" and len(requests) > 1:
            delta, finish = {"content": "fixture complete"}, "stop"
        chunks = [{"choices": [{"index": 0, "delta": delta, "finish_reason": None}]}]
        if not case.startswith("truncated"):
            chunks.append({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
        body = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks)
        if not case.startswith("truncated"):
            body += "data: [DONE]\n\n"
        body = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
rows = []
try:
    for case in ["malformed", "null", "array", "nested-null", "nested-array", "truncated-call", "truncated-text", "length-call", "length-text", "invalid-final", "duplicate-final", "sibling-final", "missing-final", "max-turns", "valid-call", "valid-final"]:
        requests.clear()
        fixture = root / case
        fixture.mkdir()
        env = {k: v for k, v in os.environ.items() if not k.startswith("BRO_HARNESS_")}
        env.update({"BRO_HOME": str(fixture / "home"), "CODEX_HOME": str(fixture / "codex"), "BRO_HARNESS_TRANSPORT": "openai-chat", "OPENAI_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1", "OPENAI_API_KEY": "synthetic-fixture", "BRO_HARNESS_WEB_SEARCH": "0", "BRO_HARNESS_NUDGES": "0", "BRO_HARNESS_MAX_TURNS": "0" if case == "max-turns" else "3", "BRO_HARNESS_TOOL_DEFAULTS": json.dumps({"default:file_write.file_path": "mutation.txt", "default:file_write.content": "completed mutation"})})
        cmd = [args.harness, "--cwd", str(fixture), "--model", "fixture-model", "--code-mode", "optional" if case.startswith("nested-") else "off", "--system-prompt", "", "--mcp-config", '{"mcpServers":{}}', "-p", "Complete the fixture."]
        if "final" in case:
            cmd += ["--output-schema", '{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}']
        result = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=30)
        events = []
        for line in result.stdout.splitlines():
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
        terminal = [event for event in events if event.get("type") == "result"]
        success = any(event.get("subtype") == "success" for event in terminal)
        mutated = (fixture / "mutation.txt").exists()
        expected_success = case in ("valid-call", "valid-final")
        passed = bool(terminal) and success == expected_success and mutated == (case == "valid-call")
        row = {"case": case, "exit": result.returncode, "requests": len(requests), "mutated": mutated, "terminal": terminal, "contract_passed": passed}
        rows.append(row)
        (fixture / "requests.json").write_text(json.dumps(requests, indent=2) + "\n")
        (fixture / "events.json").write_text(json.dumps(events, indent=2) + "\n")
        (fixture / "stderr.txt").write_text(result.stderr)
        print(json.dumps(row), flush=True)
finally:
    server.shutdown()
    server.server_close()
(root / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
print("Artifacts:", root)
if args.check and not all(row["contract_passed"] for row in rows):
    raise SystemExit(1)
