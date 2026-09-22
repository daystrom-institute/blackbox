#!/usr/bin/env python3
"""Probe Codex HTTP MCP negotiation using loopback sinks and a Responses stub.

Usage: python3 codex-audit-matrix.py [--binary /path/to/codex]
Writes raw evidence to a fresh temporary directory. No real inference is used.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def parse_wire(path):
    rows = []
    for line in path.read_text().splitlines() if path.exists() else []:
        if line.startswith("--- "):
            method, route = line[4:].split(" ", 1)
            rows.append({"http": method, "path": route, "headers": {}})
        elif line.startswith("    BODY: "):
            rows[-1]["body"] = json.loads(line[10:])
        elif line.startswith("    "):
            key, value = line.strip().split(": ", 1)
            rows[-1]["headers"][key.lower()] = value
    return rows


def run_case(binary, root, name, modern, flag, call):
    case = root / name
    case.mkdir()
    observations = []

    class Model(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            payload = b'{"models":[]}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            tools = request.get("tools", [])
            observations.append({"path": self.path, "tools": tools,
                                 "input": request.get("input", [])})
            (case / "model.json").write_text(json.dumps(observations, indent=2))
            response_id = "response-probe-" + str(len(observations))
            if call and len(observations) == 1:
                item = {"type": "tool_search_call", "call_id": "probe-search",
                        "execution": "client", "arguments": {"query": "sink_echo"}}
            elif call and len(observations) == 2:
                item = {"type": "function_call", "call_id": "probe-call",
                        "namespace": "mcp__sink", "name": "sink_echo",
                        "arguments": '{"text":"hello"}'}
            else:
                item = {"type": "message", "role": "assistant", "id": "probe-message",
                        "content": [{"type": "output_text", "text": "done"}]}
            events = [
                {"type": "response.created", "response": {"id": response_id}},
                {"type": "response.output_item.done", "item": item},
                {"type": "response.completed", "response": {"id": response_id,
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}},
            ]
            payload = "".join("event: " + ev["type"] + "\ndata: " + json.dumps(ev) + "\n\n"
                              for ev in events).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=model.serve_forever, daemon=True).start()
    port = free_port()
    fixture = Path(__file__).with_name("codex-mcp-sink-" + ("modern" if modern else "legacy") + ".py")
    wire_path = case / "wire.log"
    sink = subprocess.Popen(["python3", str(fixture), str(wire_path), str(port)],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.1):
                    break
            except OSError:
                if sink.poll() is not None:
                    raise RuntimeError("sink exited during startup")
                time.sleep(.05)
        else:
            raise RuntimeError("sink startup timed out")
        config = {
            "model": '"gpt-5.5"',
            "model_provider": '"probe"',
            "model_providers.probe": '{name="probe",base_url="http://127.0.0.1:'
                + str(model.server_port) + '/v1",wire_api="responses",requires_openai_auth=false}',
            "mcp_servers.sink.url": json.dumps(f"http://127.0.0.1:{port}/mcp"),
            "project_doc_max_bytes": "0",
            "features.code_mode": "false",
            "features.codex_apps": "false",
            "features.enable_request_compression": "false",
            "analytics.enabled": "false",
        }
        if flag is not None:
            config["features.mcp_2026_07_28"] = str(flag).lower()
        command = [binary, "exec", "--skip-git-repo-check", "--ignore-user-config",
                   "--ignore-rules", "--ephemeral", "--json", "-s", "danger-full-access",
                   "-C", str(case)]
        for key, value in config.items():
            command.extend(["-c", key + "=" + value])
        command.append("Call sink_echo with text hello, then say done." if call else "Say done.")
        (case / "command.json").write_text(json.dumps(command, indent=2))
        env = os.environ.copy()
        for key in ("OPENAI_API_KEY", "OPENAI_BASE_URL", "ANTHROPIC_API_KEY"):
            env.pop(key, None)
        with (case / "stdout.jsonl").open("w") as out, (case / "stderr.log").open("w") as err:
            proc = subprocess.run(command, env=env, stdout=out, stderr=err, timeout=60)
        rows = parse_wire(wire_path)
        result = {"case": name, "exit_code": proc.returncode,
                  "model_requests": len(observations), "wire": rows}
        (case / "result.json").write_text(json.dumps(result, indent=2))
        methods = [r.get("body", {}).get("method", r["http"]) for r in rows]
        checks = {"cli_exit_zero": proc.returncode == 0,
                  "local_model_used": bool(observations),
                  "tools_list_observed": "tools/list" in methods}
        if flag:
            checks["discover_first"] = methods[:1] == ["server/discover"]
            if modern:
                checks["stateless"] = not any(m in methods for m in
                    ("initialize", "GET", "DELETE")) and all(
                        "mcp-session-id" not in row["headers"] for row in rows)
                checks["modern_request_metadata"] = all(
                    row["headers"].get("mcp-method") == row["body"]["method"] and
                    row["headers"].get("mcp-protocol-version") == "2026-07-28" and
                    row["body"].get("params", {}).get("_meta", {}).get(
                        "io.modelcontextprotocol/protocolVersion") == "2026-07-28" and
                    all(isinstance(row["body"]["params"]["_meta"].get(key), dict)
                        for key in ("io.modelcontextprotocol/clientInfo",
                                    "io.modelcontextprotocol/clientCapabilities"))
                    for row in rows if "body" in row)
            else:
                checks["fallback"] = "initialize" in methods
        else:
            checks["legacy_default"] = methods[:1] == ["initialize"] and "server/discover" not in methods
        if call:
            checks["tools_call_observed"] = methods.count("tools/call") == 1
            checks["echo_consumed"] = any(
                item.get("type") == "function_call_output" and
                "echo: hello" in json.dumps(item.get("output", ""))
                for observation in observations for item in observation["input"])
        result["checks"] = checks
        (case / "result.json").write_text(json.dumps(result, indent=2))
        print(json.dumps({"case": name, "exit_code": proc.returncode,
                          "model_requests": len(observations), "methods": methods,
                          "checks": checks}), flush=True)
        return result
    finally:
        sink.terminate()
        try:
            sink.wait(timeout=5)
        except subprocess.TimeoutExpired:
            sink.kill()
            sink.wait()
        model.shutdown()
        model.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="codex")
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="codex-mcp-audit-"))
    print(str(root), flush=True)
    results = []
    for name, modern, flag, call in [
        ("A-default-legacy", False, None, False),
        ("B-modern-fallback", False, True, False),
        ("C-modern-tool-call", True, True, True),
        ("D-default-modern-server", True, None, True),
    ]:
        results.append(run_case(args.binary, root, name, modern, flag, call))
    (root / "results.json").write_text(json.dumps(results, indent=2))
    if any(not all(r["checks"].values()) for r in results):
        raise SystemExit(1)
