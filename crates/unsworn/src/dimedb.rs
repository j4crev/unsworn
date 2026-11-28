//! Native `unsworn` host backend for `[dimedb]`:
//! - `mode = "embedded"` (requires Cargo feature `dimedb`, enabled by default):
//!   in-process `dimedb_wasm::dynamic::DynamicEngine` with WAL, at-most-once
//!   `request_id`s, `lookup`, and hot-swappable Wasm rules.
//! - `mode = "remote"` (always available, even with `--no-default-features`):
//!   HTTP client forwarding commands, reads, and `lookup` to a remote dimeDB
//!   node (for multi-node, Lambda, or edge deployments).

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
#[cfg(feature = "dimedb")]
use dimedb::Options;
#[cfg(feature = "dimedb")]
use dimedb_wasm::WasmConfig;
#[cfg(feature = "dimedb")]
use dimedb_wasm::dynamic::{DynamicEngine, DynamicSchema, parse_request_id};
use http::header::{HeaderName, HeaderValue};
use http::{Method, Request};
use http_body_util::{BodyExt, Limited};
use serde_json::Value;
#[cfg(feature = "dimedb")]
use serde_json::json;
use unsworn_core::config::DimeDbMode;
use unsworn_core::dimedb::DimeDbConfig;

use crate::server::support::{HttpClient, full};

/// Live `[dimedb]` backend held by a server generation and worker pools.
pub struct DimeBackend {
    #[cfg(feature = "dimedb")]
    dir: String,
    #[cfg(feature = "dimedb")]
    schema_toml: String,
    #[cfg(feature = "dimedb")]
    lock_toml: String,
    inner: BackendInner,
}

/// Maximum size of a JSON response body accepted from a remote dimeDB server.
const MAX_REMOTE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

enum BackendInner {
    #[cfg(feature = "dimedb")]
    Embedded(Arc<DynamicEngine>),
    Remote {
        base_url: String,
        headers: Vec<(HeaderName, HeaderValue)>,
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
        if cfg.lock_dirty {
            let lock_dest = root.join(&cfg.lock_path);
            if let Err(e) = std::fs::write(&lock_dest, &cfg.lock_toml) {
                tracing::warn!(
                    path = %lock_dest.display(),
                    error = %e,
                    "failed to write dimedb.lock"
                );
            }
        }
        match cfg.mode {
            DimeDbMode::Embedded => {
                #[cfg(feature = "dimedb")]
                {
                    let rules_rel = cfg
                        .rules_path
                        .as_deref()
                        .ok_or("embedded [dimedb] requires `rules`")?;
                    let wasm_bytes = std::fs::read(root.join(rules_rel))
                        .map_err(|e| format!("can't read dimedb rules \"{rules_rel}\": {e}"))?;

                    if let Some(prev) = previous
                        && prev.dir == cfg.dir
                        && prev.schema_toml == cfg.schema_toml
                        && prev.lock_toml == cfg.lock_toml
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

                    let (schema, _, _) =
                        DynamicSchema::from_toml_with_lock(&cfg.schema_toml, Some(&cfg.lock_toml))?;
                    let mut opts = Options::default();
                    opts.limits.retry_window_ms = cfg.retry_window.map(|d| d.as_millis() as u64);
                    opts.limits.lookup_window_ms = cfg.lookup_window.map(|d| d.as_millis() as u64);
                    opts.limits.max_rows = cfg.max_rows;
                    opts.limits.max_request_ids = cfg.max_request_ids;
                    opts.disk_limit = cfg.disk_limit;
                    let wasm_cfg = WasmConfig {
                        consume_fuel: true,
                        fuel_per_check: cfg.fuel_per_check,
                        ..WasmConfig::default()
                    };
                    let eng = DynamicEngine::open(
                        &root.join(&cfg.dir),
                        schema,
                        &wasm_bytes,
                        opts,
                        wasm_cfg,
                    )?;
                    Ok(Arc::new(DimeBackend {
                        dir: cfg.dir.clone(),
                        schema_toml: cfg.schema_toml.clone(),
                        lock_toml: cfg.lock_toml.clone(),
                        inner: BackendInner::Embedded(Arc::new(eng)),
                    }))
                }
                #[cfg(not(feature = "dimedb"))]
                {
                    let _ = (root, client, previous);
                    Err("embedded [dimedb] requires the `dimedb` Cargo feature; \
                         rebuild with `--features dimedb` or set `mode = \"remote\"`"
                        .into())
                }
            }
            DimeDbMode::Remote => {
                let _ = (root, previous);
                let base_url = cfg.url.clone().ok_or("remote [dimedb] requires `url`")?;
                let mut headers = Vec::with_capacity(cfg.headers.len());
                for (k, v) in &cfg.headers {
                    let name = HeaderName::from_bytes(k.as_bytes())
                        .map_err(|e| format!("invalid [dimedb].headers name `{k}`: {e}"))?;
                    let val = HeaderValue::from_str(v)
                        .map_err(|e| format!("invalid [dimedb].headers value for `{k}`: {e}"))?;
                    headers.push((name, val));
                }
                Ok(Arc::new(DimeBackend {
                    #[cfg(feature = "dimedb")]
                    dir: cfg.dir.clone(),
                    #[cfg(feature = "dimedb")]
                    schema_toml: cfg.schema_toml.clone(),
                    #[cfg(feature = "dimedb")]
                    lock_toml: cfg.lock_toml.clone(),
                    inner: BackendInner::Remote {
                        base_url,
                        headers,
                        client,
                    },
                }))
            }
        }
    }

