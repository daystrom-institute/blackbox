#!/usr/bin/env python3
"""Deterministic local Anthropic Messages API stub. Usage: messages_stub.py <logfile> <port>
Answers POST /v1/messages (streaming SSE or plain JSON) with the text "ok"; logs each request's
tool names and message count as a JSON line; 200-empties everything else."""
import json, sys, time, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
LOG, PORT = sys.argv[1], int(sys.argv[2])
import os
DELAY = float(os.environ.get("STUB_DELAY", "0"))  # seconds to hold the FIRST model response
LOCK = threading.Lock()
def log(ev):
    with LOCK, open(LOG, "a") as f:
        f.write(json.dumps({"t": round(time.time(), 3), "src": "model", **ev}) + "\n")
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def _send(self, status, ctype, data):
        self.send_response(status); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def do_GET(self):
        log({"http": "GET", "path": self.path}); self._send(200, "application/json", b"{}")
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        try: req = json.loads(body)
        except Exception: req = {}
        if self.path.startswith("/v1/messages") and "count_tokens" not in self.path:
            tools = [t.get("name") for t in req.get("tools", []) if isinstance(t, dict)]
            log({"event": "model-request", "path": self.path, "stream": bool(req.get("stream")),
                 "model": req.get("model"), "n_messages": len(req.get("messages", [])), "tools": tools})
            msg_id = "msg_stub_0001"
            if DELAY and not getattr(H, "_delayed", False):
                H._delayed = True; log({"event": "model-hold", "seconds": DELAY}); time.sleep(DELAY)
            if req.get("stream"):
                self.send_response(200); self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache"); self.send_header("Connection", "close")
                self.close_connection = True; self.end_headers()
                evs = [("message_start", {"type": "message_start", "message": {"id": msg_id, "type": "message", "role": "assistant",
                         "model": req.get("model", "stub"), "content": [], "stop_reason": None, "stop_sequence": None,
                         "usage": {"input_tokens": 10, "output_tokens": 0}}}),
                       ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                       ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
                       ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                       ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}}),
                       ("message_stop", {"type": "message_stop"})]
                for name, ev in evs:
                    self.wfile.write(f"event: {name}\ndata: {json.dumps(ev)}\n\n".encode()); self.wfile.flush()
                return
            resp = {"id": msg_id, "type": "message", "role": "assistant", "model": req.get("model", "stub"),
                    "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn", "stop_sequence": None,
                    "usage": {"input_tokens": 10, "output_tokens": 1}}
            return self._send(200, "application/json", json.dumps(resp).encode())
        if "count_tokens" in self.path:
            log({"event": "count_tokens", "path": self.path})
            return self._send(200, "application/json", b'{"input_tokens": 10}')
        log({"http": "POST", "path": self.path})
        self._send(200, "application/json", b"{}")
ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
