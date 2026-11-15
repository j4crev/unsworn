#!/usr/bin/env python3
"""Unsworn's benchmarks. See docs/benchmarks.md for what they measure.

    python3 bench/run.py build                 # build everything they run
    python3 bench/run.py run                   # all groups; writes bench/results/<date>-<cpu>.json
    python3 bench/run.py run --only proxy,policies --quick
    python3 bench/run.py report bench/results/<file>.json   # markdown tables

Needs wrk, oha (`cargo install oha`), Go, Bun, Node and Python 3, and Docker
for the Caddy, nginx and Redis rows (they're skipped without it). Only uses
the Python standard library.

On a machine with 4 or more physical cores, the load generator, the server
under test and the upstream (plus Redis) are pinned to separate cores with
taskset: the first core for load, the last for the upstream, the rest for
the server under test. Everything listens on 127.0.0.1.
"""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import hmac
import http.client
import json
import os
import platform
import random
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BENCH = REPO / "bench"
WORK = REPO / "target" / "bench"
RESULTS = BENCH / "results"
UNSWORN = Path(os.environ.get("UNSWORN_BIN", REPO / "target" / "dist" / "unsworn"))
OHA = shutil.which("oha") or str(Path.home() / ".cargo" / "bin" / "oha")

HOST = "127.0.0.1"
PORT = 8080  # the server under test
UPSTREAM = 9001
REDIS_PORT = 6390
JWT_SECRET = b"bench-secret-0123456789abcdef-0123456789"
API_KEY = "bench-key-0123456789"
ORIGIN = "https://app.example.com"
MCP_VERSION = "2026-07-28"

CADDY_IMAGE = "caddy:2-alpine"
NGINX_IMAGE = "nginx:stable-alpine"
REDIS_IMAGE = "redis:7-alpine"


# ---------------------------------------------------------------------------
# CPU pinning


def physical_cores() -> list[list[int]]:
    cores: dict[tuple[str, str], list[int]] = {}
    for cpu in sorted(os.sched_getaffinity(0)):
        topo = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology")
        try:
            key = ((topo / "physical_package_id").read_text().strip(), (topo / "core_id").read_text().strip())
        except OSError:
            key = ("0", str(cpu))
        cores.setdefault(key, []).append(cpu)
    return sorted(cores.values())


@dataclass
class Cpus:
    load: list[int]
    sut: list[int]
    aux: list[int]

    @staticmethod
    def plan(pin: bool) -> Cpus | None:
        cores = physical_cores()
        if not pin or len(cores) < 4:
            return None
        return Cpus(load=cores[0], sut=[c for core in cores[1:-1] for c in core], aux=cores[-1])


CPUS: Cpus | None = None


def pinned(cpus: list[int] | None, cmd: list[str]) -> list[str]:
    return ["taskset", "-c", ",".join(map(str, cpus)), *cmd] if cpus else cmd


# ---------------------------------------------------------------------------
# Processes


class Proc:
    def __init__(self, cmd: list[str], cpus: list[int] | None, env: dict | None = None, log: Path | None = None):
        out = open(log, "wb") if log else subprocess.DEVNULL
        self.cmd = cmd
        self.p = subprocess.Popen(
            pinned(cpus, cmd),
            cwd=REPO,
            env={**os.environ, **(env or {})},
            stdin=subprocess.DEVNULL,
            stdout=out,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )

    def alive(self) -> bool:
        return self.p.poll() is None

    def stop(self):
        if self.p.poll() is None:
            try:
                os.killpg(self.p.pid, signal.SIGTERM)
                self.p.wait(10)
            except subprocess.TimeoutExpired:
                os.killpg(self.p.pid, signal.SIGKILL)
                self.p.wait()
            except ProcessLookupError:
                pass

    def tree(self) -> list[int]:
        """The process and everything it started."""
        children: dict[int, list[int]] = {}
        for stat in Path("/proc").glob("[0-9]*/stat"):
            try:
                fields = stat.read_text().rsplit(")", 1)[1].split()
                children.setdefault(int(fields[1]), []).append(int(stat.parent.name))
            except (OSError, IndexError, ValueError):
                continue
        pids, todo = [], [self.p.pid]
        while todo:
            pid = todo.pop()
            pids.append(pid)
            todo.extend(children.get(pid, []))
        return pids

    def rss_kb(self) -> int:
        total = 0
        for pid in self.tree():
            try:
                for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                    if line.startswith("VmRSS:"):
                        total += int(line.split()[1])
            except OSError:
                pass
        return total

    def cpu_s(self) -> float:
        """CPU time used so far by the process and everything it started."""
        ticks = 0
        for pid in self.tree():
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                ticks += int(fields[11]) + int(fields[12])  # utime, stime
            except (OSError, IndexError, ValueError):
                pass
        return ticks / os.sysconf("SC_CLK_TCK")


