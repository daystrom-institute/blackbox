#!/usr/bin/env python3
"""Loopback MCP fixture for Claude Code wire probes.

Usage: mcp_fixture.py <logfile> <port> <mode>
  mode = modern        answer server/discover (2026-07-28), listen with SSE, emit list_changed
         modern-drop   same, but close the FIRST listen stream after the notifications (no terminal result)
         modern-bad-ack same, but send an acknowledgement with an unmatched subscription id
         modern-url-elicitation return a URL input request on the first tools/call
         legacy        reject server/discover with -32601, answer initialize at 2025-11-25 with legacy tasks,
                       echo Mcp-Session-Id, hold the GET stream open
Every request is appended to <logfile> as one JSON line.
"""
import json, sys, time, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG, PORT, MODE = sys.argv[1], int(sys.argv[2]), sys.argv[3]
import os
PV_HEADER = os.environ.get("FX_PV_HEADER") == "1"
PLAIN_SSE = os.environ.get("FX_PLAIN_SSE") == "1"
HDRS = ("mcp-session-id", "mcp-protocol-version", "mcp-method", "last-event-id",
        "accept", "content-type", "user-agent", "origin")
LOCK = threading.Lock()
STATE = {"listen_count": 0}
SERVER_INFO = {"name": "audit-fixture", "version": "0.0.2"}
META = {"io.modelcontextprotocol/serverInfo": SERVER_INFO}
CAPS_MODERN = {"tools": {"listChanged": True}, "prompts": {"listChanged": True},
               "resources": {"listChanged": True, "subscribe": True},
               "extensions": {"io.modelcontextprotocol/tasks": {"requests": {"tools": {"call": {}}}}}}
CAPS_LEGACY = {"tools": {"listChanged": True}, "prompts": {"listChanged": True},
               "resources": {"listChanged": True, "subscribe": True},
               "tasks": {"requests": {"tools": {"call": {}}}}}
