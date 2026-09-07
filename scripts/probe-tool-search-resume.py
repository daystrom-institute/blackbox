#!/usr/bin/env python3
"""Check tool-schema resumes and local refusal using a synthetic HTTP provider.

No live provider or daemon is contacted. Each resume starts from independently
restored snapshot and durable event-log bytes in a private temporary directory.
"""

import argparse
import copy
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import uuid


RESUME_ERROR = "error.resume_tool_schema_missing"
REPORT_TOOL = "mcp__blackbox__bro_report"


class Probe:
    def __init__(self, binary):
        self.root = Path(tempfile.mkdtemp(prefix="bbox-tool-resume-")).resolve()
        self.root.chmod(0o700)
        self.requests = []
        self.results = []
        self.pending_activation = None
        self.report_in_catalog = True
        self.binary = binary.resolve(strict=True)

    def save(self, name, value):
        (self.root / name).write_text(json.dumps(value, indent=2))

    def endpoint(self):
        probe = self

        class Endpoint(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, value):
                data = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                self.reply(405, {})

            def do_DELETE(self):
                self.reply(200, {})

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if self.path == "/mcp":
                    self.mcp(body)
                    return
                probe.requests.append(body)
                probe.save(f"request-{len(probe.requests)}.json", body)
                if len(probe.requests) > 20:
                    self.reply(429, {"error": "synthetic request budget exhausted"})
                    return
                activate = probe.pending_activation
                probe.pending_activation = None
                block = ({"type": "tool_use", "id": "activate-" + str(len(probe.requests)),
                          "name": "tool_search", "input": {"query": "select:" + activate}}
                         if activate else {"type": "text", "text": ""})
                events = [
                    {"type": "message_start", "message": {"id": "synthetic", "type": "message",
                     "role": "assistant", "content": [], "model": body["model"],
                     "usage": {"input_tokens": 10, "output_tokens": 1}}},
                    {"type": "content_block_start", "index": 0, "content_block": block},
                ]
                if not activate:
                    events.append({"type": "content_block_delta", "index": 0,
                                   "delta": {"type": "text_delta", "text": "PROBE_OK"}})
                events.extend([
                    {"type": "content_block_stop", "index": 0},
                    {"type": "message_delta", "delta": {"stop_reason": "tool_use" if activate else "end_turn"},
                     "usage": {"output_tokens": 1}},
                    {"type": "message_stop"},
                ])
                data = "".join("event: " + event["type"] + "\ndata: " + json.dumps(event) + "\n\n"
                               for event in events).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def mcp(self, body):
                ident = body.get("id")
                if ident is None:
                    self.reply(202, {})
                    return
                method = body.get("method")
                if method == "initialize":
                    result = {"protocolVersion": body["params"]["protocolVersion"],
                              "capabilities": {"tools": {}},
                              "serverInfo": {"name": "synthetic", "version": "1"}}
                elif method == "tools/list":
                    result = {"tools": [{"name": "bro_report", "description": "Report synthetic progress.",
                        "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}},
                                        "required": ["message"]}}] if probe.report_in_catalog else []}
                else:
                    self.reply(200, {"jsonrpc": "2.0", "id": ident,
                                     "error": {"code": -32601, "message": "Unexpected method"}})
                    return
                self.reply(200, {"jsonrpc": "2.0", "id": ident, "result": result})

        return Endpoint

    def run_process(self, label, extra, expected_requests, refused=False, expected_tool=None):
        start = len(self.requests)
        with (self.root / f"{label}.stdout").open("w") as out, (self.root / f"{label}.stderr").open("w") as err:
            process = subprocess.run(self.base + extra, env=self.env, stdout=out, stderr=err, timeout=45)
        emitted = self.requests[start:]
        stdout = (self.root / f"{label}.stdout").read_text()
        outcomes = []
        for line in stdout.splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") == "result":
                outcomes.append(event)
        guard = [event for event in outcomes if event.get("is_error") is True
                 and event.get("subtype") == "error" and RESUME_ERROR in event.get("result", "")]
        result = {"case": label, "exit_code": process.returncode, "provider_requests": len(emitted),
                  "expected_refusal": refused, "guard_error_present": bool(guard)}
        self.results.append(result)
        self.save("cases.json", self.results)
        assert len(emitted) == expected_requests, result
        if refused:
            assert guard, result
        else:
            assert process.returncode == 0 and outcomes and not any(e.get("is_error") for e in outcomes), result
            assert "PROBE_OK" in stdout, result
        if expected_tool is not None:
            assert emitted and any(t.get("name") == expected_tool for t in emitted[-1].get("tools", [])), result
        assert all(any(t.get("type") == "web_search_20250305" for t in r.get("tools", []))
                   for r in emitted), result

    def session_files(self, sid):
        directory = self.root / "bro" / "harness-sessions"
        return directory / f"{sid}.json", directory / f"{sid}.events.jsonl"

    def capture(self, sid):
        snapshot_path, event_path = self.session_files(sid)
        return json.loads(snapshot_path.read_text()), event_path.read_bytes()

    def restore(self, sid, baseline, change=None):
        snapshot, events = baseline
        snapshot = copy.deepcopy(snapshot)
        if change:
            change(snapshot)
        snapshot_path, event_path = self.session_files(sid)
        snapshot_path.write_text(json.dumps(snapshot))
        event_path.write_bytes(events)

    def run(self):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.endpoint())
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        url = f"http://127.0.0.1:{server.server_port}"
        self.env = {k: v for k, v in os.environ.items() if k in ("PATH", "TMPDIR", "SYSTEMROOT")}
        self.env.update({"HOME": str(self.root), "BRO_HOME": str(self.root / "bro"),
            "XDG_CONFIG_HOME": str(self.root / "config"), "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_DATA_HOME": str(self.root / "data"), "XDG_CACHE_HOME": str(self.root / "cache"),
            "BRO_HARNESS_PROVIDER": "glm", "BRO_HARNESS_TRANSPORT": "anthropic",
            "BRO_HARNESS_WEB_SEARCH": "1", "BRO_HARNESS_MAX_TURNS": "4",
            "ANTHROPIC_AUTH_TOKEN": "synthetic", "ANTHROPIC_BASE_URL": url})
        self.base = [str(self.binary), "--cwd", str(self.root), "--system-prompt", "", "--model", "glm-5.3",
                     "--code-mode", "only", "--prompt", "Synthetic protocol probe.", "--mcp-config",
                     json.dumps({"mcpServers": {"blackbox": {"type": "http", "url": url + "/mcp"}}})]
        print("EVIDENCE " + str(self.root), flush=True)
        status = "fail"
        try:
            sid = str(uuid.uuid4())
            self.pending_activation = "file_read"
            self.run_process("initial", ["--session-id", sid], 2, expected_tool="file_read")
            baseline = self.capture(sid)
            assert baseline[0]["side"]["tool_activations"], "initial activation was not saved"

            self.restore(sid, baseline)
            self.run_process("resume", ["--resume", sid], 1, expected_tool="file_read")

            def legacy(snapshot):
                snapshot["side"].pop("tool_activations", None)
                snapshot["snapshot"] = [{"role": "user", "content": [
                    {"type": "text", "text": "Earlier work summarized."}]}]

            self.restore(sid, baseline, legacy)
            self.run_process("legacy-resume", ["--resume", sid], 1, expected_tool="file_read")

            self.restore(sid, baseline)
            self.run_process("denied-resume", ["--resume", sid, "--deny-tools", "file_read"], 0, refused=True)

            self.restore(sid, baseline, lambda s: s["side"].update(tool_activations=[]))
            self.run_process("explicit-empty-resume", ["--resume", sid], 0, refused=True)

            # Persisted call/result receipts must remain protective without the sidecar log.
            self.restore(sid, baseline, lambda s: s["side"].update(tool_activations=[]))
            self.session_files(sid)[1].write_bytes(b"")
            self.run_process("explicit-empty-without-events", ["--resume", sid], 0, refused=True)

            report_sid = str(uuid.uuid4())
            self.pending_activation = REPORT_TOOL
            self.run_process("report-initial", ["--session-id", report_sid], 2, expected_tool=REPORT_TOOL)
            report_baseline = self.capture(report_sid)
            self.restore(report_sid, report_baseline)
            self.run_process("report-resume", ["--resume", report_sid], 1, expected_tool=REPORT_TOOL)

            self.restore(report_sid, report_baseline)
            self.report_in_catalog = False
            self.run_process("removed-catalog-resume", ["--resume", report_sid], 0, refused=True)
            status = "pass"
        finally:
            server.shutdown()
            server.server_close()
            worker.join()
            summary = {"status": status, "evidence": str(self.root), "request_count": len(self.requests),
                       "cases": self.results}
            self.save("summary.json", summary)
            print(json.dumps(summary, indent=2), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    Probe(args.binary).run()


if __name__ == "__main__":
    main()