class Container:
    def __init__(self, name: str, image: str, cpus: list[int] | None, mounts: dict[Path, str], args: list[str] = ()):
        self.name = f"unsworn-bench-{name}"
        subprocess.run(["docker", "rm", "-f", self.name], capture_output=True)
        cmd = ["docker", "run", "-d", "--rm", "--name", self.name, "--network", "host"]
        if cpus:
            cmd += ["--cpuset-cpus", ",".join(map(str, cpus))]
        for src, dst in mounts.items():
            cmd += ["-v", f"{src}:{dst}:ro"]
        cid = subprocess.run([*cmd, image, *args], check=True, capture_output=True, text=True).stdout.strip()
        self.cpu_stat = next(
            (p for p in (Path(f"/sys/fs/cgroup/system.slice/docker-{cid}.scope/cpu.stat"),) if p.exists()), None
        )

    def alive(self) -> bool:
        return True

    def cpu_s(self) -> float | None:
        if not self.cpu_stat:
            return None
        for line in self.cpu_stat.read_text().splitlines():
            if line.startswith("usage_usec "):
                return int(line.split()[1]) / 1e6
        return None

    def stop(self):
        subprocess.run(["docker", "rm", "-f", self.name], capture_output=True)

    def rss_kb(self) -> int:
        out = subprocess.run(
            ["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", self.name], capture_output=True, text=True
        ).stdout.split("/")[0].strip()
        units = {"KiB": 1, "MiB": 1024, "GiB": 1024 * 1024, "kB": 1, "MB": 1000, "B": 0}
        for unit, kb in units.items():
            if out.endswith(unit):
                return int(float(out[: -len(unit)]) * kb)
        return 0


def request(path: str, method="GET", headers=None, body: bytes | None = None, port=PORT, timeout=10.0):
    conn = http.client.HTTPConnection(HOST, port, timeout=timeout)
    try:
        conn.request(method, path, body=body, headers=headers or {})
        r = conn.getresponse()
        return r.status, r.read()
    finally:
        conn.close()


def wait_ready(path: str, port=PORT, timeout=120.0, procs=(), want=None) -> float:
    start = time.perf_counter()
    while time.perf_counter() - start < timeout:
        for p in procs:
            if not p.alive():
                raise RuntimeError(f"{' '.join(p.cmd)} exited with {p.p.returncode}")
        try:
            status, _ = request(path, port=port, timeout=2)
            if want is None or status == want:
                return time.perf_counter() - start
        except OSError:
            pass
        time.sleep(0.002)
    raise RuntimeError(f"nothing answered {path} on port {port} within {timeout}s")


def wait_port_free(port=PORT, timeout=15.0):
    import socket

    start = time.time()
    while time.time() - start < timeout:
        with socket.socket() as s:
            if s.connect_ex((HOST, port)) != 0:
                return
        time.sleep(0.05)
    raise RuntimeError(f"port {port} is still in use")


# ---------------------------------------------------------------------------
# Generated inputs: configs, request bodies, tokens


def jwt(sub="bench") -> str:
    def b64(data: bytes) -> str:
        return base64.urlsafe_b64encode(data).rstrip(b"=").decode()

    head = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    claims = b64(json.dumps({"sub": sub, "exp": int(time.time()) + 7 * 86400}).encode())
    sig = hmac.new(JWT_SECRET, f"{head}.{claims}".encode(), hashlib.sha256).digest()
    return f"{head}.{claims}.{b64(sig)}"


def mcp_body(method: str, params: dict) -> bytes:
    params = {
        **params,
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": MCP_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": {"name": "bench", "version": "1"},
        },
    }
    return json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()


def order_body() -> bytes:
    """About 1 KB of JSON that bench/projects/schemas/order.json accepts."""
    items = [{"sku": f"ABC-{1000 + i}", "quantity": i + 1, "price": 9.99 + i} for i in range(12)]
    return json.dumps(
        {
            "customer": "Ada Lovelace",
            "email": "ada@example.com",
            "note": "Leave the parcel with the neighbour if nobody is home. " * 3,
            "tags": ["priority", "gift", "returning"],
            "items": items,
        }
    ).encode()


BODIES: dict[str, bytes] = {}


def make_bodies():
    rnd = random.Random(42)
    BODIES.update(
        {
            "order": order_body(),
            "1 KB": rnd.randbytes(1024),
            "64 KB": rnd.randbytes(64 * 1024),
            "1 MB": rnd.randbytes(1024 * 1024),
            "mcp-call-text": mcp_body("tools/call", {"name": "text", "arguments": {}}),
            "mcp-call-echo": mcp_body("tools/call", {"name": "echo", "arguments": {"id": 42, "name": "widget"}}),
            "mcp-list": mcp_body("tools/list", {}),
        }
    )
    (WORK / "bodies").mkdir(parents=True, exist_ok=True)
    for name, data in BODIES.items():
        body_file(name).write_bytes(data)


def body_file(name: str) -> Path:
    return WORK / "bodies" / name.replace(" ", "")


# Handler runtimes, each running the conformance handlers for its SDK
# (docs/conformance.md): `text` answers 201 "hello" and `binary` echoes the
# request body.
RUNTIMES = {
    "bun-framed": ("TypeScript on Bun", "framed", "bun run sdk/js/src/worker.ts", "sdk/js/conformance/handlers.ts", {}),
    "bun-jsonl": ("TypeScript on Bun", "jsonl", "bun run sdk/js/src/worker.ts", "sdk/js/conformance/handlers.ts", {}),
    "node-framed": ("TypeScript on Node", "framed", "node sdk/js/src/worker.ts", "sdk/js/conformance/handlers.ts", {}),
    "python-framed": (
        "Python",
        "framed",
        "python3 -u -m unsworn.worker",
        "sdk/python/conformance/handlers.py",
        {"PYTHONPATH": "sdk/python"},
    ),
    "python-jsonl": (
        "Python",
        "jsonl",
        "python3 -u -m unsworn.worker",
        "sdk/python/conformance/handlers.py",
        {"PYTHONPATH": "sdk/python"},
    ),
    "raw-python": ("Python, no SDK", "jsonl", "python3 -u examples/raw-python/worker.py", "examples/raw-python/worker.py", {}),
    "go-framed": ("Go", "framed", "target/bench/go-worker", "sdk/go/conformance/handlers.go", {}),
    "go-jsonl": ("Go", "jsonl", "target/bench/go-worker", "sdk/go/conformance/handlers.go", {}),
    "rust-framed": ("Rust", "framed", "target/release/examples/conformance", "sdk/rust/examples/conformance.rs", {}),
    "rust-jsonl": ("Rust", "jsonl", "target/release/examples/conformance", "sdk/rust/examples/conformance.rs", {}),
}


