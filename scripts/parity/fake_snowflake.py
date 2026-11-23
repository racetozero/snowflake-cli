#!/usr/bin/env python3
"""A tiny fake Snowflake REST endpoint used for differential parity checks.

Both the Python reference `snow` and the Rust port connect to this server with
`--host 127.0.0.1 --port N --protocol http`. Every SQL text received is appended
to the JSON-lines log named by FAKE_SF_LOG. Responses come from a rules file
(FAKE_SF_RULES, JSON list of {"match": regex, "columns": [...], "rows": [[...]],
"error": {...}}). The first matching rule wins; otherwise a one-row
"status" result is returned.
"""
import gzip
import json
import os
import re
import sys
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = os.environ.get("FAKE_SF_LOG")
RULES_PATH = os.environ.get("FAKE_SF_RULES")
_lock = threading.Lock()
_counter = [0]


RESULTS = {}


def next_query_id():
    with _lock:
        _counter[0] += 1
        return "01000000-0000-0000-0000-%012d" % _counter[0]


_USED = set()


def load_rules():
    """Case rules; a rule with `"once": true` applies to its first match only."""
    if RULES_PATH and os.path.exists(RULES_PATH):
        with open(RULES_PATH) as f:
            rules = json.load(f)
        return [dict(r, _idx=i) for i, r in enumerate(rules) if i not in _USED]
    return []


def consume(rule):
    if rule and rule.get("once"):
        _USED.add(rule["_idx"])


def record(entry):
    if not LOG:
        return
    with _lock, open(LOG, "a") as f:
        f.write(json.dumps(entry, sort_keys=True) + "\n")


def col(name, typ="text", **extra):
    base = {"name": name, "database": "", "schema": "", "table": "", "nullable": True,
            "type": typ, "length": None, "precision": None, "scale": None, "byteLength": None,
            "collation": None}
    if typ == "fixed":
        base.update(precision=38, scale=0)
    if typ == "text":
        base.update(length=16777216, byteLength=16777216)
    base.update(extra)
    return base


def to_cell(v):
    if v is None or v == "<NULL>":
        return None
    if isinstance(v, bool):
        return "1" if v else "0"
    return str(v)


SESSION = {"database": None, "schema": None, "warehouse": None, "role": "SYSADMIN"}


STAGES = os.environ.get("FAKE_SF_STAGES", "")
_TRANSFER = re.compile(
    r"^\s*(PUT|GET)\s+('(?:[^']|'')*'|\S+)\s+('(?:[^']|'')*'|\S+)(.*)$", re.IGNORECASE | re.DOTALL)


def _unq(t):
    return t[1:-1].replace("''", "'") if t.startswith("'") and t.endswith("'") else t


def stage_dir(stage_path):
    """`@db.s.stage/a/b` -> <STAGES>/db.s.stage/a/b (stage names case-folded unless quoted)."""
    p = stage_path[1:] if stage_path.startswith("@") else stage_path
    if p.lower().startswith("snow://"):
        p = p[7:]
    name, _, rest = p.partition("/")
    if not name.startswith('"'):
        name = name.lower().split(".")[-1]
    return os.path.join(STAGES, name.replace('"', ""), rest)


