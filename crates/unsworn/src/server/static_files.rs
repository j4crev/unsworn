//! Static files, with an optional single-page-app fallback.

use std::path::PathBuf;

use http::{HeaderMap, HeaderValue, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;
use tower_http::services::ServeDir;

use super::support::{Body, BoxError, problem};

pub struct StaticRequest {
    pub base: PathBuf,
    pub rest: String,
    pub index: String,
    pub spa_fallback: Option<String>,
    pub cache_control: Option<String>,
    pub method: String,
    pub headers: HeaderMap,
}

/// Serves a file. Takes owned values so the future is `Send` for any
/// lifetime, as hyper requires.
pub fn serve(r: StaticRequest) -> futures_util::future::BoxFuture<'static, Response<Body>> {
    Box::pin(serve_inner(r))
}

async fn serve_inner(r: StaticRequest) -> Response<Body> {
    let StaticRequest {
        base,
        rest,
        index,
        spa_fallback,
        cache_control,
        method,
        headers,
    } = r;
    if method != "GET" && method != "HEAD" {
        return problem(
            StatusCode::METHOD_NOT_ALLOWED,
            "static files only accept GET and HEAD",
        );
    }
    let mut file = rest.clone();
    if file.is_empty() || file.ends_with('/') {
        file.push_str(&index);
    } else if base.join(&file).is_dir() {
        file = format!("{file}/{index}");
    }
    let mut resp = fetch(base.clone(), file, method.clone(), headers.clone()).await;
    if resp.status() == StatusCode::NOT_FOUND
        && let Some(fallback) = spa_fallback
    {
        resp = fetch(base, fallback, method, headers).await;
    }
    if resp.status() == StatusCode::NOT_FOUND {
        return problem(StatusCode::NOT_FOUND, format!("/{rest} not found"));
    }
    if let Some(cc) = cache_control
        && let Ok(v) = HeaderValue::from_str(&cc)
    {
        resp.headers_mut().insert(header::CACHE_CONTROL, v);
    }
    resp
}

async fn fetch(base: PathBuf, file: String, method: String, headers: HeaderMap) -> Response<Body> {
    let uri = format!(
        "/{}",
        percent_encoding::utf8_percent_encode(file.trim_start_matches('/'), PATH)
    );
    let mut builder = Request::builder().method(method.as_str()).uri(uri);
    for name in [
        header::RANGE,
        header::IF_MODIFIED_SINCE,
        header::IF_NONE_MATCH,
        header::ACCEPT_ENCODING,
    ] {
        if let Some(v) = headers.get(&name) {
            builder = builder.header(name, v.clone());
        }
    }
    let Ok(req) = builder.body(http_body_util::Empty::<bytes::Bytes>::new()) else {
        return problem(StatusCode::BAD_REQUEST, "invalid path");
    };
    let service = ServeDir::new(base).append_index_html_on_directories(false);
    match service.oneshot(req).await {
        Ok(resp) => resp.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync()),
        Err(e) => match e {},
    }
}

const PATH: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'?')
    .add(b'%')
    .add(b'<')
    .add(b'>');