def handlers_config(runtime: str, workers: int, eager=True) -> str:
    _, protocol, command, file, env = RUNTIMES[runtime]
    env_toml = ", ".join(f'{k} = "{v}"' for k, v in env.items())
    return f"""# Generated by bench/run.py: the {runtime} runtime with {workers} workers.
[server]
listen   = "{HOST}:{PORT}"
max_body = "16MB"

[runtimes.bench]
extensions      = ["{Path(file).suffix}"]
command         = "{command}"
protocol        = "{protocol}"
workers         = {workers}
eager           = {str(eager).lower()}
startup_timeout = "120s"
env             = {{ {env_toml} }}

[[route]]
path    = "/text"
method  = "GET"
handler = "{file}#text"
runtime = "bench"

[[route]]
path    = "/binary"
method  = "POST"
handler = "{file}#binary"
runtime = "bench"
"""


def routes_config(n: int) -> str:
    """n routes with a path parameter each, and a catch-all behind them."""
    out = [f'# Generated by bench/run.py: {n} routes.\n[server]\nlisten = "{HOST}:{PORT}"\n']
    for i in range(n):
        out.append(f'[[route]]\npath = "/api/res{i}/{{id}}"\nmethod = "GET"\nrespond = {{ status = 200, json = {{ ok = true }} }}\n')
    out.append('[[route]]\npath = "/{*rest}"\nrespond = { status = 404, json = { found = false } }\n')
    return "\n".join(out)


def write_config(name: str, text: str) -> str:
    path = WORK / "projects" / f"{name}.toml"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return str(path.relative_to(REPO))


# ---------------------------------------------------------------------------
# Servers under test, by key


def unsworn(config: str, env: dict | None = None, log: str | None = None) -> Proc:
    logfile = WORK / "logs" / f"{log}.log" if log else None
    if logfile:
        logfile.parent.mkdir(parents=True, exist_ok=True)
    return Proc(
        [str(UNSWORN), "serve", "--no-build", "-C", str(REPO), "-c", config], CPUS and CPUS.sut, env=env, log=logfile
    )


def start_server(key: str):
    """Starts the server for `key` on PORT and returns it once it answers."""
    wait_port_free()
    kind, _, rest = key.partition(":")
    if kind == "unsworn":
        name, _, arg = rest.partition(":")
        if name == "handlers":
            runtime, workers = arg.split(":")
            config = write_config(f"handlers-{runtime}-{workers}", handlers_config(runtime, int(workers)))
        elif name == "routes":
            config = write_config(f"routes-{arg}", routes_config(int(arg)))
        else:
            config = f"bench/projects/{name}.toml"
        env = {"UNSWORN_LOG": "info,access=off"} if arg == "no-access-log" else None
        proc = unsworn(config, env=env, log=key.replace(":", "-") if ARGS.logs else None)
        wait_ready("/_unsworn/health", procs=[proc], want=200)
        return proc
    if kind == "go":
        proc = Proc([str(WORK / "upstream"), f"{HOST}:{PORT}"], CPUS and CPUS.sut)
    elif kind == "bun":
        n = int(rest)
        procs = [Proc(["bun", "run", "bench/servers/bun.ts", str(PORT)], CPUS and CPUS.sut) for _ in range(n)]
        wait_ready("/", procs=procs)
        return Group(procs)
    elif kind == "node":
        proc = Proc(["node", "bench/servers/node.mjs", str(PORT), rest], CPUS and CPUS.sut)
    elif kind == "caddy":
        caddyfile = WORK / "Caddyfile"
        caddyfile.write_text(
            "{\n\tadmin off\n\tauto_https off\n}\n\n"
            f"http://{HOST}:{PORT} {{\n\treverse_proxy {HOST}:{UPSTREAM}\n}}\n"
        )
        proc = Container("caddy", CADDY_IMAGE, CPUS and CPUS.sut, {caddyfile: "/etc/caddy/Caddyfile"})
    elif kind == "nginx":
        conf = WORK / "nginx.conf"
        conf.write_text(
            "worker_processes auto;\nevents { worker_connections 4096; }\nhttp {\n  access_log off;\n"
            f"  upstream app {{ server {HOST}:{UPSTREAM}; keepalive 64; }}\n"
            f"  server {{\n    listen {HOST}:{PORT};\n    location / {{\n      proxy_pass http://app;\n"
            '      proxy_http_version 1.1;\n      proxy_set_header Connection "";\n    }\n  }\n}\n'
        )
        proc = Container("nginx", NGINX_IMAGE, CPUS and CPUS.sut, {conf: "/etc/nginx/nginx.conf"})
    else:
        raise ValueError(key)
    wait_ready("/", procs=[proc])
    return proc


class Group:
    def __init__(self, procs):
        self.procs = procs

    def stop(self):
        for p in self.procs:
            p.stop()

    def rss_kb(self):
        return sum(p.rss_kb() for p in self.procs)

    def cpu_s(self):
        return sum(p.cpu_s() for p in self.procs)


