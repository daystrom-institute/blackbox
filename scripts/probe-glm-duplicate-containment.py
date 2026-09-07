#!/usr/bin/env python3
"""Exercise GLM duplicate-client containment through a real isolated harness.

By default every provider response and MCP tool is synthetic and local. Optional
--live --settings PATH --capture PATH injects a captured duplicate SSE response,
then sends only correction requests to GLM. It never executes real client tools.
Evidence, raw SSE, request bodies, and a synthetic mutator counter stay in a new
private temporary directory. No credentials enter child arguments or captures.
"""

import argparse
import http.server
import json
import os
from pathlib import Path
import runpy
import signal
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


COMMON = runpy.run_path(str(Path(__file__).with_name("probe-glm-native-search.py")), run_name="probe_helpers")
REPORT = COMMON["REPORT_NAME"]
MARKER = COMMON["AMBIGUOUS_BATCH_ERROR"]
RECURRENT = "Automatic correction was already attempted for this user turn; stopping."


def encode_sse(blocks, model, stop="tool_use"):
    events = [{"type": "message_start", "message": {"id": "synthetic", "type": "message",
        "role": "assistant", "content": [], "model": model,
        "usage": {"input_tokens": 10, "output_tokens": 1}}}]
    for index, block in enumerate(blocks):
        initial = dict(block)
        if block.get("type") == "text":
            initial["text"] = ""
        events.append({"type": "content_block_start", "index": index, "content_block": initial})
        if block.get("type") == "text":
            events.append({"type": "content_block_delta", "index": index,
                           "delta": {"type": "text_delta", "text": block["text"]}})
        events.append({"type": "content_block_stop", "index": index})
    events.extend([{"type": "message_delta", "delta": {"stop_reason": stop},
                    "usage": {"output_tokens": 1}}, {"type": "message_stop"}])
    return "".join("event: " + event["type"] + "\ndata: " + json.dumps(event) + "\n\n"
                   for event in events).encode()


def duplicate_blocks(prefix):
    # The first client call precedes the native result; the identical second call
    # follows provider-internal continuation, using a distinct client call ID.
    return [{"type": "server_tool_use", "id": prefix + "-native", "name": "web_search_prime",
             "input": {"search_query": "official Python pathlib.Path.samefile documentation"}},
            {"type": "tool_use", "id": prefix + "-client-a", "name": REPORT,
             "input": {"message": "SECOND_SEARCH_COMPLETE"}},
            {"type": "tool_result", "tool_use_id": prefix + "-native", "content": json.dumps([
                {"title": "Python pathlib documentation", "url": "https://docs.python.org/3/library/pathlib.html#pathlib.Path.samefile",
                 "content": "Return whether this path points to the same file as the other path."}])},
            {"type": "tool_use", "id": prefix + "-client-b", "name": REPORT,
             "input": {"message": "SECOND_SEARCH_COMPLETE"}}]


