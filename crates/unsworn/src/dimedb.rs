//! Native `unsworn` host backend for `[dimedb]`:
//! - `mode = "embedded"`: in-process `dimedb_wasm::dynamic::DynamicEngine` with
//!   WAL, at-most-once `request_id`s, `lookup`, and hot-swappable Wasm rules.
//! - `mode = "remote"`: HTTP client forwarding commands, reads, and `lookup` to
//!   a remote dimeDB node (for multi-node, Lambda, or edge deployments).

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use dimedb::Options;
use dimedb_wasm::WasmConfig;
use dimedb_wasm::dynamic::{DynamicEngine, DynamicSchema, parse_request_id};
use http::{Method, Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use unsworn_core::config::DimeDbMode;
use unsworn_core::dimedb::DimeDbConfig;

use crate::server::support::{HttpClient, full};

/// Live `[dimedb]` backend held by a server generation and worker pools.
pub struct DimeBackend {
    dir: String,
    schema_toml: String,
    inner: BackendInner,
}

enum BackendInner {
    Embedded(Arc<DynamicEngine>),
    Remote {
        base_url: String,
        client: HttpClient,
    },
}

impl DimeBackend {
    /// Opens a new `[dimedb]` backend, or hot-swaps rules in place if `previous`
    /// is an embedded engine for the same directory and schema.
    pub fn open_or_reuse(
        cfg: &DimeDbConfig,
        root: &Path,
        client: HttpClient,
        previous: Option<&Arc<DimeBackend>>,
    ) -> Result<Arc<Self>, String> {
        match cfg.mode {
            DimeDbMode::Embedded => {
                let rules_rel = cfg
                    .rules_path
                    .as_deref()
                    .ok_or("embedded [dimedb] requires `rules`")?;
                let wasm_bytes = std::fs::read(root.join(rules_rel))
                    .map_err(|e| format!("can't read dimedb rules \"{rules_rel}\": {e}"))?;

                if let Some(prev) = previous
                    && prev.dir == cfg.dir
                    && prev.schema_toml == cfg.schema_toml
                    && let BackendInner::Embedded(eng) = &prev.inner
                {
                    let stats = eng.hot_swap(&wasm_bytes)?;
                    tracing::info!(
                        compile_us = stats.compile_us,
                        instantiate_us = stats.instantiate_us,
                        "hot-swapped dimedb Wasm rules"
                    );
                    return Ok(prev.clone());
                }

                let schema = DynamicSchema::from_toml(&cfg.schema_toml)?;
                let mut opts = Options::default();
                opts.limits.retry_window_ms = cfg.retry_window.map(|d| d.as_millis() as u64);
                opts.limits.lookup_window_ms = cfg.lookup_window.map(|d| d.as_millis() as u64);
                let wasm_cfg = WasmConfig {
                    consume_fuel: true,
                    fuel_per_check: cfg.fuel_per_check,
                };
                let eng = DynamicEngine::open(&root.join(&cfg.dir), schema, &wasm_bytes, opts, wasm_cfg)?;
                Ok(Arc::new(DimeBackend {
                    dir: cfg.dir.clone(),
                    schema_toml: cfg.schema_toml.clone(),
                    inner: BackendInner::Embedded(Arc::new(eng)),
                }))
            }
            DimeDbMode::Remote => {
                let base_url = cfg
                    .url
                    .clone()
                    .ok_or("remote [dimedb] requires `url`")?;
                Ok(Arc::new(DimeBackend {
                    dir: cfg.dir.clone(),
                    schema_toml: cfg.schema_toml.clone(),
                    inner: BackendInner::Remote { base_url, client },
                }))
            }
        }
    }

    /// Dispatches a virtual `dimedb` handler op (`cmd:<Name>`, `lookup`, or
    /// `get:<table_name>`) with JSON input and returns `(http_status, json)`.
    pub async fn dispatch(&self, op: &str, input: &Value) -> Result<(u16, Value), String> {
        match &self.inner {
            BackendInner::Embedded(eng) => dispatch_embedded(eng, op, input).await,
            BackendInner::Remote { base_url, client } => {
                dispatch_remote(base_url, client, op, input).await
            }
        }
    }
}

async fn dispatch_embedded(
    eng: &DynamicEngine,
    op: &str,
    input: &Value,
) -> Result<(u16, Value), String> {
    if let Some(cmd_name) = op.strip_prefix("cmd:") {
        let req_id_val = input
            .get("request_id")
            .ok_or("missing required `request_id`")?;
        let req_id = parse_request_id(req_id_val)?;
        let res = eng.call_json_async(cmd_name, req_id, input).await?;
        Ok((200, res))
    } else if op == "lookup" {
        let req_id_val = input
            .get("request_id")
            .ok_or("missing required `request_id`")?;
        let req_id = parse_request_id(req_id_val)?;
        let res = eng.lookup_json_async(req_id).await?;
        Ok((200, res))
    } else if let Some(table_name) = op.strip_prefix("get:") {
        let key = input.get("key").ok_or("missing required `key`")?;
        let row = eng.get_json(table_name, key)?;
        Ok((
            200,
            match row {
                Some(r) => json!({ "found": true, "row": r }),
                None => json!({ "found": false, "row": null }),
            },
        ))
    } else {
        Err(format!("unknown dimedb op `{op}`"))
    }
}

async fn dispatch_remote(
    base_url: &str,
    client: &HttpClient,
    op: &str,
    input: &Value,
) -> Result<(u16, Value), String> {
    let path = if let Some(cmd) = op.strip_prefix("cmd:") {
        format!("{base_url}/dimedb/cmd/{cmd}")
    } else if op == "lookup" {
        format!("{base_url}/dimedb/lookup")
    } else if let Some(table) = op.strip_prefix("get:") {
        format!("{base_url}/dimedb/get/{table}")
    } else {
        return Err(format!("unknown dimedb op `{op}`"));
    };
    let body = serde_json::to_vec(input).map_err(|e| e.to_string())?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(&path)
        .header("content-type", "application/json")
        .body(full(Bytes::from(body)))
        .map_err(|e| e.to_string())?;
    let resp = client
        .request(req)
        .await
        .map_err(|e| format!("remote dimedb request failed: {e}"))?;
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("reading remote dimedb response: {e}"))?
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("invalid JSON from remote dimedb: {e}"))?;
    Ok((status, json))
}