def file_transfer(sql, qid):
    m = _TRANSFER.match(sql)
    if not m or not STAGES:
        return None
    kind, a, b, opts = m.group(1).upper(), _unq(m.group(2)), _unq(m.group(3)), m.group(4)
    o = {k.lower(): _unq(v) for k, v in re.findall(r"(\w+)\s*=\s*('(?:[^']|'')*'|\S+)", opts)}
    data = {"queryId": qid, "parallel": int(o.get("parallel", 4)), "overwrite": o.get("overwrite", "false").lower() == "true",
            "encryptionMaterial": None, "presignedUrls": None, "statementTypeId": 0}
    if kind == "PUT":
        local = a[7:] if a.lower().startswith("file://") else a
        loc = stage_dir(b)
        os.makedirs(loc, exist_ok=True)
        data.update(command="UPLOAD", src_locations=[local],
                    autoCompress=o.get("auto_compress", "true").lower() == "true",
                    sourceCompression=o.get("source_compression", "auto_detect"),
                    stageInfo={"locationType": "LOCAL_FS", "location": loc.rstrip("/") + "/", "path": "", "creds": {},
                               "region": None, "endPoint": None, "isClientSideEncrypted": False})
    else:
        local = b[7:] if b.lower().startswith("file://") else b
        base = stage_dir(a)
        root = base if os.path.isdir(base) else os.path.dirname(base)
        files = []
        if os.path.isfile(base):
            files = [os.path.basename(base)]
        elif os.path.isdir(base):
            for dp, _, fs in os.walk(base):
                for f in sorted(fs):
                    files.append(os.path.relpath(os.path.join(dp, f), base))
        pat = o.get("pattern")
        if pat:
            files = [f for f in files if re.fullmatch(pat, f)]
        data.update(command="DOWNLOAD", src_locations=sorted(files), localLocation=local,
                    stageInfo={"locationType": "LOCAL_FS", "location": root.rstrip("/") + "/", "path": "", "creds": {},
                               "region": None, "endPoint": None, "isClientSideEncrypted": False})
    record({"kind": "transfer", "command": data["command"], "src": [os.path.basename(x) for x in data["src_locations"]]})
    return {"success": True, "code": None, "message": None, "data": data}


# snowflake.core probes the server version on first use (Root()).
BUILTIN_RULES = [
    {"match": r"^SELECT SYSTEM\$CLIENT_VERSION_INFO\(\)$", "columns": ["SYSTEM$CLIENT_VERSION_INFO()"], "rows": [["[]"]]},
]


def stage_listing(sql):
    """`ls @stage/path` against the fake stage directory."""
    m = re.match(r"^\s*(?:ls|list)\s+('(?:[^']|'')*'|\S+)(.*)$", sql, re.IGNORECASE | re.DOTALL)
    if not m or not STAGES:
        return None
    target = _unq(m.group(1))
    base = stage_dir(target)
    stage_root = stage_dir("@" + (target[1:] if target.startswith("@") else target).split("/")[0])
    rows = []
    walk_root = base if os.path.isdir(base) else os.path.dirname(base)
    for dp, _, fs in sorted(os.walk(walk_root)):
        for f in sorted(fs):
            full = os.path.join(dp, f)
            if not full.startswith(base.rstrip("/")):
                continue
            rel = os.path.relpath(full, os.path.dirname(stage_root.rstrip("/")))
            data = open(full, "rb").read()
            import hashlib
            rows.append([rel, len(data), hashlib.md5(data).hexdigest(), "Mon, 01 Jan 2024 00:00:00 GMT"])
    return {"columns": ["name", {"name": "size", "type": "fixed"}, "md5", "last_modified"], "rows": rows}


def stage_copy(sql):
    """`copy files into @dst from @src` between fake stage directories."""
    m = re.match(r"^\s*copy\s+files\s+into\s+('(?:[^']|'')*'|\S+)\s+from\s+('(?:[^']|'')*'|\S+)", sql, re.IGNORECASE)
    if not m or not STAGES:
        return None
    import shutil
    dst, src = stage_dir(_unq(m.group(1))), stage_dir(_unq(m.group(2)))
    rows = []
    if os.path.isdir(src):
        for dp, _, fs in os.walk(src):
            for f in sorted(fs):
                rel = os.path.relpath(os.path.join(dp, f), src)
                os.makedirs(os.path.dirname(os.path.join(dst, rel)), exist_ok=True)
                shutil.copyfile(os.path.join(dp, f), os.path.join(dst, rel))
                rows.append([rel])
    return {"columns": ["file"], "rows": rows}


