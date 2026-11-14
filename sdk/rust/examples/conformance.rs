//! Conformance handlers for the Rust SDK. See docs/conformance.md.

use std::time::Duration;

use unsworn_sdk::{Error, Reply, Request, Result, handlers, json};

fn main() {
    unsworn_sdk::run(handlers![
        echo, text, binary, status, stream, fail, slow, crash, allow, deny, noisy
    ]);
}

async fn echo(req: Request) -> Result {
    Ok(Reply::json(json!({
        "method": req.method,
        "params": req.params,
        "query": req.query,
        "context": req.ctx,
        "json": req.json,
        "header": req.header("x-test"),
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
