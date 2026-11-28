//! End-to-end integration tests for `[dimedb]` inside `unsworn`:
//! - Automatic MCP 2026-07-28 tool discovery (`tools/list`)
//! - At-most-once mutating MCP tools (`dimedb_create_account`, `dimedb_debit`)
//!   with retry deduplication on identical `request_id`
//! - Built-in `dimedb_lookup` and `dimedb_get_<table>` read-only MCP tools
//! - Automatic `ctx.call` function registration (`dimedb.*`)

use std::fs;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use tempfile::TempDir;

const DIME_SCHEMA: &str = r#"
[app]
name = "Wallet"
version = 1

[[tables]]
name = "accounts"
tag = 3
key = "u64"
fields = [
  { name = "balance", type = "u64" },
  { name = "credited", type = "u64" },
  { name = "debited", type = "u64" },
]

[[commands]]
name = "CreateAccount"
tag = 2
fields = [
  { name = "account", type = "u64" },
  { name = "balance", type = "u64" },
]

[[commands]]
name = "Debit"
tag = 6
fields = [
  { name = "account", type = "u64" },
  { name = "amount", type = "u64" },
]

[[replies]]
name = "Ok"
tag = 1

[[replies]]
name = "Balance"
tag = 3
fields = [{ name = "balance", type = "u64" }]

[[replies]]
name = "Insufficient"
tag = 9
fields = [{ name = "balance", type = "u64" }]
"#;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[cfg(feature = "dimedb")]
fn guest_wasm_bytes() -> &'static [u8] {
    // Pre-built from `dimedb/wasm/guest-rules` (v0.1.0) so `cargo test` needs
    // neither a sibling `dimedb` checkout nor the `wasm32-unknown-unknown` target.
    include_bytes!("fixtures/dimedb_wallet_rules.wasm")
}

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn post_mcp(url: &str, method: &str, tool: Option<&str>, mut body: Value) -> (u16, Value) {
    if let Some(params) = body.get_mut("params").and_then(Value::as_object_mut) {
        params.insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": { "name": "test-agent", "version": "1.0" },
                "io.modelcontextprotocol/clientCapabilities": {}
            }),
        );
    }
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method);
    if let Some(t) = tool {
        builder = builder.header("mcp-name", t);
    }
    let req = builder
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap();
    let resp = client.request(req).await.expect("mcp request");
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "invalid JSON ({status}): {e}: {}",
            String::from_utf8_lossy(&bytes)
        )
    });
    (status, val)
}

async fn wait_health(port: u16) {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let url = format!("http://127.0.0.1:{port}/_unsworn/health");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let req = Request::builder()
            .method(Method::GET)
            .uri(&url)
            .body(Full::new(Bytes::new()))
            .unwrap();
        if let Ok(resp) = client.request(req).await
            && resp.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("unsworn server did not become healthy on port {port}");
}

#[cfg(feature = "dimedb")]
#[tokio::test]
async fn dimedb_mcp_tools_at_most_once_and_lookup() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    fs::write(tmp.path().join("dimedb.toml"), DIME_SCHEMA).unwrap();
    fs::write(tmp.path().join("rules.wasm"), guest_wasm_bytes()).unwrap();
    fs::write(
        tmp.path().join("unsworn.toml"),
        format!(
            r#"
[server]
listen = "127.0.0.1:{port}"

[dimedb]
mode = "embedded"
schema = "dimedb.toml"
rules = "rules.wasm"
dir = "data/wallet"

[mcp]
path = "/mcp"
name = "wallet-mcp"
auth = "none"
"#
        ),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_unsworn");
    let child = Command::new(bin)
        .current_dir(tmp.path())
        .args(["serve", "--no-build"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn unsworn serve");
    let _guard = ServerGuard(child);
    wait_health(port).await;

    let mcp_url = format!("http://127.0.0.1:{port}/mcp");

    // 1. tools/list exposes commands, lookup, and table reads automatically.
    let (status, list_resp) = post_mcp(
        &mcp_url,
        "tools/list",
        None,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
    )
    .await;
    assert_eq!(status, 200, "list_resp={list_resp}");
    let tool_names: Vec<&str> = list_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        tool_names.contains(&"dimedb_create_account"),
        "{tool_names:?}"
    );
    assert!(tool_names.contains(&"dimedb_debit"), "{tool_names:?}");
    assert!(tool_names.contains(&"dimedb_lookup"), "{tool_names:?}");
    assert!(
        tool_names.contains(&"dimedb_get_accounts"),
        "{tool_names:?}"
    );

    // Helper to extract parsed tool JSON from MCP `tools/call` result.
    let parse_tool_json = |resp: &Value| -> Value {
        if let Some(sc) = resp["result"].get("structuredContent") {
            return sc.clone();
        }
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    };

    // 2. CreateAccount via MCP tool.
    let (_, create_resp) = post_mcp(
        &mcp_url,
        "tools/call",
        Some("dimedb_create_account"),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "dimedb_create_account",
                "arguments": {
                    "request_id": "01937000-0000-7000-8000-000000000001",
                    "account": 7,
                    "balance": 150
                }
            }
        }),
    )
    .await;
    assert_eq!(
        parse_tool_json(&create_resp),
        json!({ "outcome": "done", "reply": { "kind": "Ok" } })
    );

    // 3. Debit 30 via MCP tool (150 -> 120).
    let debit_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "dimedb_debit",
            "arguments": {
                "request_id": "01937000-0000-7000-8000-000000000002",
                "account": 7,
                "amount": 30
            }
        }
    });
    let (_, debit_resp) = post_mcp(
        &mcp_url,
        "tools/call",
        Some("dimedb_debit"),
        debit_req.clone(),
    )
    .await;
    assert_eq!(
        parse_tool_json(&debit_resp),
        json!({ "outcome": "done", "reply": { "kind": "Balance", "balance": 120 } })
    );

    // 4. Retry the EXACT SAME debit request_id -> must NOT double-debit!
    let (_, retry_resp) = post_mcp(&mcp_url, "tools/call", Some("dimedb_debit"), debit_req).await;
    assert_eq!(
        parse_tool_json(&retry_resp),
        json!({ "outcome": "done", "reply": { "kind": "Balance", "balance": 120 } })
    );

    // 5. Look up the applied request_id via `dimedb_lookup`.
    let (_, lookup_resp) = post_mcp(
        &mcp_url,
        "tools/call",
        Some("dimedb_lookup"),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "dimedb_lookup",
                "arguments": { "request_id": "01937000-0000-7000-8000-000000000002" }
            }
        }),
    )
    .await;
    assert_eq!(
        parse_tool_json(&lookup_resp),
        json!({ "state": "applied", "reply": { "kind": "Balance", "balance": 120 } })
    );

    // 6. Look up an unseen request_id -> not_applied, retryable: true.
    let (_, unseen_resp) = post_mcp(
        &mcp_url,
        "tools/call",
        Some("dimedb_lookup"),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "dimedb_lookup",
                "arguments": { "request_id": "01937000-0000-7000-8000-000000000099" }
            }
        }),
    )
    .await;
    assert_eq!(
        parse_tool_json(&unseen_resp),
        json!({ "state": "not_applied", "retryable": true })
    );

    // 7. Read account 7 via `dimedb_get_accounts` -> balance is 120 (not 90!).
    let (_, get_resp) = post_mcp(
        &mcp_url,
        "tools/call",
        Some("dimedb_get_accounts"),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "dimedb_get_accounts",
                "arguments": { "key": 7 }
            }
        }),
    )
    .await;
    assert_eq!(
        parse_tool_json(&get_resp),
        json!({
            "found": true,
            "row": { "balance": 120, "credited": 150, "debited": 30 }
        })
    );
}