def result_for(sql):
    for rule in [*[r for r in load_rules() if "match" in r], *BUILTIN_RULES]:
        if re.search(rule["match"], sql, re.IGNORECASE | re.DOTALL):
            consume(rule)
            return rule
    listing = stage_listing(sql) or stage_copy(sql)
    if listing is not None:
        return listing
    return None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b""
        if self.headers.get("Content-Encoding") == "gzip":
            raw = gzip.decompress(raw)
        try:
            return json.loads(raw or b"{}")
        except Exception:
            return {}

    def _send(self, obj, status=200):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def rest(self, method, body):
        record({"kind": "rest", "method": method, "path": self.path, "body": body})
        for rule in load_rules():
            if "rest" in rule and re.search(rule["rest"], f"{method} {self.path}"):
                if "sse" in rule:
                    data = "".join(f"data: {e}\n\n" for e in rule["sse"]).encode()
                    self.send_response(rule.get("status", 200))
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    return
                return self._send(rule.get("body", {}), rule.get("status", 200))
        return self._send({"message": "not found"}, 404)

    def do_DELETE(self):
        return self.rest("DELETE", self._body())

    def do_PUT(self):
        return self.rest("PUT", self._body())

    def _websocket(self):
        """A WebSocket log stream: handshake, read the client's request frame,
        send each `ws` rule entry as a protobuf LogEntry, then close."""
        import base64, hashlib, struct
        key = self.headers.get("Sec-WebSocket-Key", "")
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.wfile.flush()

        def read_frame():
            h = self.rfile.read(2)
            if len(h) < 2:
                return None, b""
            n = h[1] & 0x7F
            if n == 126:
                n = struct.unpack(">H", self.rfile.read(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self.rfile.read(8))[0]
            mask = self.rfile.read(4) if h[1] & 0x80 else b"\0\0\0\0"
            data = bytes(b ^ mask[i % 4] for i, b in enumerate(self.rfile.read(n)))
            return h[0] & 0x0F, data

        def varint(v):
            out = b""
            while True:
                b = v & 0x7F
                v >>= 7
                if v:
                    out += bytes([b | 0x80])
                else:
                    return out + bytes([b])

        def frame(op, payload):
            n = len(payload)
            head = bytes([0x80 | op])
            if n < 126:
                head += bytes([n])
            elif n < 65536:
                head += bytes([126]) + struct.pack(">H", n)
            else:
                head += bytes([127]) + struct.pack(">Q", n)
            return head + payload

        op, data = read_frame()
        record({"kind": "ws", "path": self.path, "auth": self.headers.get("Authorization"), "request": data.hex()})
        entries = []
        for rule in load_rules():
            if "ws" in rule:
                entries = rule["ws"]
        for e in entries:
            if isinstance(e, str):
                payload = bytes.fromhex(e)
            else:
                ts = varint(0x08) + varint(e.get("seconds", 0)) if e.get("seconds") else b""
                if e.get("nanos"):
                    ts += varint(0x10) + varint(e["nanos"])
                payload = b""
                if e.get("source"):
                    payload += varint(0x08) + varint(e["source"])
                c = e.get("content", "").encode()
                if c:
                    payload += varint(0x12) + varint(len(c)) + c
                if "seconds" in e:
                    payload += varint(0x1A) + varint(len(ts)) + ts
                if e.get("sequence"):
                    payload += varint(0x20) + varint(e["sequence"])
                if e.get("level"):
                    payload += varint(0x28) + varint(e["level"])
            self.wfile.write(frame(0x2, payload))
        self.wfile.write(frame(0x8, struct.pack(">H", 1000)))
        self.wfile.flush()
        read_frame()
        self.close_connection = True

    def do_GET(self):
        if self.headers.get("Upgrade", "").lower() == "websocket":
            return self._websocket()
        if self.path.startswith("/api/"):
            return self.rest("GET", None)
        for rule in load_rules():
            if "rest" in rule and re.search(rule["rest"], f"GET {self.path}"):
                return self.rest("GET", None)
        return self.do_POST()

    def do_POST(self):
        path = self.path
        body = self._body()
        if path.startswith("/api/"):
            return self.rest(self.command, body)
        if path.startswith("/session/v1/login-request"):
            d = body.get("data", {})
            record({"kind": "login", "user": d.get("LOGIN_NAME"), "account": d.get("ACCOUNT_NAME"),
                    "authenticator": d.get("AUTHENTICATOR")})
            if d.get("PASSWORD") == "wrong":
                return self._send({"success": False, "code": "390100",
                                   "message": "Incorrect username or password was specified.",
                                   "data": {"nextAction": "RETRY_LOGIN", "authnMethod": "USERNAME_PASSWORD"}})
            from urllib.parse import urlparse, parse_qs
            q = parse_qs(urlparse(path).query)
            SESSION["database"] = (q.get("databaseName") or [None])[0]
            SESSION["schema"] = (q.get("schemaName") or [None])[0]
            SESSION["warehouse"] = (q.get("warehouse") or [None])[0]
            SESSION["role"] = (q.get("roleName") or ["SYSADMIN"])[0]
            return self._send({"success": True, "code": None, "message": None, "data": {
                "token": "fake-token", "masterToken": "fake-master", "validityInSeconds": 3600,
                "masterValidityInSeconds": 14400, "sessionId": 1, "displayUserName": d.get("LOGIN_NAME"),
                "serverVersion": "9.0.0", "parameters": [
                    {"name": "TIMEZONE", "value": "UTC"},
                    {"name": "CLIENT_SESSION_KEEP_ALIVE", "value": False}],
                "sessionInfo": {"databaseName": SESSION["database"], "schemaName": SESSION["schema"],
                                "warehouseName": SESSION["warehouse"], "roleName": SESSION["role"]}}})
        if path.startswith("/queries/v1/query-request"):
            sql = body.get("sqlText", "")
            rec = {"kind": "query", "sql": sql}
            if body.get("bindings"):
                rec["bindings"] = {k: v.get("value") for k, v in body["bindings"].items()}
            record(rec)
            qid = next_query_id()
            transfer = file_transfer(sql, qid)
            if transfer is not None:
                return self._send(transfer)
            rule = result_for(sql)
            if rule and rule.get("error"):
                e = rule["error"]
                return self._send({"success": False, "code": str(e.get("code", "002003")),
                                   "message": e.get("message", "error"),
                                   "data": {"queryId": qid, "sqlState": e.get("sqlState", "02000"),
                                            "errorCode": str(e.get("code", "002003"))}})
            if rule:
                cols = [col(c) if isinstance(c, str) else col(**c) for c in rule.get("columns", [])]
                port = str(self.server.server_address[1])
                rows = [[to_cell(v.replace("<SELF_PORT>", port) if isinstance(v, str) else v) for v in r] for r in rule.get("rows", [])]
            else:
                cols = [col("status")]
                rows = [["Statement executed successfully."]]
            data = {
                "parameters": [], "rowtype": cols, "rowset": rows, "total": len(rows), "returned": len(rows),
                "queryId": qid, "queryResultFormat": "json", "statementTypeId": 4096,
                "finalDatabaseName": SESSION["database"], "finalSchemaName": SESSION["schema"],
                "finalWarehouseName": SESSION["warehouse"], "finalRoleName": SESSION["role"]}
            # Kept for `/queries/<qid>/result` (async execution, ServerPoll).
            RESULTS[qid] = data
            return self._send({"success": True, "code": None, "message": None, "data": data})
        if path.startswith("/session/token-request"):
            record({"kind": "token", "type": body.get("requestType")})
            return self._send({"success": True, "data": {"sessionToken": "fake-session-token",
                               "validityInSecondsST": 3600, "masterToken": "fake-master"}})
        if path.startswith("/session"):
            return self._send({"success": True, "code": None, "message": None, "data": None})
        if path.startswith("/telemetry"):
            record({"kind": "telemetry"})
            for log in body.get("logs", []) if isinstance(body, dict) else []:
                m = log.get("message") if isinstance(log, dict) else None
                if isinstance(m, dict) and m.get("type") == "cli_user_feedback":
                    keep = ("message", "category", "rating", "truncated", "last_error_id", "last_error_type")
                    record({"kind": "feedback", **{k: m[k] for k in keep if k in m}})
            return self._send({"success": True, "data": None})
        m = re.match(r"^/monitoring/queries/([0-9a-f-]+)", path)
        if m:
            record({"kind": "monitor", "qid": m.group(1)})
            return self._send({"success": True, "data": {"queries": [{"id": m.group(1), "status": "SUCCESS"}]}})
        m = re.match(r"^/queries/([0-9a-f-]+)/result", path)
        if m and m.group(1) in RESULTS:
            record({"kind": "result", "qid": m.group(1)})
            return self._send({"success": True, "code": None, "message": None, "data": RESULTS[m.group(1)]})
        record({"kind": "unknown", "path": path})
        return self._send({"success": True, "data": None})



def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(srv.server_address[1], flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