class Probe:
    def __init__(self, args):
        self.args = args
        self.root = Path(tempfile.mkdtemp(prefix="glm-duplicate-containment-")).resolve()
        self.root.chmod(0o700)
        self.token = ""
        self.upstream = None
        if args.live:
            settings = json.loads(args.settings.expanduser().read_text())["env"]
            self.token = settings["ANTHROPIC_AUTH_TOKEN"]
            self.upstream = settings["ANTHROPIC_BASE_URL"].rstrip("/")
            url = urllib.parse.urlsplit(self.upstream)
            if (url.scheme != "https" or not url.hostname or url.username or url.password
                    or url.query or url.fragment or not isinstance(self.token, str) or not self.token):
                raise ValueError("HTTPS settings without URL credentials and a nonempty token required")
        self.capture = args.capture.read_bytes() if args.capture else None
        if args.capture:
            parsed = COMMON["parse_response"](args.capture)
            clients = [b for b in parsed["blocks"] if b.get("type") == "tool_use"]
            assert parsed["message_stop"] and not parsed["errors"], "capture must be a complete valid SSE response"
            assert COMMON["duplicate_calls"](clients), "capture must contain identical client calls"
            assert all(c.get("name") == REPORT for c in clients), "capture client calls must target synthetic bro_report"
        self.opener = urllib.request.build_opener(COMMON["NoRedirect"]())
        self.rows = []
        self.mutations = []
        self.results = []
        self.case = None
        self.step = 0
        self.live_requests = 0

    def clean(self, data):
        return data.replace(self.token.encode(), b"[REDACTED]") if self.token else data

    def save(self, name, value):
        (self.root / name).write_bytes(self.clean(json.dumps(value, indent=2).encode()))

    def handler(self):
        probe = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, data, content_type="application/json"):
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                self.reply(405, b"{}")

            def do_DELETE(self):
                self.reply(200, b"{}")

            def do_POST(self):
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 < size <= COMMON["MAX_BODY_BYTES"]:
                        raise ValueError("request byte budget")
                    body = json.loads(self.rfile.read(size))
                    if self.path == "/mcp":
                        self.mcp(body)
                    else:
                        self.provider(body)
                except Exception as error:
                    probe.save("handler-error.json", {"type": type(error).__name__})
                    try:
                        self.reply(502, b'{"error":"probe handler failed"}')
                    except OSError:
                        pass

            def mcp(self, body):
                ident = body.get("id")
                if ident is None:
                    self.reply(202, b"{}")
                    return
                method = body.get("method")
                if method == "initialize":
                    result = {"protocolVersion": body["params"]["protocolVersion"], "capabilities": {"tools": {}},
                              "serverInfo": {"name": "synthetic-counter", "version": "1"}}
                elif method == "tools/list":
                    result = {"tools": [{"name": "bro_report", "description": "Submit a progress report once.",
                        "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}},
                                        "required": ["message"]}}]}
                elif method == "tools/call":
                    probe.mutations.append({"case": probe.case, "after_response_n": len(probe.rows),
                                            "params": body.get("params", {})})
                    probe.save("mutator-calls.json", probe.mutations)
                    result = {"content": [{"type": "text", "text": json.dumps({"ok": True})}], "isError": False}
                else:
                    self.reply(200, json.dumps({"jsonrpc": "2.0", "id": ident,
                        "error": {"code": -32601, "message": "Unknown method"}}).encode())
                    return
                self.reply(200, json.dumps({"jsonrpc": "2.0", "id": ident, "result": result}).encode())

            def provider(self, body):
                probe.step += 1
                step = probe.step
                row = {"n": len(probe.rows) + 1, "case": probe.case, "step": step,
                       "mutations_before_request": len(probe.mutations), "source": "synthetic"}
                probe.rows.append(row)
                stem = f"{row['n']:03}"
                probe.save(stem + "-request.json", body)
                if step > 8:
                    raise ValueError("provider request budget exceeded")
                if step == 1:
                    blocks = [{"type": "tool_use", "id": "activate-report", "name": "tool_search",
                               "input": {"query": "select:" + REPORT}}]
                    data = encode_sse(blocks, probe.args.model)
                elif step == 2:
                    assert any(t.get("name") == REPORT for t in body.get("tools", [])), "report schema missing"
                    data = probe.capture or encode_sse(duplicate_blocks("first"), probe.args.model)
                    row["source"] = "injected_capture" if probe.capture else "synthetic_ambiguity"
                elif probe.case == "recurrent":
                    if step != 3:
                        raise ValueError("recurrent ambiguity should terminate without another request")
                    data = encode_sse(duplicate_blocks("second"), probe.args.model)
                    row["source"] = "synthetic_ambiguity"
                elif probe.args.live:
                    row["source"] = "live_correction"
                    data = self.forward(body, row)
                elif step == 3:
                    data = encode_sse([{"type": "tool_use", "id": "corrected-single", "name": REPORT,
                                        "input": {"message": "SECOND_SEARCH_COMPLETE"}}], probe.args.model)
                elif step == 4:
                    data = encode_sse([{"type": "text", "text": "PROBE_OK"}], probe.args.model, "end_turn")
                else:
                    raise ValueError("unexpected synthetic continuation")
                row.setdefault("http_status", 200)
                path = probe.root / (stem + "-response.sse")
                path.write_bytes(probe.clean(data))
                row["response"] = COMMON["parse_response"](path)
                probe.save(stem + "-meta.json", row)
                self.reply(row["http_status"], data, "text/event-stream")

            def forward(self, body, row):
                probe.live_requests += 1
                if probe.live_requests > 4:
                    raise ValueError("live correction request budget exceeded")
                headers = {"Authorization": "Bearer " + probe.token, "Content-Type": "application/json",
                           "anthropic-version": self.headers.get("anthropic-version", "2023-06-01")}
                if self.headers.get("anthropic-beta"):
                    headers["anthropic-beta"] = self.headers["anthropic-beta"]
                request = urllib.request.Request(probe.upstream + "/v1/messages", data=json.dumps(body).encode(), headers=headers)
                try:
                    response = probe.opener.open(request, timeout=20)
                except urllib.error.HTTPError as error:
                    response = error
                chunks, total = [], 0
                deadline = time.monotonic() + 90
                with response:
                    row["http_status"] = response.status
                    row["headers"] = {k.lower(): v for k, v in response.headers.items()
                                      if k.lower() in COMMON["SAFE_RESPONSE_HEADERS"]}
                    while True:
                        if time.monotonic() > deadline:
                            raise TimeoutError("live response deadline")
                        chunk = response.read1(65536)
                        if not chunk:
                            break
                        chunks.append(chunk)
                        total += len(chunk)
                        if total > COMMON["MAX_BODY_BYTES"]:
                            raise ValueError("response byte budget exceeded")
                return b"".join(chunks)

        return Handler

    def run_case(self, name):
        self.case, self.step = name, 0
        sid = str(uuid.uuid4())
        command = self.base + ["--session-id", sid, "--prompt",
            "Search the web for official Python pathlib.Path.samefile documentation. Then call bro_report "
            "with message SECOND_SEARCH_COMPLETE exactly once. Reuse completed search results if tool calls need correction."]
        process = subprocess.Popen(command, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        try:
            out, err = process.communicate(timeout=240 if self.args.live else 60)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            out, err = process.communicate()
            raise TimeoutError("harness process budget exceeded")
        finally:
            if process.poll() is not None:
                (self.root / f"{name}.stdout").write_bytes(self.clean(out))
                (self.root / f"{name}.stderr").write_bytes(self.clean(err))
        events_path = self.root / "bro" / "harness-sessions" / f"{sid}.events.jsonl"
        events = [json.loads(line) for line in events_path.read_text().splitlines()]
        snapshot = json.loads(events_path.with_name(sid + ".json").read_text())
        rows = [row for row in self.rows if row["case"] == name]
        mutations = [call for call in self.mutations if call["case"] == name]
        checks = []
        for index, row in enumerate(rows):
            response = row["response"]
            assert row["http_status"] == 200 and response["message_stop"] and not response["errors"], row["n"]
            clients = [block for block in response["blocks"] if block.get("type") == "tool_use"]
            native = COMMON["native_blocks"](response["blocks"])
            if row["source"] == "live_correction":
                assert not native, "correction introduced a new native search"
            if COMMON["duplicate_calls"](clients):
                ids = [call["id"] for call in clients]
                pairing = COMMON["rejected_batch"](events, ids)
                assert pairing["all_rejected"], pairing
                assert not any(call["after_response_n"] == row["n"] for call in mutations), "ambiguous batch executed"
                assert COMMON["matching_native"](events, native) == native, "native observation changed"
                assert COMMON["matching_native"](snapshot, native) == native, "native snapshot changed"
                assert COMMON["rejected_batch"](snapshot, ids)["all_rejected"], "snapshot omitted paired errors"
                if index + 1 < len(rows):
                    next_request = json.loads((self.root / f"{rows[index+1]['n']:03}-request.json").read_text())
                    assert COMMON["rejected_batch"](next_request["messages"], ids)["all_rejected"], "correction omitted errors"
                    assert COMMON["matching_native"](next_request["messages"], native) == native, "native replay changed"
                checks.append({"request": row["n"], "durable_rejections": pairing, "native_preserved": True})
        outcomes = [x["event"] for x in events if x.get("event", {}).get("type") == "result"]
        assert outcomes, "terminal result missing"
        if name == "recurrent":
            assert len(rows) == 3 and len(checks) == 2 and not mutations, "recurrent batch escaped guard"
            assert outcomes[-1].get("is_error") and RECURRENT in outcomes[-1].get("result", ""), outcomes[-1]
        else:
            assert process.returncode == 0 and not outcomes[-1].get("is_error"), outcomes[-1]
            assert len(mutations) == 1, mutations
            assert mutations[0]["params"].get("name") == "bro_report", mutations
            assert mutations[0]["params"].get("arguments", {}).get("message") == "SECOND_SEARCH_COMPLETE", mutations
            assert checks, "ambiguity not exercised"
        result = {"case": name, "status": "pass", "provider_requests": len(rows),
                  "live_requests": sum(row["source"] == "live_correction" for row in rows),
                  "mutator_executions": len(mutations), "contained_batches": checks}
        self.results.append(result)
        self.save("summary.json", self.results)
        print(json.dumps(result), flush=True)

    def run(self):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        server.daemon_threads = True
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        url = f"http://127.0.0.1:{server.server_port}"
        self.env = {k: v for k, v in os.environ.items() if k in ("PATH", "TMPDIR", "SYSTEMROOT")}
        self.env.update({"HOME": str(self.root / "home"), "BRO_HOME": str(self.root / "bro"),
            "BRO_HARNESS_PROVIDER": "glm", "BRO_HARNESS_TRANSPORT": "anthropic",
            "BRO_HARNESS_WEB_SEARCH": "1", "BRO_HARNESS_MAX_TURNS": "8", "BRO_HARNESS_MAX_TOKENS": "4096",
            "ANTHROPIC_AUTH_TOKEN": "synthetic", "ANTHROPIC_BASE_URL": url})
        self.base = [str(self.args.binary.resolve(strict=True)), "--cwd", str(self.root), "--system-prompt", "",
            "--model", self.args.model, "--code-mode", "only", "--mcp-config",
            json.dumps({"mcpServers": {"blackbox": {"type": "http", "url": url + "/mcp"}}})]
        print("EVIDENCE " + str(self.root), flush=True)
        status = "fail"
        try:
            for case in ("recovery", "recurrent") if self.args.cases == "all" else (self.args.cases,):
                self.run_case(case)
            status = "pass"
        finally:
            server.shutdown()
            server.server_close()
            self.save("summary.json", {"status": status, "cases": self.results,
                                      "live_request_count": self.live_requests})
        return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--model", default="glm-5.3-flash")
    parser.add_argument("--cases", choices=("all", "recovery", "recurrent"), default="all")
    parser.add_argument("--live", action="store_true", help="send only correction requests to the live provider")
    parser.add_argument("--settings", type=Path)
    parser.add_argument("--capture", type=Path, help="captured successful mixed native/duplicate-client SSE response")
    args = parser.parse_args()
    if args.live and (not args.settings or not args.capture or args.cases == "recurrent"):
        parser.error("--live requires explicit --settings and --capture plus a recovery case")
    if args.settings and not args.live:
        parser.error("--settings requires explicit --live")
    os.umask(0o077)
    try:
        return Probe(args).run()
    except Exception as error:
        print(json.dumps({"status": "fail", "error_type": type(error).__name__}))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