# ---------------------------------------------------------------------------
# Scenarios


@dataclass
class S:
    group: str
    name: str
    server: str
    path: str
    method: str = "GET"
    headers: dict = field(default_factory=dict)
    body: str | None = None
    expect: int = 200
    rate: int | None = 1000  # fixed-rate latency run; None skips it
    conns: int = 64
    needs: str | None = None  # "docker"


def scenarios() -> list[S]:
    bearer = {"Authorization": f"Bearer {jwt()}"}
    json_h = {"Content-Type": "application/json"}
    mcp_h = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": MCP_VERSION,
    }
    out = [
        # Answers without a handler.
        S("declarative", "Go net/http, for reference", "go", "/"),
        S("declarative", "`respond` (static JSON)", "unsworn:declarative", "/respond"),
        S("declarative", "`redirect`", "unsworn:declarative", "/redirect", expect=308),
        S("declarative", "No route (404)", "unsworn:declarative", "/nothing-here", expect=404),
        S("declarative", "Rejected: missing JWT (401)", "unsworn:declarative", "/private", expect=401),
        S("declarative", "Rejected: rate limited (429)", "unsworn:declarative", "/limited", expect=429),
        S("declarative", "Cache hit", "unsworn:declarative", "/cached"),
        S("declarative", "Same handler, uncached (Bun, Tier 2)", "unsworn:declarative", "/handler"),
        # Unsworn in front of an existing app.
        S("proxy", "Direct to the app, no proxy", "go", "/"),
        S("proxy", "Unsworn `proxy`", "unsworn:proxy", "/"),
        S("proxy", "Unsworn `proxy`, access log off", "unsworn:proxy:no-access-log", "/"),
        S("proxy", "Unsworn `proxy` with JWT and a rate limit", "unsworn:proxy", "/guarded/x", headers=bearer),
        S("proxy", "Caddy `reverse_proxy`", "caddy", "/", needs="docker"),
        S("proxy", "nginx `proxy_pass`, keepalive, access log off", "nginx", "/", needs="docker"),
        # One trivial handler, called directly and through Unsworn.
        S("handlers", "Bun on its own, 1 process", "bun:1", "/text", expect=201),
        S("handlers", "Bun on its own, 4 processes (reusePort)", "bun:4", "/text", expect=201),
        S("handlers", "Node on its own, 1 process", "node:1", "/text", expect=201),
        S("handlers", "Node on its own, 4 processes (cluster)", "node:4", "/text", expect=201),
        S("handlers", "Go on its own", "go", "/text", expect=201),
    ]
    for runtime, workers in [
        ("bun-framed", 1),
        ("bun-framed", 4),
        ("bun-jsonl", 4),
        ("node-framed", 4),
        ("python-framed", 4),
        ("python-jsonl", 4),
        ("raw-python", 4),
        ("go-framed", 4),
        ("go-jsonl", 4),
        ("rust-framed", 4),
        ("rust-jsonl", 4),
    ]:
        label, protocol, *_ = RUNTIMES[runtime]
        tier = "Tier 2" if protocol == "framed" else "Tier 1"
        out.append(
            S(
                "handlers",
                f"Unsworn → {label}, {tier}, {workers} worker{'s' if workers > 1 else ''}",
                f"unsworn:handlers:{runtime}:{workers}",
                "/text",
                expect=201,
            )
        )
    out += [
        # One policy at a time, on a static response.
        S("policies", "No policy", "unsworn:policies", "/p/none"),
        S("policies", "JWT (HS256)", "unsworn:policies", "/p/jwt", headers=bearer),
        S("policies", "API key", "unsworn:policies", "/p/key", headers={"X-API-Key": API_KEY}),
        S("policies", "Rate limit, memory store", "unsworn:policies", "/p/rate-limit"),
        S("policies", "Rate limit, Redis store", "unsworn:redis", "/p/rate-limit", needs="docker"),
        S("policies", "CORS (request with Origin)", "unsworn:policies", "/p/cors", headers={"Origin": ORIGIN}),
        S("policies", "Path and query validation", "unsworn:policies", "/p/params/42?limit=10&q=widgets"),
        S(
            "policies",
            "All of JWT, rate limit, CORS, path and query",
            "unsworn:policies",
            "/p/all/42?limit=10&q=widgets",
            headers={**bearer, "Origin": ORIGIN},
        ),
        S("policies", "`proxy`, 1 KB JSON body", "unsworn:policies", "/p/proxy", "POST", json_h, "order"),
        S("policies", "`proxy`, same body checked against a JSON Schema", "unsworn:policies", "/p/body", "POST", json_h, "order"),
        # MCP: the same handler as a route and as a tool.
        S("mcp", "`GET /text` (the route)", "unsworn:mcp", "/text", expect=201),
        S(
            "mcp",
            "`tools/call text` (the same handler)",
            "unsworn:mcp",
            "/mcp",
            "POST",
            {**mcp_h, "Mcp-Method": "tools/call", "Mcp-Name": "text"},
            "mcp-call-text",
        ),
        S(
            "mcp",
            "`tools/call echo`, arguments checked against its schema",
            "unsworn:mcp",
            "/mcp",
            "POST",
            {**mcp_h, "Mcp-Method": "tools/call", "Mcp-Name": "echo"},
            "mcp-call-echo",
        ),
        S("mcp", "`tools/list`", "unsworn:mcp", "/mcp", "POST", {**mcp_h, "Mcp-Method": "tools/list"}, "mcp-list"),
    ]
    for n in (10, 1000, 10000):
        out.append(S("routes", f"{n:,} routes", f"unsworn:routes:{n}", f"/api/res{n - 1}/42"))
    # Request bodies of several sizes, echoed back.
    octet = {"Content-Type": "application/octet-stream"}
    for size in ("1 KB", "64 KB", "1 MB"):
        conns = 16 if size == "1 MB" else 64
        for name, server, path in [
            ("Go on its own", "go", "/echo"),
            ("Unsworn `proxy` → Go", "unsworn:proxy", "/echo"),
            ("Unsworn → Bun, Tier 2", "unsworn:handlers:bun-framed:4", "/binary"),
            ("Unsworn → Bun, Tier 1", "unsworn:handlers:bun-jsonl:4", "/binary"),
            ("Unsworn → Go, Tier 2", "unsworn:handlers:go-framed:4", "/binary"),
            ("Unsworn → Go, Tier 1", "unsworn:handlers:go-jsonl:4", "/binary"),
        ]:
            out.append(S("bodies", f"{size}: {name}", server, path, "POST", octet, size, rate=None, conns=conns))
    return out


