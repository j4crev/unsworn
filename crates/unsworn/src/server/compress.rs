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

/// Picks the best encoding the client accepts, preferring br, then zstd, then gzip.
fn choose(accept: &str) -> Option<Encoding> {
    let mut best: Option<(Encoding, f32)> = None;
    for part in accept.split(',') {
        let mut pieces = part.trim().split(';');
        let name = pieces.next().unwrap_or("").trim().to_ascii_lowercase();
        let q = pieces
            .find_map(|p| {
                p.trim()
                    .strip_prefix("q=")
                    .and_then(|q| q.parse::<f32>().ok())
            })
            .unwrap_or(1.0);
        if q <= 0.0 {
            continue;
        }
        let enc = match name.as_str() {
            "br" => Encoding::Br,
            "zstd" => Encoding::Zstd,
            "gzip" | "x-gzip" => Encoding::Gzip,
            _ => continue,
        };
        let rank = |e: Encoding| match e {
            Encoding::Br => 3,
            Encoding::Zstd => 2,
            Encoding::Gzip => 1,
        };
        let better = match best {
            None => true,
            Some((b, bq)) => q > bq || (q == bq && rank(enc) > rank(b)),
        };
        if better {
            best = Some((enc, q));
        }
    }
    best.map(|(e, _)| e)
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
    if headers.contains_key(header::CONTENT_ENCODING) || headers.contains_key(header::CONTENT_RANGE)
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

    #[test]
    fn picks_encodings() {
        assert_eq!(choose("gzip, deflate, br"), Some(Encoding::Br));
        assert_eq!(choose("gzip;q=1.0, br;q=0.5"), Some(Encoding::Gzip));
        assert_eq!(choose("identity"), None);
        assert_eq!(choose("br;q=0, gzip"), Some(Encoding::Gzip));
        assert!(compressible("application/json"));
        assert!(!compressible("text/event-stream"));
        assert!(!compressible("image/png"));
    }
}
