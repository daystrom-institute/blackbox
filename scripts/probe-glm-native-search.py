#!/usr/bin/env python3
r"""Opt-in live GLM native-search and process-resume probe (Python stdlib only).

Example:
  scripts/probe-glm-native-search.py --live --binary /path/to/bro-harness \
      --settings /path/to/settings.json --lost-activation

Settings must contain env.ANTHROPIC_BASE_URL and env.ANTHROPIC_AUTH_TOKEN.
The credential stays in this process, never in child arguments or captures.
Evidence is retained in a new private temporary directory, outside the repo.
Exit codes: 0 = observed checks pass, 1 = behavioral failure, 2 = inconclusive.
These finite probes establish observations, not a guarantee of model behavior.
"""

import argparse
import collections
import datetime
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


MAX_BODY_BYTES = 16 * 1024 * 1024
SAFE_RESPONSE_HEADERS = {"request-id", "x-request-id", "x-log-id", "content-type"}
REPORT_NAME = "mcp__blackbox__bro_report"


def arguments():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--live", action="store_true", help="authorize live provider requests")
    parser.add_argument("--binary", type=Path, help="explicit bro-harness executable path")
    parser.add_argument("--settings", type=Path, help="explicit credential settings JSON path")
    parser.add_argument("--model", default="glm-5.3-flash")
    parser.add_argument("--cases", choices=("all", "tools", "search"), default="all")
    parser.add_argument("--lost-activation", action="store_true",
                        help="clear activations in this probe's own report session before resume")
    parser.add_argument("--report-resumes", type=int, default=3)
    parser.add_argument("--max-requests", type=int, default=40, help="total upstream request limit")
    parser.add_argument("--max-turns", type=int, default=8, help="model turns per harness process")
    parser.add_argument("--case-timeout", type=int, default=180, help="seconds per harness process")
    parser.add_argument("--request-timeout", type=int, default=90, help="seconds per upstream response")
    args = parser.parse_args()
    if not args.live:
        parser.error("no network calls made: --live is required")
    if args.binary is None or args.settings is None:
        parser.error("--binary and --settings are required for a live probe")
    for key, maximum in (("report_resumes", 10), ("max_requests", 100), ("max_turns", 20),
                         ("case_timeout", 600), ("request_timeout", 180)):
        if not 1 <= getattr(args, key) <= maximum:
            parser.error(f"--{key.replace('_', '-')} must be between 1 and {maximum}")
    if args.lost_activation and args.cases == "search":
        parser.error("--lost-activation requires tools or all cases")
    args.binary = args.binary.expanduser().resolve(strict=True)
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("--binary must be an executable file")
    return args


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never forward the provider credential to a redirect target."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Probe:
    def __init__(self, args):
        self.args = args
        settings = json.loads(args.settings.expanduser().read_text())["env"]
        self.token = settings["ANTHROPIC_AUTH_TOKEN"]
        self.upstream = settings["ANTHROPIC_BASE_URL"].rstrip("/")
        url = urllib.parse.urlsplit(self.upstream)
        if (url.scheme != "https" or not url.hostname or url.username or url.password
                or url.query or url.fragment or not isinstance(self.token, str) or not self.token):
            raise ValueError("settings require an HTTPS base URL without credentials and a token")
        self.root = Path(tempfile.mkdtemp(prefix="glm-native-search-")).resolve()
        self.root.chmod(0o700)
        self.lock = threading.Lock()
        self.requests = []
        self.mcp_calls = []
        self.cases = []
        self.case = None
        self.aborted = False
        self.active = 0
        self.opener = urllib.request.build_opener(NoRedirect())

    def clean(self, data):
        return data.replace(self.token.encode(), b"[REDACTED]")

    def save(self, name, value):
        data = json.dumps(value, indent=2, ensure_ascii=False).encode()
        (self.root / name).write_bytes(self.clean(data))

    def snapshot_path(self, sid):
        return self.root / "bro" / "harness-sessions" / f"{sid}.json"

    def handler(self):
        probe = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, body):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                self.reply(405, {"error": "POST required"})

            def do_DELETE(self):
                self.reply(200, {})

            def do_POST(self):
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 < size <= MAX_BODY_BYTES:
                        self.reply(413, {"error": "bounded JSON body required"})
                        return
                    data = self.rfile.read(size)
                    body = json.loads(data)
                    if self.path == "/mcp":
                        self.mcp(body)
                    elif self.path in ("/v1/messages", "/v1/messages?beta=true"):
                        self.proxy(data, body)
                    else:
                        self.reply(404, {"error": "unsupported probe route"})
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception as error:
                    # Exception text can contain a URL or credential; retain only its class.
                    with probe.lock:
                        probe.aborted = True
                        probe.save("server-error.json", {"type": type(error).__name__})
                    try:
                        self.reply(502, {"error": "probe server failure"})
                    except OSError:
                        pass

            def mcp(self, body):
                method, ident = body.get("method"), body.get("id")
                if ident is None:
                    self.reply(202, {})
                    return
                if method == "initialize":
                    result = {"protocolVersion": body["params"]["protocolVersion"],
                              "capabilities": {"tools": {}},
                              "serverInfo": {"name": "synthetic-probe", "version": "1"}}
                elif method == "tools/list":
                    result = {"tools": [{"name": "bro_report",
                        "description": "Attach the latest progress report to the current task.",
                        "inputSchema": {"type": "object", "properties": {
                            "message": {"type": "string"}, "task_id": {"type": "string"}},
                            "required": ["message"]}}]}
                elif method == "tools/call":
                    params = body.get("params", {})
                    valid = (params.get("name") == "bro_report"
                             and isinstance(params.get("arguments", {}).get("message"), str))
                    with probe.lock:
                        probe.mcp_calls.append({"case": probe.case, "params": params, "success": valid})
                        probe.save("mcp-calls.json", probe.mcp_calls)
                    result = {"content": [{"type": "text", "text": json.dumps({"ok": valid})}],
                              "isError": not valid}
                else:
                    self.reply(200, {"jsonrpc": "2.0", "id": ident,
                                     "error": {"code": -32601, "message": "Unknown method"}})
                    return
                self.reply(200, {"jsonrpc": "2.0", "id": ident, "result": result})

            def proxy(self, data, body):
                with probe.lock:
                    if probe.aborted or len(probe.requests) >= probe.args.max_requests:
                        probe.aborted = True
                        probe.save("budget-exhausted.json", {"request_budget": True})
                        self.reply(429, {"error": "probe stopped or request budget exhausted"})
                        return
                    row = {"n": len(probe.requests) + 1, "case": probe.case,
                           "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                           "requested_model": body.get("model")}
                    probe.requests.append(row)
                    probe.active += 1
                stem = f"{row['n']:03}"
                chunks = []
                response = None
                try:
                    probe.save(f"{stem}-request.json", body)
                    headers = {"Authorization": "Bearer " + probe.token,
                               "Content-Type": "application/json",
                               "anthropic-version": self.headers.get("anthropic-version", "2023-06-01")}
                    if self.headers.get("anthropic-beta"):
                        headers["anthropic-beta"] = self.headers["anthropic-beta"]
                    request = urllib.request.Request(probe.upstream + self.path, data=data, headers=headers)
                    deadline = time.monotonic() + probe.args.request_timeout
                    try:
                        response = probe.opener.open(request, timeout=min(15, probe.args.request_timeout))
                    except urllib.error.HTTPError as error:
                        response = error
                    row["http_status"] = response.status
                    row["headers"] = {k.lower(): v for k, v in response.headers.items()
                                      if k.lower() in SAFE_RESPONSE_HEADERS}
                    self.send_response(response.status)
                    self.send_header("Content-Type", response.headers.get("Content-Type", "text/event-stream"))
                    self.end_headers()
                    total = 0
                    while True:
                        if time.monotonic() >= deadline or probe.aborted:
                            raise TimeoutError("bounded upstream response")
                        chunk = response.read1(65536)
                        if not chunk:
                            break
                        total += len(chunk)
                        if total > MAX_BODY_BYTES:
                            raise ValueError("response budget exceeded")
                        chunks.append(chunk)
                        self.wfile.write(chunk)
                        self.wfile.flush()
                    row["complete"] = True
                except Exception as error:
                    row["error_type"] = type(error).__name__
                    with probe.lock:
                        probe.aborted = True
                finally:
                    if response is not None:
                        response.close()
                    # Buffering permits exact token redaction even across SSE chunk boundaries.
                    (probe.root / f"{stem}-response.sse").write_bytes(probe.clean(b"".join(chunks)))
                    with probe.lock:
                        probe.save(f"{stem}-meta.json", row)
                        probe.save("requests.json", probe.requests)
                        probe.active -= 1

        return Handler

    def run_case(self, label, prompt, sid=None, search=False, report=True):
        if self.aborted:
            raise RuntimeError("prior probe error prevents dependent cases")
        fresh = sid is None
        sid = sid or str(uuid.uuid4())
        self.case = label
        row = {"case": label, "session_id": sid, "fresh": fresh,
               "expects_search": search, "expects_report": report}
        self.cases.append(row)
        command = self.base + ["--session-id" if fresh else "--resume", sid, "--prompt", prompt]
        process = subprocess.Popen(command, env=self.env, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, start_new_session=True)
        try:
            out, err = process.communicate(timeout=self.args.case_timeout)
            row["exit_code"] = process.returncode
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            out, err = process.communicate()
            row["timeout"] = True
            self.aborted = True
        (self.root / f"{label}.stdout").write_bytes(self.clean(out))
        (self.root / f"{label}.stderr").write_bytes(self.clean(err))
        results = []
        for line in out.decode(errors="replace").splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") == "result":
                results.append(event)
        row["harness_results"] = results
        # A killed child may leave one in-flight proxy request. Do not attribute it to a new case.
        deadline = time.monotonic() + self.args.request_timeout + 1
        while self.active and time.monotonic() < deadline:
            time.sleep(0.05)
        if self.active or row.get("exit_code") != 0:
            self.aborted = True
        snapshot = self.snapshot_path(sid)
        if snapshot.is_file():
            self.save(f"{label}-snapshot.json", json.loads(snapshot.read_text()))
        self.save("cases.json", self.cases)
        print(json.dumps({"case": label, "exit_code": row.get("exit_code"),
                          "timeout": row.get("timeout", False)}), flush=True)
        return sid

    def run(self):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        server.daemon_threads = True
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        url = f"http://127.0.0.1:{server.server_port}"
        home = self.root / "home"
        home.mkdir(mode=0o700)
        self.env = {k: v for k, v in os.environ.items() if k in ("PATH", "TMPDIR", "SYSTEMROOT")}
        self.env.update({"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
            "BRO_HOME": str(self.root / "bro"), "BRO_HARNESS_PROVIDER": "glm",
            "BRO_HARNESS_TRANSPORT": "anthropic", "ANTHROPIC_AUTH_TOKEN": "local-probe-placeholder",
            "ANTHROPIC_BASE_URL": url, "BRO_HARNESS_WEB_SEARCH": "1",
            "BRO_HARNESS_MAX_TURNS": str(self.args.max_turns), "BRO_HARNESS_MAX_TOKENS": "4096"})
        self.base = [str(self.args.binary), "--cwd", str(self.root), "--system-prompt", "",
                     "--model", self.args.model, "--code-mode", "only", "--mcp-config",
                     json.dumps({"mcpServers": {"blackbox": {"type": "http", "url": url + "/mcp"}}})]
        print("EVIDENCE " + str(self.root), flush=True)
        try:
            if self.args.cases in ("all", "tools"):
                self.run_case("fresh-arithmetic", "What is 17 multiplied by 23? Reply with just the number.",
                              report=False)
                sid = self.run_case("fresh-report", "Discover bro_report, then use it to report that "
                                    "the synthetic fixture is ready. Reply REPORT_OK after the tool succeeds.")
                for number in range(1, self.args.report_resumes + 1):
                    self.run_case(f"resume-report-{number}", "Use the already loaded bro_report directly "
                                  f"to report synthetic checkpoint {number}. Reply CHECKPOINT_OK after success.", sid)
                if self.args.lost_activation:
                    path = self.snapshot_path(sid)
                    state = json.loads(path.read_text())
                    if not state.get("side", {}).get("tool_activations"):
                        raise ValueError("lost-activation case requires previously persisted activations")
                    self.save("lost-activation-before.json", state)
                    state["side"]["tool_activations"] = []
                    # This UUID was created above, and this path is under our private BRO_HOME.
                    path.write_bytes(self.clean(json.dumps(state).encode()))
                    self.run_case("resume-lost-activation", "Use the already loaded bro_report directly "
                                  "to report the final synthetic checkpoint. Reply CHECKPOINT_OK after success.", sid)
            if self.args.cases in ("all", "search"):
                sid = self.run_case("fresh-search", "Search the web for the official Python documentation "
                    "for pathlib.Path.resolve. Give its official documentation URL and one sentence about "
                    "the strict parameter. Then discover bro_report and call it with message SEARCH_COMPLETE "
                    "before your final answer.", search=True)
                self.run_case("resume-after-search", "Using only what is already in this conversation, "
                    "repeat the documentation URL you found and call the already loaded bro_report with "
                    "message REPLAY_COMPLETE.", sid)
                self.run_case("resume-search", "Search the web for the official Python documentation for "
                    "pathlib.Path.samefile. Give its official documentation URL and one sentence about "
                    "what it returns. Then call the loaded bro_report with message SECOND_SEARCH_COMPLETE.",
                    sid, search=True)
        except Exception as error:
            self.aborted = True
            self.save("run-error.json", {"type": type(error).__name__})
        finally:
            server.shutdown()
            server.server_close()
            self.save("cases.json", self.cases)
        summary = summarize(self.root, self.requests, self.cases, self.mcp_calls, self.aborted)
        self.save("summary.json", summary)
        print(json.dumps({"status": summary["status"], "summary": str(self.root / "summary.json")}), flush=True)
        return {"pass": 0, "fail": 1, "inconclusive": 2}[summary["status"]]


def sse_events(path):
    """Decode SSE data frames, including multi-line data fields."""
    pending = []
    for line in path.read_text(errors="replace").splitlines() + [""]:
        if line.startswith("data:"):
            pending.append(line[5:].lstrip(" "))
        elif not line and pending:
            data = "\n".join(pending)
            pending = []
            if data != "[DONE]":
                try:
                    yield json.loads(data)
                except ValueError:
                    yield {"type": "error", "parse_error": "invalid SSE JSON"}


def parse_response(path):
    blocks, active, stopped = [], {}, []
    models, errors, stop_reasons = [], [], []
    complete = False
    try:
        for event in sse_events(path):
            kind, index = event.get("type"), event.get("index")
            if kind == "message_start":
                msg = event["message"]
                models.append({"id": msg.get("id"), "model": msg.get("model")})
                active = {}
            elif kind == "content_block_start":
                block = dict(event["content_block"])
                blocks.append(block)
                active[index] = block
            elif kind == "content_block_delta":
                block = active[index]
                delta = event.get("delta", {})
                delta_type = delta.get("type")
                if delta_type == "input_json_delta":
                    block["_input_json"] = block.get("_input_json", "") + delta.get("partial_json", "")
                elif delta_type in ("text_delta", "thinking_delta", "signature_delta"):
                    key = delta_type.removesuffix("_delta")
                    block[key] = block.get(key, "") + delta.get(key, "")
            elif kind == "content_block_stop":
                stopped.append(active[index])
            elif kind == "message_delta":
                stop_reasons.append(event.get("delta", {}).get("stop_reason"))
            elif kind == "message_stop":
                complete = True
            elif kind == "error":
                errors.append(event)
    except (ValueError, KeyError, TypeError) as error:
        errors.append({"parse_error": type(error).__name__})
    # Finish each observed block independently, even after an error or truncated stream.
    # A malformed client call must not hide native calls appearing later in the SSE.
    for block in blocks:
        partial = block.pop("_input_json", "")
        if partial:
            try:
                block["input"] = json.loads(partial)
            except ValueError:
                errors.append({"parse_error": "invalid tool input JSON", "id": block.get("id"),
                               "partial_input": partial})
                block["input"] = {"_malformed_partial_json": partial}
    return {"blocks": blocks, "completed_native_blocks": native_blocks(stopped),
            "observed": models, "errors": errors,
            "message_stop": complete, "stop_reasons": stop_reasons}


def nested_blocks(value):
    if isinstance(value, dict):
        if value.get("type") in ("server_tool_use", "web_search_tool_result", "tool_result", "tool_use"):
            yield value
        for child in value.values():
            yield from nested_blocks(child)
    elif isinstance(value, list):
        for child in value:
            yield from nested_blocks(child)


def native_blocks(blocks):
    ids = {b.get("id") for b in blocks if b.get("type") == "server_tool_use"}
    return [b for b in blocks if b.get("type") == "server_tool_use"
            or (b.get("type") in ("tool_result", "web_search_tool_result") and b.get("tool_use_id") in ids)]


def matching_native(value, expected):
    ids = {b.get("id") for b in expected if b.get("type") == "server_tool_use"}
    return [b for b in nested_blocks(value)
            if (b.get("type") == "server_tool_use" and b.get("id") in ids)
            or (b.get("type") in ("tool_result", "web_search_tool_result") and b.get("tool_use_id") in ids)]


def duplicate_calls(calls):
    groups = collections.defaultdict(list)
    for call in calls:
        key = json.dumps([call.get("name"), call.get("input", call.get("arguments"))], sort_keys=True)
        groups[key].append(call.get("id"))
    return [{"name_and_input": json.loads(key), "count": len(ids), "ids": ids}
            for key, ids in groups.items() if len(ids) > 1]


def summarize(root, requests, cases, mcp_calls, aborted):
    failures, unknown, wire = [], [], []
    if aborted:
        unknown.append("run aborted, timed out, or exhausted a budget")
    by_case = {case["case"]: case for case in cases}
    for meta in requests:
        stem = f"{meta['n']:03}"
        request = json.loads((root / f"{stem}-request.json").read_text())
        response = parse_response(root / f"{stem}-response.sse")
        blocks = response.pop("blocks")
        completed_native = response.pop("completed_native_blocks")
        native = native_blocks(blocks)
        client = [b for b in blocks if b.get("type") == "tool_use"]
        tools = request.get("tools", [])
        row = dict(meta, **response, native_calls=[b for b in native if b.get("type") == "server_tool_use"],
                   client_calls=client, duplicate_client_emissions=duplicate_calls(client),
                   native_search_enabled=any(t.get("type") == "web_search_20250305" for t in tools),
                   report_loaded=any(t.get("name") == REPORT_NAME for t in tools))
        row["queries"] = [b.get("input", {}).get("search_query", b.get("input", {}).get("query"))
                          for b in row["native_calls"] if isinstance(b.get("input"), dict)]
        failed_response = (meta.get("http_status") != 200 or not meta.get("complete") or response["errors"]
                           or not response["message_stop"] or not response["observed"])
        if failed_response:
            unknown.append(f"request {meta['n']}: missing, failed, or incomplete provider response")
        if "max_tokens" in response["stop_reasons"]:
            unknown.append(f"request {meta['n']}: model output token budget exhausted")
        if not row["native_search_enabled"]:
            failures.append(f"request {meta['n']}: native search was not enabled")
        if row["duplicate_client_emissions"]:
            failures.append(f"request {meta['n']}: duplicate client calls emitted by provider")
        if native:
            sid = by_case[meta["case"]]["session_id"]
            following = [r for r in requests if r["n"] > meta["n"]
                         and by_case[r["case"]]["session_id"] == sid]
            if failed_response:
                row["replay_checks"] = []
                row["durability_contract"] = "failed response: completed native observations only; no replay required"
            elif following:
                row["durability_contract"] = "successful response: exact replay, snapshot and observation"
                row["replay_checks"] = []
                for next_row in following:
                    replay = json.loads((root / f"{next_row['n']:03}-request.json").read_text())
                    check = {"request": next_row["n"], "case": next_row["case"],
                             "across_process_resume": next_row["case"] != meta["case"],
                             "equal": matching_native(replay.get("messages", []), native) == native}
                    row["replay_checks"].append(check)
                    if not check["equal"]:
                        failures.append(f"request {meta['n']}: native replay differs in request {next_row['n']}")
            else:
                row["replay_checks"] = []
                unknown.append(f"request {meta['n']}: native replay not exercised")
            snapshot = root / f"{meta['case']}-snapshot.json"
            event_file = root / "bro" / "harness-sessions" / f"{sid}.events.jsonl"
            snapshot_data = json.loads(snapshot.read_text()) if snapshot.exists() else None
            events = [json.loads(line) for line in event_file.read_text().splitlines()] if event_file.exists() else None
            expected_observation = completed_native if failed_response else native
            row["completed_native_block_count"] = len(completed_native)
            row["durable_snapshot_equal"] = None if failed_response else matching_native(snapshot_data, native) == native
            row["durable_observation_equal"] = matching_native(events, expected_observation) == expected_observation
            if row["durable_snapshot_equal"] is False or not row["durable_observation_equal"]:
                failures.append(f"request {meta['n']}: native blocks missing or changed in durable state")
        wire.append(row)
    results = []
    for case in cases:
        rows = [r for r in wire if r["case"] == case["case"]]
        native = [c for r in rows for c in r["native_calls"]]
        reports = [c["params"] for c in mcp_calls if c["case"] == case["case"] and c["success"]]
        result = dict(case, request_count=len(rows), native_call_count=len(native),
                      executed_reports=reports, duplicate_executed_reports=duplicate_calls(reports))
        if not rows or case.get("exit_code") != 0:
            unknown.append(f"{case['case']}: process did not complete successfully with wire evidence")
        outcomes = case.get("harness_results", [])
        if not outcomes or any(r.get("is_error") for r in outcomes):
            unknown.append(f"{case['case']}: successful harness result missing")
        if any(c["case"] == case["case"] and not c["success"] for c in mcp_calls):
            failures.append(f"{case['case']}: invalid MCP call executed")
        if bool(native) != case["expects_search"]:
            failures.append(f"{case['case']}: {'unexpected' if native else 'missing requested'} native search")
        if case["expects_report"] and not reports:
            failures.append(f"{case['case']}: requested report not executed")
        if len(reports) > 1:
            failures.append(f"{case['case']}: multiple reports executed")
        if case["case"] == "fresh-arithmetic" and any(r["client_calls"] for r in rows):
            failures.append("fresh-arithmetic: unexpected client tool call")
        results.append(result)
    return {"status": "fail" if failures else "inconclusive" if unknown else "pass",
            "failures": failures, "inconclusive_reasons": unknown, "cases": results, "wire": wire,
            "interpretation": "Provider-emitted client calls and locally executed MCP calls are counted separately. "
                              "Observed model names are provider claims. Native IDs, queries, replay and durable "
                              "blocks are wire observations; they do not establish quota attribution."}


def main():
    args = arguments()
    # All child-created session files inherit private permissions too.
    os.umask(0o077)
    try:
        return Probe(args).run()
    except Exception as error:
        print(json.dumps({"status": "inconclusive", "error_type": type(error).__name__}))
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