# ---------------------------------------------------------------------------
# Measuring


def check(s: S):
    body = BODIES[s.body] if s.body else None
    status = None
    for _ in range(2):  # twice, so a rate limit of 1 has kicked in
        status, data = request(s.path, s.method, s.headers, body)
    if status != s.expect:
        raise RuntimeError(f"{s.name}: expected {s.expect}, got {status}: {data[:300]!r}")
    if s.body and s.group == "bodies" and data != body:
        raise RuntimeError(f"{s.name}: the echoed body doesn't match")


def wrk(s: S, seconds: float) -> dict:
    threads = len(CPUS.load) if CPUS else 2
    cmd = ["wrk", f"-t{threads}", f"-c{s.conns}", f"-d{seconds}s", "--timeout", "10s", "-s", str(BENCH / "wrk.lua")]
    for k, v in s.headers.items():
        cmd += ["-H", f"{k}: {v}"]
    env = {**os.environ, "BENCH_METHOD": s.method, "BENCH_BODY_FILE": str(body_file(s.body)) if s.body else ""}
    out = subprocess.run(pinned(CPUS and CPUS.load, [*cmd, f"http://{HOST}:{PORT}{s.path}"]), env=env, capture_output=True, text=True)
    line = next((ln for ln in out.stdout.splitlines() if ln.startswith("BENCH_JSON ")), None)
    if not line:
        raise RuntimeError(f"wrk failed: {out.stdout}{out.stderr}")
    r = json.loads(line.removeprefix("BENCH_JSON "))
    secs = r["duration_us"] / 1e6
    expected_non2xx = not (200 <= s.expect < 400)
    return {
        "rps": r["requests"] / secs,
        "mb_per_s": (r["requests"] * len(BODIES[s.body]) / secs / 1e6) if s.body else None,
        "p50_ms": r["p50_us"] / 1000,
        "p99_ms": r["p99_us"] / 1000,
        "p999_ms": r["p999_us"] / 1000,
        "errors": r["errors"] + (0 if expected_non2xx else r["non2xx"]),
    }


def oha(s: S, seconds: float) -> dict:
    cmd = [OHA, "-z", f"{seconds}s", "-q", str(s.rate), "-c", "32", "--latency-correction", "--no-tui"]
    cmd += ["--output-format", "json", "-m", s.method]
    for k, v in s.headers.items():
        cmd += ["-H", f"{k}: {v}"]
    if s.body:
        cmd += ["-D", str(body_file(s.body))]
    out = subprocess.run(pinned(CPUS and CPUS.load, [*cmd, f"http://{HOST}:{PORT}{s.path}"]), capture_output=True, text=True)
    try:
        r = json.loads(out.stdout)
    except json.JSONDecodeError:
        raise RuntimeError(f"oha failed: {out.stdout[:500]}{out.stderr[:500]}") from None
    codes = r.get("statusCodeDistribution", {})
    p = r["latencyPercentiles"]
    return {
        "rps": r["summary"]["requestsPerSec"],
        "p50_ms": p["p50"] * 1000,
        "p99_ms": p["p99"] * 1000,
        "p999_ms": p["p99.9"] * 1000,
        "errors": sum(n for code, n in codes.items() if int(code) != s.expect)
        + sum(n for e, n in r.get("errorDistribution", {}).items() if "deadline" not in e),
    }


def measure(s: S, server) -> dict:
    check(s)
    wrk(s, ARGS.warmup)
    peak = [0]
    done = threading.Event()

    def sample():
        while not done.wait(0.5):
            peak[0] = max(peak[0], server.rss_kb())

    sampler = threading.Thread(target=sample, daemon=True)
    sampler.start()
    cpu_before = server.cpu_s()
    result = {"throughput": wrk(s, ARGS.duration)}
    cpu_after = server.cpu_s()
    done.set()
    sampler.join()
    if cpu_before is not None and cpu_after is not None:
        requests = result["throughput"]["rps"] * ARGS.duration
        result["throughput"]["cpu_us_per_req"] = (cpu_after - cpu_before) / requests * 1e6
        result["throughput"]["cores_used"] = (cpu_after - cpu_before) / ARGS.duration
    result["rss_load_kb"] = peak[0]
    if s.rate and not ARGS.no_latency:
        result["latency"] = oha(s, ARGS.duration)
    return result


