//! Helpers for running the `unsworn` binary against a temporary project.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

pub fn has(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub struct Project {
    pub dir: tempfile::TempDir,
}

impl Project {
    pub fn new(files: &[(&str, &str)]) -> Project {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
        Project { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn write(&self, path: &str, content: &str) {
        let full = self.dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    pub fn command(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_unsworn"))
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .unwrap()
    }

    pub fn serve(&self) -> Server {
        self.start("serve")
    }

    pub fn start(&self, mode: &str) -> Server {
        let port = free_port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let log = self.path().join("unsworn.log");
        let child = Command::new(env!("CARGO_BIN_EXE_unsworn"))
            .arg("-C")
            .arg(self.path())
            .arg(mode)
            .arg("--listen")
            .arg(addr.to_string())
            .env("PYTHONPATH", repo_root().join("sdk/python"))
            .env("UNSWORN_LOG", "info")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let server = Server { child, addr, log };
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(addr).is_err() {
            if Instant::now() > deadline {
                panic!("server didn't start:\n{}", server.logs());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        server
    }
}

pub struct Server {
    child: Child,
    pub addr: SocketAddr,
    log: PathBuf,
}

impl Server {
    pub fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub fn get(&self, path: &str) -> Response {
        self.request("GET", path, &[], b"")
    }

    pub fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Response {
        http(self.addr, method, path, headers, body)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }
}

/// A minimal HTTP/1.1 client: one request per connection.
pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Response {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH") {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    parse(&raw, method == "HEAD")
}

fn parse(raw: &[u8], head: bool) -> Response {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end in {:?}", String::from_utf8_lossy(raw)));
    let head_text = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head_text.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers
                .entry(k.trim().to_ascii_lowercase())
                .or_default()
                .push(v.trim().to_string());
        }
    }
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.iter().any(|v| v.contains("chunked")));
    if chunked && !head {
        body = dechunk(&body);
    }
    Response {
        status,
        headers,
        body,
    }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") {
        let size_text = String::from_utf8_lossy(&data[..line_end]);
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap().trim(), 16).unwrap_or(0);
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&data[..size.min(data.len())]);
        data = &data[(size + 2).min(data.len())..];
    }
    out
}

/// A Tier 1 Python worker that echoes the request, counts calls, and can
/// stream or act as a before handler, depending on the handler name.
pub const ECHO_WORKER: &str = r##"
import json, sys
out = sys.stdout.buffer
def send(m):
    out.write(json.dumps(m).encode() + b"\n"); out.flush()
send({"type": "ready", "protocol": 1})
count = 0
stdin = sys.stdin.buffer
while True:
    line = stdin.readline()
    if not line:
        break
    m = json.loads(line)
    raw = stdin.read(m["body_len"]) if m.get("body_len") else b""
    if m["type"] != "request":
        continue
    name = m["handler"].partition("#")[2]
    rid = m["id"]
    count += 1
    if name == "tenant":
        if m.get("headers", {}).get("x-tenant") == "blocked":
            send({"type": "response", "id": rid, "status": 403, "json": {"blocked": True}})
        else:
            send({"type": "continue", "id": rid, "context": {"tenant": m.get("headers", {}).get("x-tenant", "none")}})
    elif name == "events":
        send({"type": "response", "id": rid, "status": 200, "stream": True, "headers": {"content-type": "text/event-stream"}})
        for i in range(3):
            send({"type": "chunk", "id": rid, "text": f"data: {i}\n\n"})
        send({"type": "end", "id": rid})
    elif name == "big":
        send({"type": "response", "id": rid, "status": 200, "json": {"items": ["unsworn"] * 500}})
    else:
        send({"type": "response", "id": rid, "status": 200, "json": {
            "count": count, "handler": m["handler"], "method": m["method"], "path": m["path"],
            "params": m.get("params", {}), "query": m.get("query", {}), "context": m.get("context", {}),
            "json": m.get("json"), "raw_len": len(raw), "headers": m.get("headers", {}),
        }})
"##;
