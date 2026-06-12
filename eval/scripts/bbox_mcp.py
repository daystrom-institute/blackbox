"""Minimal MCP-over-HTTP client for eval scripts.

Speaks just enough of the streamable-HTTP MCP transport to call blackboxd
tools: initialize handshake, session header, SSE frame unwrapping, and the
lossless over-cap spill envelope (a `spilled_to` file pointer in place of
the inline payload).

Default endpoint follows the eval harness convention (run-agentic-eval.sh):
`EVAL_MCP_URL`, else the dev daemon at `BBOX_DEV_PORT` (7265). Pass
`--url http://127.0.0.1:7264/mcp` to target prod for read-only operations.
"""

import json
import os
import urllib.request

PROTOCOL = "2024-11-05"


def default_url():
    return os.environ.get(
        "EVAL_MCP_URL",
        f"http://127.0.0.1:{os.environ.get('BBOX_DEV_PORT', '7265')}/mcp",
    )


class McpClient:
    def __init__(self, url=None, timeout=120):
        self.url = url or default_url()
        self.timeout = timeout
        self.session = None
        self._rpc_id = 0
        self._initialize()

    def _post(self, payload):
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self.session:
            headers["Mcp-Session-Id"] = self.session
            headers["Mcp-Protocol-Version"] = PROTOCOL
        req = urllib.request.Request(
            self.url, data=json.dumps(payload).encode(), headers=headers, method="POST"
        )
        with urllib.request.urlopen(req, timeout=self.timeout) as resp:
            sid = resp.headers.get("Mcp-Session-Id")
            body = resp.read().decode()
        if body.lstrip().startswith("event:") or "\ndata: " in body or body.startswith("data: "):
            datas = [line[6:] for line in body.splitlines() if line.startswith("data: ")]
            body = datas[-1] if datas else body
        return (json.loads(body) if body.strip() else None), sid

    def _initialize(self):
        self._rpc_id += 1
        payload = {
            "jsonrpc": "2.0",
            "id": self._rpc_id,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "bbox-eval-script", "version": "0.1"},
            },
        }
        _, sid = self._post(payload)
        if not sid:
            raise RuntimeError(f"initialize against {self.url} returned no Mcp-Session-Id")
        self.session = sid
        try:
            self._post({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
        except Exception:
            pass

    def call_tool(self, name, args, raw=False):
        """Call an MCP tool. Returns parsed JSON (default) or raw text."""
        self._rpc_id += 1
        payload = {
            "jsonrpc": "2.0",
            "id": self._rpc_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": args},
        }
        resp, _ = self._post(payload)
        if resp is None or "result" not in resp:
            raise RuntimeError(f"{name}: bad RPC response: {resp}")
        texts = [c["text"] for c in resp["result"].get("content", []) if c.get("type") == "text"]
        if not texts:
            raise RuntimeError(f"{name}: no text content in response")
        text = texts[0]
        if raw:
            return text
        try:
            parsed = json.loads(text)
        except json.JSONDecodeError:
            # Tools surface errors as plain "Error: ..." text.
            raise RuntimeError(f"{name}: non-JSON response: {text[:300]}")
        if isinstance(parsed, dict) and parsed.get("spilled_to"):
            with open(parsed["spilled_to"]) as fh:
                parsed = json.load(fh)
        return parsed