TOOLS = [{"name": "fixture_echo", "description": "echoes input",
          "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]

def log(ev):
    with LOCK, open(LOG, "a") as f:
        f.write(json.dumps({"t": round(time.time(), 3), "src": "mcp", **ev}) + "\n")

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def _hdrs(self):
        h = {k.lower(): v for k, v in self.headers.items() if k.lower() in HDRS}
        if any(k.lower() == "authorization" for k in self.headers.keys()):
            h["authorization"] = "<present>"
        return h
    def _json(self, obj, extra=None, status=200):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        if PV_HEADER and MODE != "legacy": self.send_header("MCP-Protocol-Version", "2026-07-28")
        for k, v in (extra or {}).items(): self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers(); self.wfile.write(data)
    def _sse_start(self, extra=None):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        if PV_HEADER and MODE != "legacy": self.send_header("MCP-Protocol-Version", "2026-07-28")
        self.send_header("Connection", "close")
        self.close_connection = True
        for k, v in (extra or {}).items(): self.send_header(k, v)
        self.end_headers()
        self._chunk(b": open\n\n")
    def _chunk(self, raw):
        self.wfile.write(f"{len(raw):x}\r\n".encode() + raw + b"\r\n"); self.wfile.flush()
    def _chunk_end(self):
        self.wfile.write(b"0\r\n\r\n"); self.wfile.flush()
    def _sse(self, obj, eid=None):
        s = ""
        if eid is not None and not PLAIN_SSE: s += f"id: {eid}\n"
        if not PLAIN_SSE: s += "event: message\n"
        s += "data: " + json.dumps(obj) + "\n\n"
        self._chunk(s.encode())
    def do_GET(self):
        log({"http": "GET", "path": self.path, "headers": self._hdrs()})
        if MODE == "legacy":
            self._sse_start()
            try:
                for i in range(40):
                    self._chunk(b": keepalive\n\n"); time.sleep(0.5)
                self._chunk_end()
            except Exception:
                pass
            log({"event": "get_stream_closed"})
        else:
            self.send_response(405); self.send_header("Content-Length", "0"); self.end_headers()
    def do_DELETE(self):
        log({"http": "DELETE", "path": self.path, "headers": self._hdrs()})
        self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        try: msg = json.loads(body)
        except Exception: msg = {"raw": body.decode("utf-8", "replace")[:500]}
        log({"http": "POST", "path": self.path, "headers": self._hdrs(), "body": msg})
        method = msg.get("method") if isinstance(msg, dict) else None
        rid = msg.get("id") if isinstance(msg, dict) else None
        sess = {"Mcp-Session-Id": "fixture-session-1"} if MODE == "legacy" else None
        if method == "server/discover":
            if MODE == "legacy":
                return self._json({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "Method not found"}})
            return self._json({"jsonrpc": "2.0", "id": rid, "result": {
                "resultType": "complete", "supportedVersions": ["2026-07-28", "2025-11-25"],
                "capabilities": CAPS_MODERN, "serverInfo": SERVER_INFO, "instructions": "audit fixture",
                "ttlMs": 60000, "cacheScope": "private", "_meta": META}})
        if method == "initialize":
            ver = msg.get("params", {}).get("protocolVersion", "2025-11-25")
            return self._json({"jsonrpc": "2.0", "id": rid, "result": {
                "protocolVersion": ver, "capabilities": CAPS_LEGACY, "serverInfo": SERVER_INFO}}, sess)
        if method == "subscriptions/listen":
            STATE["listen_count"] += 1
            k = STATE["listen_count"]
            filt = msg.get("params", {}).get("notifications") or msg.get("params", {})
            self._sse_start()
            try:
                # Valid acknowledgements correlate the subscription id with the listen request id.
                sub_id = "fixture-unmatched-subscription" if MODE == "modern-bad-ack" else rid
                self._sse({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
                           "params": {"notifications": filt,
                                      "_meta": {"io.modelcontextprotocol/subscriptionId": sub_id}}}, 1)
                # Wait at most 10 s for the first tools/list, then delay another 1 s.
                # An unmatched ack can hold catalog startup past this bounded wait.
                for _ in range(100):
                    if STATE.get("tools_list_count", 0) >= 1: break
                    time.sleep(0.1)
                time.sleep(1.0)
                for i, m in enumerate(["notifications/tools/list_changed", "notifications/resources/list_changed",
                                       "notifications/prompts/list_changed"]):
                    self._sse({"jsonrpc": "2.0", "method": m,
                               "params": {"_meta": {"io.modelcontextprotocol/subscriptionId": sub_id}}}, 2 + i)
                    log({"event": "emitted", "method": m, "listen": k})
                if MODE == "modern-drop" and k == 1:
                    log({"event": "drop_first_listen_stream", "listen": k})
                    return  # connection closes with no terminal result
                for i in range(120):
                    self._chunk(b": keepalive\n\n"); time.sleep(0.5)
                # graceful close: terminal JSON-RPC result for the listen request
                self._sse({"jsonrpc": "2.0", "id": sub_id, "result": {"resultType": "complete",
                           "_meta": {"io.modelcontextprotocol/subscriptionId": sub_id, **META}}}, 99)
                self._chunk_end()
            except Exception:
                pass
            log({"event": "listen_stream_closed", "listen": k})
            return
        if method == "tools/list":
            STATE["tools_list_count"] = STATE.get("tools_list_count", 0) + 1
            return self._json({"jsonrpc": "2.0", "id": rid, "result": {"resultType": "complete", "tools": TOOLS,
                                                                       "ttlMs": 60000, "cacheScope": "private", "_meta": META}}, sess)
        if method in ("prompts/list", "resources/list", "resources/templates/list"):
            key = {"prompts/list": "prompts", "resources/list": "resources", "resources/templates/list": "resourceTemplates"}[method]
            return self._json({"jsonrpc": "2.0", "id": rid, "result": {"resultType": "complete", key: [],
                                                                       "ttlMs": 60000, "cacheScope": "private", "_meta": META}}, sess)
        if method == "tools/call":
            if MODE == "modern-url-elicitation" and not msg.get("params", {}).get("inputResponses"):
                result = {"resultType": "input_required", "requestState": "fixture-url-state",
                          "inputRequests": {"url-flow": {"method": "elicitation/create", "params": {
                              "mode": "url", "message": "Open the local audit flow",
                              "url": f"http://127.0.0.1:{PORT}/audit-flow"}}}, "_meta": META}
                log({"event": "url_input_required", "result": result})
                return self._json({"jsonrpc": "2.0", "id": rid, "result": result})
            if MODE == "modern-url-elicitation":
                action = msg.get("params", {}).get("inputResponses", {}).get("url-flow", {}).get("action")
                return self._json({"jsonrpc": "2.0", "id": rid, "result": {
                    "resultType": "complete", "content": [{"type": "text", "text": f"URL flow {action}"}],
                    "isError": action != "accept", "_meta": META}})
            text = (msg.get("params", {}).get("arguments") or {}).get("text", "")
            return self._json({"jsonrpc": "2.0", "id": rid, "result": {"resultType": "complete",
                               "content": [{"type": "text", "text": f"echo: {text}"}], "isError": False, "_meta": META}}, sess)
        if method == "ping":
            return self._json({"jsonrpc": "2.0", "id": rid, "result": {}}, sess)
        if rid is not None:
            return self._json({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": f"fixture: {method} logged"}}, sess)
        self.send_response(202); self.send_header("Content-Length", "0"); self.end_headers()

ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