    /// Dispatches a virtual `dimedb` handler op (`cmd:<Name>`, `lookup`, or
    /// `get:<table_name>`) with JSON input and returns `(http_status, json)`.
    pub async fn dispatch(&self, op: &str, input: &Value) -> Result<(u16, Value), String> {
        match &self.inner {
            #[cfg(feature = "dimedb")]
            BackendInner::Embedded(eng) => dispatch_embedded(eng, op, input).await,
            BackendInner::Remote {
                base_url,
                headers,
                client,
            } => dispatch_remote(base_url, headers, client, op, input).await,
        }
    }
}

#[cfg(feature = "dimedb")]
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
    } else if let Some(table_name) = op.strip_prefix("list:") {
        let cursor = input.get("cursor");
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let page = eng.list_json(table_name, cursor, limit)?;
        Ok((200, page))
    } else if let Some(rest) = op.strip_prefix("find:") {
        let (table_name, field_name) = rest
            .split_once(':')
            .ok_or_else(|| format!("invalid dimedb find op `{op}`"))?;
        let value = input.get("value").ok_or("missing required `value`")?;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let res = eng.find_json(table_name, field_name, value, limit)?;
        Ok((200, res))
    } else {
        Err(format!("unknown dimedb op `{op}`"))
    }
}

async fn dispatch_remote(
    base_url: &str,
    headers: &[(HeaderName, HeaderValue)],
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
    } else if let Some(table) = op.strip_prefix("list:") {
        format!("{base_url}/dimedb/list/{table}")
    } else if let Some(rest) = op.strip_prefix("find:") {
        let (table, field) = rest
            .split_once(':')
            .ok_or_else(|| format!("invalid dimedb find op `{op}`"))?;
        format!("{base_url}/dimedb/find/{table}/{field}")
    } else {
        return Err(format!("unknown dimedb op `{op}`"));
    };
    let body = serde_json::to_vec(input).map_err(|e| e.to_string())?;
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(&path)
        .header("content-type", "application/json");
    for (name, val) in headers {
        builder = builder.header(name, val);
    }
    let req = builder
        .body(full(Bytes::from(body)))
        .map_err(|_| "invalid remote dimedb request".to_string())?;
    let resp = client.request(req).await.map_err(|e| {
        tracing::warn!(op, error = %e, "remote dimedb request failed");
        "remote dimedb request failed".to_string()
    })?;
    let status = resp.status().as_u16();
    let bytes = Limited::new(resp.into_body(), MAX_REMOTE_RESPONSE_BYTES)
        .collect()
        .await
        .map_err(|e| {
            tracing::warn!(op, status, error = %e, "reading remote dimedb response failed");
            "remote dimedb response unreadable or exceeded size limit".to_string()
        })?
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).map_err(|e| {
        tracing::warn!(op, status, error = %e, "invalid JSON from remote dimedb");
        "invalid JSON response from remote dimedb".to_string()
    })?;
    Ok((status, json))
}
