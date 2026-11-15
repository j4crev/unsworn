//! Response compression: brotli, zstd or gzip, picked from `Accept-Encoding`.

use futures_util::StreamExt;
use http::{HeaderValue, Method, Response, StatusCode, header};
use http_body_util::{BodyDataStream, BodyExt, StreamBody};
use hyper::body::Frame;
use tokio_util::io::{ReaderStream, StreamReader};

use super::support::{Body, BoxError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Br,
    Zstd,
    Gzip,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Encoding::Br => "br",
            Encoding::Zstd => "zstd",
            Encoding::Gzip => "gzip",
        }
    }
}

/// Picks the best encoding the client accepts, preferring br, then zstd,
/// then gzip. `*` stands for every encoding the header doesn't name
/// (RFC 9110 section 12.5.3).
fn choose(accept: &str) -> Option<Encoding> {
    // In order of preference.
    let mut named = [
        (Encoding::Br, None),
        (Encoding::Zstd, None),
        (Encoding::Gzip, None),
    ];
    let mut wildcard: Option<f32> = None;
    for part in accept.split(',') {
        let mut pieces = part.split(';');
        let name = pieces.next().unwrap_or("").trim().to_ascii_lowercase();
        let q = pieces
            .find_map(|p| {
                let p = p.trim().to_ascii_lowercase();
                p.strip_prefix("q=")
                    .and_then(|q| q.trim().parse::<f32>().ok())
            })
            .unwrap_or(1.0);
        let enc = match name.as_str() {
            "*" => {
                wildcard = Some(q);
                continue;
            }
            "br" => Encoding::Br,
            "zstd" => Encoding::Zstd,
            "gzip" | "x-gzip" => Encoding::Gzip,
            _ => continue,
        };
        if let Some(slot) = named.iter_mut().find(|(e, _)| *e == enc) {
            slot.1 = Some(q);
        }
    }
    let mut best: Option<(Encoding, f32)> = None;
    for (enc, q) in named {
        let q = q.or(wildcard).unwrap_or(0.0);
        if q > 0.0 && best.is_none_or(|(_, bq)| q > bq) {
            best = Some((enc, q));
        }
    }
    best.map(|(e, _)| e)
}

/// Whether `Cache-Control` has `no-transform`, which forbids an
/// intermediary from changing the content's coding (RFC 9110 section 7.7).
fn no_transform(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(header::CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|d| d.trim().eq_ignore_ascii_case("no-transform"))
}

fn compressible(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    if ct.starts_with("text/event-stream") {
        return false;
    }
    ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("javascript")
        || ct.contains("xml")
        || ct.starts_with("image/svg")
        || ct.starts_with("application/wasm")
}

const MIN_SIZE: u64 = 1024;

pub fn maybe_compress(
    method: &Method,
    accept: Option<&HeaderValue>,
    resp: Response<Body>,
) -> Response<Body> {
    let Some(encoding) = accept.and_then(|a| a.to_str().ok()).and_then(choose) else {
        return resp;
    };
    let status = resp.status();
    if method == Method::HEAD
        || status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
    {
        return resp;
    }
    let headers = resp.headers();
    if headers.contains_key(header::CONTENT_ENCODING)
        || headers.contains_key(header::CONTENT_RANGE)
        || no_transform(headers)
    {
        return resp;
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !compressible(content_type) {
        return resp;
    }
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && len < MIN_SIZE
    {
        return resp;
    }
    if http_body::Body::size_hint(resp.body())
        .exact()
        .is_some_and(|n| n < MIN_SIZE)
    {
        return resp;
    }

    let (mut parts, body) = resp.into_parts();
    let data = BodyDataStream::new(body).map(|r| r.map_err(std::io::Error::other));
    let reader = StreamReader::new(data);
    let compressed: Body = match encoding {
        Encoding::Br => wrap(async_compression::tokio::bufread::BrotliEncoder::new(
            reader,
        )),
        Encoding::Zstd => wrap(async_compression::tokio::bufread::ZstdEncoder::new(reader)),
        Encoding::Gzip => wrap(async_compression::tokio::bufread::GzipEncoder::new(reader)),
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    // A strong ETag names these exact bytes, and they're about to change.
    // A weak one may cover every coding of the content (RFC 9110 section
    // 8.8.3.3), and still answers `If-None-Match`.
    if let Some(etag) = parts.headers.get(header::ETAG)
        && !etag.as_bytes().starts_with(b"W/")
        && let Ok(weak) = HeaderValue::from_bytes(&[b"W/", etag.as_bytes()].concat())
    {
        parts.headers.insert(header::ETAG, weak);
    }
    parts.headers.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(encoding.name()),
    );
    parts
        .headers
        .append(header::VARY, HeaderValue::from_static("accept-encoding"));
    Response::from_parts(parts, compressed)
}

fn wrap<R: tokio::io::AsyncRead + Send + 'static>(reader: R) -> Body {
    let stream = ReaderStream::new(reader).map(|r| match r {
        Ok(bytes) => Ok(Frame::data(bytes)),
        Err(e) => Err(BoxError::from(e)),
    });
    StreamBody::new(stream).boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(headers: &[(&str, &str)]) -> Response<Body> {
        let mut resp = Response::new(super::super::support::full(vec![b'a'; 4096]));
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        for (k, v) in headers {
            resp.headers_mut().insert(
                http::HeaderName::try_from(*k).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        resp
    }

    #[test]
    fn compressing_changes_the_validator_and_respects_no_transform() {
        let gzip = HeaderValue::from_static("gzip");
        let go =
            |headers: &[(&str, &str)]| maybe_compress(&Method::GET, Some(&gzip), response(headers));
        let r = go(&[("etag", "\"abc\"")]);
        assert_eq!(r.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(r.headers()[header::ETAG], "W/\"abc\"");
        let r = go(&[("etag", "W/\"abc\"")]);
        assert_eq!(r.headers()[header::ETAG], "W/\"abc\"");

        let r = go(&[
            ("etag", "\"abc\""),
            ("cache-control", "max-age=60, No-Transform"),
        ]);
        assert!(!r.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(r.headers()[header::ETAG], "\"abc\"");
    }

    #[test]
    fn picks_encodings() {
        assert_eq!(choose("gzip, deflate, br"), Some(Encoding::Br));
        assert_eq!(choose("gzip;q=1.0, br;q=0.5"), Some(Encoding::Gzip));
        assert_eq!(choose("identity"), None);
        assert_eq!(choose("br;q=0, gzip"), Some(Encoding::Gzip));
        assert_eq!(choose("*"), Some(Encoding::Br));
        assert_eq!(choose("br;q=0, *"), Some(Encoding::Zstd));
        assert_eq!(choose("gzip;q=0.5, *;q=0.1"), Some(Encoding::Gzip));
        assert_eq!(choose("*;q=0"), None);
        assert_eq!(choose("*;q=0, gzip"), Some(Encoding::Gzip));
        assert_eq!(choose("GZIP; Q=0.8"), Some(Encoding::Gzip));
        assert!(compressible("application/json"));
        assert!(!compressible("text/event-stream"));
        assert!(!compressible("image/png"));
    }
}
