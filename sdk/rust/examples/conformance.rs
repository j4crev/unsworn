//! Conformance handlers for the Rust SDK. See docs/conformance.md.

use std::time::Duration;

use unsworn_sdk::{CallOptions, Error, Reply, Request, Result, handlers, json};

fn main() {
    unsworn_sdk::run(handlers![
        echo, text, binary, status, stream, fail, slow, crash, allow, deny, noisy, log, upload,
        content, mcp, relay, outbound
    ]);
}

async fn echo(req: Request) -> Result {
    Ok(Reply::json(json!({
        "method": req.method,
        "params": req.params,
        "query": req.query,
        "context": req.ctx,
        "json": req.json()?,
        "header": req.header("x-test"),
        "traceparent": req.header("traceparent"),
    })))
}

async fn text(_: Request) -> Result {
    Ok(Reply::text("hello")
        .with_status(201)
        .with_header("x-conformance", "yes"))
}

async fn binary(req: Request) -> Result {
    Ok(Reply::bytes(req.body.unwrap_or_default()))
}

async fn status(req: Request) -> Result {
    Ok(Reply::status(req.query_value("code")?))
}

async fn stream(_: Request) -> Result {
    Ok(Reply::stream(futures_util::stream::iter(
        ["a", "b", "c"].map(Ok::<_, Error>),
    )))
}

async fn fail(_: Request) -> Result {
    Err(Error::msg("boom"))
}

async fn slow(_: Request) -> Result {
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(Reply::text("done"))
}

async fn crash(_: Request) -> Result {
    std::process::exit(3)
}

async fn allow(_: Request) -> Result {
    Ok(Reply::next(json!({ "checked": true })))
}

async fn deny(_: Request) -> Result {
    Ok(Reply::json(json!({ "error": "denied" })).with_status(403))
}

async fn noisy(_: Request) -> Result {
    println!("this goes to stderr, not the protocol stream");
    Ok(Reply::text("ok"))
}

async fn upload(mut req: Request) -> Result {
    let stream = req
        .body_stream()
        .ok_or_else(|| Error::msg("no streamed body"))?;
    Ok(Reply::bytes(stream.bytes().await?))
}

async fn log(_: Request) -> Result {
    tracing::info!(n = 1, "conformance log");
    Ok(Reply::status(200))
}

async fn content(_: Request) -> Result {
    Ok(Reply::json(json!({ "ok": true })).with_content(json!([{ "type": "text", "text": "hi" }])))
}

async fn relay(req: Request) -> Result {
    let function: String = req.query_value("fn")?;
    let input = req.json()?.cloned().unwrap_or_default();
    let options = CallOptions {
        traceparent: req.query_value("traceparent").ok(),
        tracestate: req.query_value("tracestate").ok(),
    };
    match req
        .call_with::<unsworn_sdk::JsonValue>(&function, input, options)
        .await
    {
        Ok(answer) => Ok(Reply::json(json!({ "answer": answer }))),
        Err(e) => Ok(Reply::json(json!({ "failed": e.status }))),
    }
}

async fn outbound(req: Request) -> Result {
    let path: String = req.query_value("path")?;
    let request = unsworn_sdk::Outbound::post(path)
        .header("x-from", "handler")
        .body(req.body.clone().unwrap_or_default());
    match req.fetch("conformance", request).await {
        Ok(answer) => {
            Ok(Reply::bytes(answer.body).with_header("x-status", answer.status.to_string()))
        }
        Err(e) => Ok(Reply::json(json!({ "failed": e.status }))),
    }
}

async fn mcp(req: Request) -> Result {
    let call = req.mcp().ok_or_else(|| Error::msg("not an MCP call"))?;
    Ok(Reply::json(json!({
        "tool": call.tool,
        "protocol_version": call.protocol_version,
        "client": call.client.map(|c| c.name),
    })))
}
