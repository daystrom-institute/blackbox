#!/usr/bin/env python3
"""Legacy-only logging sink for Codex MCP wire probes: answers initialize (2025-06-18 style, session id), -32601 everything else. Usage: python3 codex-mcp-sink-legacy.py <logfile> <port>"""
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[1]
PORT = int(sys.argv[2])
HDRS = ("mcp-session-id", "mcp-protocol-version", "mcp-method", "last-event-id",
        "accept", "content-type", "user-agent", "origin")

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def _log(self, body):
        with open(LOG, "a") as f:
            f.write(f"--- {self.command} {self.path}\n")
            for k, v in self.headers.items():
                if k.lower() in HDRS:
                    f.write(f"    {k}: {v}\n")
            if "authorization" in {k.lower() for k in self.headers.keys()}:
                f.write("    authorization: <present>\n")
            if body:
                f.write("    BODY: " + body.decode("utf-8", "replace")[:3000] + "\n")
    def _send(self, obj, extra=None):
        data = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        self._log(body)
        try:
            msg = json.loads(body)
        except Exception:
            msg = {}
        method = msg.get("method") if isinstance(msg, dict) else None
        if method == "initialize":
            ver = msg.get("params", {}).get("protocolVersion", "2025-06-18")
            self._send({"jsonrpc": "2.0", "id": msg.get("id"), "result": {
                "protocolVersion": ver, "capabilities": {"tools": {}},
                "serverInfo": {"name": "sink", "version": "0.0.1"}}},
                {"Mcp-Session-Id": "sink-session-1"})
        elif isinstance(msg, dict) and "method" in msg and "id" in msg:
            self._send({"jsonrpc": "2.0", "id": msg.get("id"),
                        "error": {"code": -32601, "message": f"sink: method {method} logged"}})
        else:
            self.send_response(202); self.send_header("Content-Length", "0"); self.end_headers()
    def do_GET(self):
        self._log(b""); self.send_response(405); self.send_header("Content-Length", "0"); self.end_headers()
    def do_DELETE(self):
        self._log(b""); self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()
    def log_message(self, *a): pass

ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