#[tokio::test]
async fn dimedb_remote_mode_forwards_mcp_tools_over_http() {
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener as TokioListener;

    // Tiny mock remote dimeDB HTTP server.
    let listener = TokioListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = listener.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let svc = service_fn(|req: Request<hyper::body::Incoming>| async move {
                    let path = req.uri().path().to_string();
                    let body = req.into_body().collect().await.unwrap().to_bytes();
                    let input: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                    let out = match path.as_str() {
                        "/dimedb/cmd/Debit" => json!({
                            "outcome": "done",
                            "reply": { "kind": "Balance", "balance": 70 },
                            "echo_req_id": input["request_id"],
                        }),
                        "/dimedb/lookup" => json!({
                            "state": "applied",
                            "reply": { "kind": "Balance", "balance": 70 },
                        }),
                        "/dimedb/get/accounts" => json!({
                            "found": true,
                            "row": { "balance": 70, "credited": 100, "debited": 30 },
                        }),
                        _ => json!({ "error": format!("unexpected path {path}") }),
                    };
                    Ok::<_, std::convert::Infallible>(
                        http::Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from(serde_json::to_vec(&out).unwrap())))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });

    let tmp = TempDir::new().unwrap();
    let port = free_port();
    fs::write(tmp.path().join("dimedb.toml"), DIME_SCHEMA).unwrap();
    fs::write(
        tmp.path().join("unsworn.toml"),
        format!(
            r#"
[server]
listen = "127.0.0.1:{port}"

[dimedb]
mode = "remote"
schema = "dimedb.toml"
url = "http://127.0.0.1:{remote_port}"

[mcp]
path = "/mcp"
name = "wallet-remote-mcp"
auth = "none"
"#
        ),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_unsworn");
    let child = Command::new(bin)
        .current_dir(tmp.path())
        .args(["serve", "--no-build"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn unsworn serve");
    let _guard = ServerGuard(child);
    wait_health(port).await;

    let mcp_url = format!("http://127.0.0.1:{port}/mcp");
    let parse_tool_json = |resp: &Value| -> Value {
        if let Some(sc) = resp["result"].get("structuredContent") {
            return sc.clone();
        }
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    };

    let (_, resp) = post_mcp(
        &mcp_url,
        "tools/call",
        Some("dimedb_debit"),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "dimedb_debit",
                "arguments": {
                    "request_id": "01937000-0000-7000-8000-000000000077",
                    "account": 7,
                    "amount": 30
                }
            }
        }),
    )
    .await;
    let parsed = parse_tool_json(&resp);
    assert_eq!(parsed["outcome"], "done");
    assert_eq!(parsed["reply"]["balance"], 70);
    assert_eq!(
        parsed["echo_req_id"],
        "01937000-0000-7000-8000-000000000077"
    );

    let (_, lookup_resp) = post_mcp(
        &mcp_url,
        "tools/call",
        Some("dimedb_lookup"),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "dimedb_lookup",
                "arguments": { "request_id": "01937000-0000-7000-8000-000000000077" }
            }
        }),
    )
    .await;
    assert_eq!(parse_tool_json(&lookup_resp)["state"], "applied");

    server_task.abort();
}
