//! Mapping a `static` route's request onto a host's asset store.

use unsworn_core::pipeline::StaticRequest;

const PATH: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'?')
    .add(b'%')
    .add(b'<')
    .add(b'>');

/// The asset paths to try for a request, in order: the file (or the
/// directory's index), the index below it when the file might be a
/// directory, then the single-page-app fallback. Paths are URL paths below
/// the asset root: `/{dir}/{file}`, percent-encoded. Empty when the request
/// tries to leave the route's directory.
pub fn candidates(r: &StaticRequest) -> Vec<String> {
    let dir = r.dir.strip_prefix("./").unwrap_or(&r.dir).trim_matches('/');
    if r.rest.split('/').any(|s| s == ".." || s == ".") || r.rest.contains('\\') {
        return Vec::new();
    }
    let at = |file: &str| {
        let file = file.trim_start_matches('/');
        let full = if dir.is_empty() || dir == "." {
            file.to_string()
        } else {
            format!("{dir}/{file}")
        };
        format!("/{}", percent_encoding::utf8_percent_encode(&full, PATH))
    };
    let mut out = Vec::new();
    let rest = r.rest.as_str();
    if rest.is_empty() || rest.ends_with('/') {
        out.push(at(&format!("{rest}{}", r.index)));
    } else {
        out.push(at(rest));
        // Without a filesystem there's no telling a directory apart, so try
        // its index next.
        out.push(at(&format!("{rest}/{}", r.index)));
    }
    if let Some(fallback) = &r.spa_fallback {
        out.push(at(fallback));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(dir: &str, rest: &str, fallback: Option<&str>) -> StaticRequest {
        StaticRequest {
            dir: dir.into(),
            rest: rest.into(),
            index: "index.html".into(),
            spa_fallback: fallback.map(String::from),
            cache_control: None,
            method: "GET".into(),
            headers: Default::default(),
        }
    }

    #[test]
    fn maps_requests_to_assets() {
        assert_eq!(
            candidates(&req("./public/", "", None)),
            ["/public/index.html"]
        );
        assert_eq!(
            candidates(&req("public", "docs/", Some("index.html"))),
            ["/public/docs/index.html", "/public/index.html"]
        );
        assert_eq!(
            candidates(&req("public", "a b.txt", None)),
            ["/public/a%20b.txt", "/public/a%20b.txt/index.html"]
        );
        assert_eq!(candidates(&req(".", "x.css", None))[0], "/x.css");
        assert!(candidates(&req("public", "../unsworn.toml", None)).is_empty());
        assert!(candidates(&req("public", "a/./b", None)).is_empty());
    }
}
