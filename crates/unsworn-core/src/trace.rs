//! W3C Trace Context: the `traceparent` and `tracestate` headers a request
//! arrives with, carried to what its handler asks Unsworn to do for it.
//!
//! Unsworn doesn't start traces or record spans. A request's own headers
//! reach its handlers and a `proxy` route's upstream as they came. What
//! this adds is the rest of the way: a function a handler calls and a
//! request a handler makes to an outbound service get the same headers, so
//! their spans join the trace the request belongs to.
//!
//! A request that arrives without a `traceparent` gets none made up for
//! it. One made here would carry a sampling decision Unsworn has no
//! grounds for: marked sampled, it would have every service behind it
//! record every request, and marked unsampled, none.

pub const TRACEPARENT: &str = "traceparent";
pub const TRACESTATE: &str = "tracestate";

/// The trace a request belongs to, as its headers said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    pub traceparent: String,
    /// Vendor data that travels with a `traceparent`. Never sent alone.
    pub tracestate: Option<String>,
}

impl TraceContext {
    /// From a request's headers, by lowercase name. `None` without a valid
    /// `traceparent`: receivers ignore an invalid one, so passing it on
    /// would only spread it.
    pub fn from_headers<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Self> {
        let traceparent = get(TRACEPARENT)?.trim();
        if !valid_traceparent(traceparent) {
            return None;
        }
        let tracestate = get(TRACESTATE)
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= MAX_TRACESTATE);
        Some(TraceContext {
            traceparent: traceparent.to_string(),
            tracestate: tracestate.map(str::to_string),
        })
    }

    /// The headers to send, by lowercase name.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        std::iter::once((TRACEPARENT, self.traceparent.as_str()))
            .chain(self.tracestate.as_deref().map(|s| (TRACESTATE, s)))
    }
}

/// The longest `tracestate` passed on. The specification asks for at least
/// 512 characters.
const MAX_TRACESTATE: usize = 4096;

/// Whether `value` is a `traceparent` a receiver would accept:
/// `<version>-<trace id>-<parent id>-<flags>` in lowercase hex, with ids
/// that aren't all zeros. Version `00` has nothing after the flags; a later
/// version may.
pub fn valid_traceparent(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 55 {
        return false;
    }
    let hex = |part: &[u8]| part.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let zero = |part: &[u8]| part.iter().all(|b| *b == b'0');
    let (version, trace, parent, flags) =
        (&bytes[..2], &bytes[3..35], &bytes[36..52], &bytes[53..55]);
    if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return false;
    }
    if !hex(version) || !hex(trace) || !hex(parent) || !hex(flags) {
        return false;
    }
    if version == b"ff" || zero(trace) || zero(parent) {
        return false;
    }
    match bytes.len() {
        55 => true,
        _ => version != b"00" && bytes[55] == b'-',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn traceparents() {
        assert!(valid_traceparent(OK));
        // A later version may add fields.
        assert!(valid_traceparent(
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-more"
        ));
        for bad in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-more",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-600f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0é",
        ] {
            assert!(!valid_traceparent(bad), "{bad}");
        }
    }

    #[test]
    fn contexts_from_headers() {
        let headers = |tp: &'static str, ts: Option<&'static str>| {
            move |name: &str| match name {
                "traceparent" => Some(tp),
                "tracestate" => ts,
                _ => None,
            }
        };
        let both = TraceContext::from_headers(headers(OK, Some("vendor=1"))).unwrap();
        assert_eq!(
            both.headers().collect::<Vec<_>>(),
            [("traceparent", OK), ("tracestate", "vendor=1")]
        );
        let alone = TraceContext::from_headers(headers(OK, None)).unwrap();
        assert_eq!(alone.headers().count(), 1);
        // A tracestate means nothing without the traceparent it came with.
        assert_eq!(
            TraceContext::from_headers(headers("nonsense", Some("vendor=1"))),
            None
        );
    }
}