# ---------------------------------------------------------------------------
# Startup, first requests and the wasm core


def startup(repeats: int) -> dict:
    out: dict = {"binary": {}, "first_request": {}, "wasm": {}}
    for n in (10, 1000, 10000):
        config = write_config(f"routes-{n}", routes_config(n))
        times, rss = [], []
        for _ in range(repeats):
            wait_port_free()
            start = time.perf_counter()
            proc = unsworn(config)
            wait_ready("/_unsworn/health", procs=[proc], want=200, timeout=60)
            times.append((time.perf_counter() - start) * 1000)
            rss.append(proc.rss_kb())
            proc.stop()
        out["binary"][str(n)] = {"ready_ms": statistics.median(times), "rss_kb": statistics.median(rss)}
        print(f"  start with {n} routes: {statistics.median(times):.1f} ms", flush=True)

    for runtime in ("bun-framed", "node-framed", "python-framed", "go-framed", "rust-framed"):
        rows = {"lazy": [], "first": [], "second": [], "eager": []}
        for _ in range(max(3, repeats // 2)):
            for eager in (False, True):
                wait_port_free()
                config = write_config(f"handlers-{runtime}-{'eager' if eager else 'lazy'}", handlers_config(runtime, 4, eager))
                start = time.perf_counter()
                proc = unsworn(config)
                ready = wait_ready("/_unsworn/health", procs=[proc], want=200)
                t = time.perf_counter()
                status, _ = request("/text")
                first = time.perf_counter() - t
                t = time.perf_counter()
                request("/text")
                second = time.perf_counter() - t
                proc.stop()
                if status != 201:
                    raise RuntimeError(f"{runtime}: first request got {status}")
                if eager:
                    rows["eager"].append(ready * 1000)
                else:
                    rows["lazy"].append(ready * 1000)
                    rows["first"].append(first * 1000)
                    rows["second"].append(second * 1000)
        out["first_request"][runtime] = {k: statistics.median(v) for k, v in rows.items()}
        print(f"  {runtime}: {out['first_request'][runtime]}", flush=True)

    for n in (1, 100, 1000):
        runs = []
        for _ in range(repeats):
            res = subprocess.run(
                pinned(CPUS and CPUS.sut, ["node", "bench/coldstart.mjs", str(n), "2000"]),
                cwd=REPO,
                capture_output=True,
                text=True,
            )
            if res.returncode != 0:
                raise RuntimeError(f"coldstart.mjs failed: {res.stderr}")
            runs.append(json.loads(res.stdout.strip().splitlines()[-1]))
        out["wasm"][str(n)] = {k: statistics.median([r[k] for r in runs]) for k in runs[0] if runs[0][k] is not None}
        print(f"  wasm, {n} routes: {out['wasm'][str(n)]}", flush=True)
    return out


def sizes() -> dict:
    wasm = REPO / "sdk" / "js" / "wasm" / "unsworn_bg.wasm"
    data = wasm.read_bytes()
    return {
        "binary_bytes": UNSWORN.stat().st_size,
        "wasm_bytes": len(data),
        "wasm_gzip_bytes": len(gzip.compress(data, 9)),
    }


# ---------------------------------------------------------------------------
# Metadata


def cmd_out(cmd: list[str]) -> str:
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30, cwd=REPO)
        return (r.stdout or r.stderr).strip().splitlines()[0]
    except (OSError, IndexError, subprocess.TimeoutExpired):
        return "unavailable"


def cpu_slug(model: str) -> str:
    """"11th Gen Intel(R) Core(TM) i5-1145G7 @ 2.60GHz" -> "intel-core-i5-1145g7"."""
    import re

    model = re.sub(r"\(R\)|\(TM\)|@.*|\bCPU\b|\d+(st|nd|rd|th) Gen", " ", model)
    return "-".join(w.lower() for w in re.findall(r"[A-Za-z0-9-]+", model)) or "unknown-cpu"


def metadata(docker: bool) -> dict:
    cpu = next((ln.split(":", 1)[1].strip() for ln in Path("/proc/cpuinfo").read_text().splitlines() if ln.startswith("model name")), "")
    governor = Path("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
    meta = {
        "date": datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
        "commit": cmd_out(["git", "rev-parse", "--short", "HEAD"]),
        "dirty": bool(subprocess.run(["git", "status", "--porcelain", "--", "crates", "sdk"], capture_output=True, text=True, cwd=REPO).stdout.strip()),
        "unsworn": cmd_out([str(UNSWORN), "--version"]),
        "cpu": cpu,
        "logical_cpus": os.cpu_count(),
        "physical_cores": len(physical_cores()),
        "governor": governor.read_text().strip() if governor.exists() else "unknown",
        "kernel": platform.release(),
        "memory_gb": round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 2**30, 1),
        "pinning": None if not CPUS else {"load": CPUS.load, "server": CPUS.sut, "upstream": CPUS.aux},
        "settings": {"duration_s": ARGS.duration, "warmup_s": ARGS.warmup, "repeats": ARGS.repeats},
        "versions": {
            "bun": cmd_out(["bun", "--version"]),
            "node": cmd_out(["node", "--version"]),
            "python": cmd_out(["python3", "--version"]),
            "go": cmd_out(["go", "version"]),
            "rustc": cmd_out(["rustc", "--version"]),
            "wrk": cmd_out(["wrk", "--version"]),
            "oha": cmd_out([OHA, "--version"]),
        },
    }
    if docker:
        meta["versions"]["caddy"] = cmd_out(["docker", "run", "--rm", CADDY_IMAGE, "caddy", "version"])
        meta["versions"]["nginx"] = cmd_out(["docker", "run", "--rm", "--entrypoint", "nginx", NGINX_IMAGE, "-v"])
        meta["versions"]["redis"] = cmd_out(["docker", "run", "--rm", REDIS_IMAGE, "redis-server", "--version"])
    return meta


# ---------------------------------------------------------------------------
# Commands


def build():
    steps = [
        ["cargo", "build", "--profile", "dist", "-p", "unsworn"],
        ["cargo", "build", "--release", "-p", "unsworn-sdk", "--example", "conformance"],
        ["go", "build", "-C", "bench/servers", "-o", str(WORK / "upstream"), "."],
        ["go", "build", "-C", "sdk/go", "-o", str(WORK / "go-worker"), "./conformance"],
        ["crates/unsworn-wasm/build.sh"],
    ]
    WORK.mkdir(parents=True, exist_ok=True)
    for step in steps:
        print("+", " ".join(step), flush=True)
        subprocess.run(step, cwd=REPO, check=True)
    if shutil.which("docker"):
        for image in (CADDY_IMAGE, NGINX_IMAGE, REDIS_IMAGE):
            subprocess.run(["docker", "pull", "-q", image], check=False)


def wait_until_idle(force: bool):
    for _ in range(60):
        load = os.getloadavg()[0]
        if load < 1.0 or force:
            return
        print(f"  waiting for the machine to settle (load {load:.2f})...", flush=True)
        time.sleep(10)
    if not force:
        sys.exit("the machine stayed busy; close what's running, or pass --force")


def run():
    global CPUS
    CPUS = Cpus.plan(not ARGS.no_pin)
    groups = ARGS.only.split(",") if ARGS.only else None
    docker = bool(shutil.which("docker")) and subprocess.run(["docker", "info"], capture_output=True).returncode == 0
    make_bodies()
    wait_until_idle(ARGS.force)

    meta = metadata(docker)
    print(json.dumps(meta, indent=2), flush=True)
    results = {"meta": meta, "runs": [], "sizes": sizes()}

    aux = CPUS and CPUS.aux
    upstream = Proc([str(WORK / "upstream"), f"{HOST}:{UPSTREAM}"], aux)
    redis = None
    try:
        wait_ready("/", port=UPSTREAM, procs=[upstream])
        if docker:
            redis = Container("redis", REDIS_IMAGE, aux, {}, ["redis-server", "--port", str(REDIS_PORT), "--save", "", "--appendonly", "no"])
            time.sleep(1)

        chosen = [s for s in scenarios() if (not groups or s.group in groups) and (s.needs != "docker" or docker)]
        for repeat in range(ARGS.repeats):
            # One server at a time, in the order they first appear.
            order: list[str] = []
            for s in chosen:
                if s.server not in order:
                    order.append(s.server)
            for key in order:
                server = start_server(key)
                try:
                    time.sleep(0.5)
                    idle = server.rss_kb()
                    for s in [s for s in chosen if s.server == key]:
                        print(f"[{repeat + 1}/{ARGS.repeats}] {s.group}: {s.name}", flush=True)
                        r = measure(s, server)
                        r.update(group=s.group, name=s.name, server=key, repeat=repeat, rss_idle_kb=idle)
                        t = r["throughput"]
                        lat = r.get("latency")
                        print(
                            f"    {t['rps']:>10,.0f} req/s  p50 {t['p50_ms']:.3f} ms  p99 {t['p99_ms']:.3f} ms"
                            + (f"  {t['cpu_us_per_req']:.1f} CPU µs/req on {t['cores_used']:.2f} cores" if "cpu_us_per_req" in t else "")
                            + (f"  | @{s.rate}/s p50 {lat['p50_ms']:.3f} p99 {lat['p99_ms']:.3f} p99.9 {lat['p999_ms']:.3f}" if lat else "")
                            + (f"  ERRORS {t['errors']}" if t["errors"] else ""),
                            flush=True,
                        )
                        results["runs"].append(r)
                finally:
                    server.stop()

        if not groups or "startup" in groups:
            print("startup", flush=True)
            results["startup"] = startup(max(ARGS.repeats * 3, 5))
    finally:
        upstream.stop()
        if redis:
            redis.stop()

    RESULTS.mkdir(parents=True, exist_ok=True)
    out = Path(ARGS.out) if ARGS.out else RESULTS / f"{datetime.now().strftime('%Y-%m-%d')}-{cpu_slug(meta['cpu'])}.json"
    out.write_text(json.dumps(results, indent=1) + "\n")
    print(f"wrote {out}")
    print(report(results))


# ---------------------------------------------------------------------------
# Report


def ms(v: float) -> str:
    return f"{v:.3f}" if v < 1 else f"{v:.2f}" if v < 100 else f"{v:.0f}"


def cpu(us: float | None) -> str:
    return "–" if us is None else f"{us:.1f}" if us < 100 else f"{us:.0f}"


def mb(kb: float) -> str:
    return f"{kb / 1024:.0f} MB" if kb else "–"


def report(results: dict) -> str:
    by: dict[tuple[str, str], list[dict]] = {}
    for r in results["runs"]:
        by.setdefault((r["group"], r["name"]), []).append(r)

    def med(rows, *path):
        vals = []
        for r in rows:
            v = r
            for p in path:
                v = v.get(p) if isinstance(v, dict) else None
            if v is not None:
                vals.append(v)
        return statistics.median(vals) if vals else None

    titles = {
        "declarative": "Answers without a handler",
        "proxy": "In front of an existing app",
        "handlers": "Handler calls",
        "policies": "Policies",
        "mcp": "MCP",
        "routes": "Route table size",
        "bodies": "Request bodies",
    }
    out = []
    for group, title in titles.items():
        rows = [(name, rs) for (g, name), rs in by.items() if g == group]
        if not rows:
            continue
        out.append(f"### {title}\n")
        if group == "bodies":
            out.append("| Scenario | Req/s | MB/s in | CPU µs/req | p50 ms | p99 ms | Peak memory |")
            out.append("|---|--:|--:|--:|--:|--:|--:|")
            for name, rs in rows:
                out.append(
                    f"| {name} | {med(rs, 'throughput', 'rps'):,.0f} | {med(rs, 'throughput', 'mb_per_s'):,.0f} "
                    f"| {cpu(med(rs, 'throughput', 'cpu_us_per_req'))} "
                    f"| {ms(med(rs, 'throughput', 'p50_ms'))} | {ms(med(rs, 'throughput', 'p99_ms'))} "
                    f"| {mb(med(rs, 'rss_load_kb'))} |"
                )
        else:
            out.append(
                "| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |"
            )
            out.append("|---|--:|--:|--:|--:|--:|--:|--:|--:|")
            for name, rs in rows:
                lat = [med(rs, "latency", k) for k in ("p50_ms", "p99_ms", "p999_ms")]
                lat_s = " | ".join(ms(v) if v is not None else "–" for v in lat)
                errs = sum(r["throughput"]["errors"] + r.get("latency", {}).get("errors", 0) for r in rs)
                out.append(
                    f"| {name}{' ⚠ ' + str(errs) + ' errors' if errs else ''} | {med(rs, 'throughput', 'rps'):,.0f} "
                    f"| {cpu(med(rs, 'throughput', 'cpu_us_per_req'))} "
                    f"| {ms(med(rs, 'throughput', 'p50_ms'))} | {ms(med(rs, 'throughput', 'p99_ms'))} | {lat_s} "
                    f"| {mb(med(rs, 'rss_load_kb'))} |"
                )
        out.append("")

    st = results.get("startup")
    if st:
        out.append("### Startup\n")
        out.append("| Routes | Start to first response, ms | Memory at idle |")
        out.append("|--:|--:|--:|")
        for n, v in st["binary"].items():
            out.append(f"| {int(n):,} | {ms(v['ready_ms'])} | {mb(v['rss_kb'])} |")
        out.append("")
        out.append("| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |")
        out.append("|---|--:|--:|--:|--:|")
        for rt, v in st["first_request"].items():
            out.append(f"| {rt} | {ms(v['lazy'])} | {ms(v['first'])} | {ms(v['second'])} | {ms(v['eager'])} |")
        out.append("")
        out.append("| Routes | Node started, ms | SDK imported, ms | First response, ms | Total, ms | Warm request, µs | First request to another route, ms |")
        out.append("|--:|--:|--:|--:|--:|--:|--:|")
        for n, v in st["wasm"].items():
            fu = v.get("first_use_ms_per_route")
            out.append(
                f"| {int(n):,} | {ms(v['node_start_ms'])} | {ms(v['import_ms'])} | {ms(v['first_request_ms'])} "
                f"| {ms(v['total_ms'])} | {v['warm_us_per_request']:.0f} | {ms(fu) if fu else '–'} |"
            )
        out.append("")
    sz = results.get("sizes")
    if sz:
        out.append(
            f"Sizes: `unsworn` binary {sz['binary_bytes'] / 2**20:.1f} MB; wasm core {sz['wasm_bytes'] / 2**20:.1f} MB, "
            f"{sz['wasm_gzip_bytes'] / 2**20:.2f} MB gzipped.\n"
        )
    return "\n".join(out)


def main():
    global ARGS
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("build")
    r = sub.add_parser("run")
    r.add_argument("--only", help="comma-separated groups: declarative, proxy, handlers, policies, mcp, routes, bodies, startup")
    r.add_argument("--duration", type=float, default=8, help="seconds per measurement (default 8)")
    r.add_argument("--warmup", type=float, default=2, help="seconds of warmup before each scenario (default 2)")
    r.add_argument("--repeats", type=int, default=3, help="runs of every scenario; the report shows the median (default 3)")
    r.add_argument("--quick", action="store_true", help="one 3-second run of each, to check the harness")
    r.add_argument("--no-latency", action="store_true", help="skip the fixed-rate latency runs")
    r.add_argument("--no-pin", action="store_true", help="don't pin processes to cores")
    r.add_argument("--logs", action="store_true", help="keep Unsworn's logs in target/bench/logs (adds disk writes)")
    r.add_argument("--force", action="store_true", help="run even when the machine looks busy")
    r.add_argument("--out", help="results file (default bench/results/<date>-<cpu>.json)")
    rep = sub.add_parser("report")
    rep.add_argument("file")
    ARGS = ap.parse_args()
    if ARGS.cmd == "build":
        build()
    elif ARGS.cmd == "run":
        if ARGS.quick:
            ARGS.duration, ARGS.warmup, ARGS.repeats = 3, 1, 1
        run()
    else:
        print(report(json.loads(Path(ARGS.file).read_text())))


if __name__ == "__main__":
    main()
