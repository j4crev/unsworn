//! Outbound services: what a handler's request to one is checked against
//! and turned into, the same on every host. The host sends it.

use std::collections::BTreeMap;
use std::sync::Arc;

use http::HeaderMap;

use crate::calls::declared;
use crate::project::Outbound;
use crate::trace::{self, TraceContext};

/// Headers that belong to one connection, or that the request is given
/// anyway, which a handler's `fetch` doesn't get to set.
const NOT_FORWARDED: &[&str] = &[
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// A request to a service, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// Uppercase.
    pub method: String,
    pub url: String,
    /// The handler's headers, then the service's own. The service's hold
    /// its credential.
    pub headers: Vec<(String, String)>,
}

/// The service a handler named. `Err` is the status and the reason: `404`
/// for a name the config doesn't declare.
pub fn service<'a>(
    services: &'a BTreeMap<String, Arc<Outbound>>,
    name: &str,
) -> Result<&'a Arc<Outbound>, (u16, String)> {
    services.get(name).ok_or_else(|| {
        // Name the table to add, since a handler's `fetch` doesn't say
        // where its services come from.
        (
            404,
            format!(
                "no [outbound.{name}] in the config ({})",
                declared(services.keys())
            ),
        )
    })
}

/// The request to send for a handler's `fetch`. `Err` is the status and
/// the reason: `403` for a method or path the service doesn't allow.
///
/// The headers are the request's trace (unless the handler sent its own
/// position in it, or the service's headers set one), then the handler's,
/// then the service's over them: a handler can't replace the credential,
/// or see it.
pub fn plan(
    service: &Outbound,
    method: &str,
    path: &str,
    sent: &BTreeMap<String, String>,
    trace: Option<&TraceContext>,
) -> Result<Planned, (u16, String)> {
    let method = method.to_ascii_uppercase();
    service.allows(&method, path).map_err(|why| (403, why))?;
    let sets = |name: &str| service.headers.iter().any(|(own, _)| own == name);
    let has_trace = sent
        .keys()
        .any(|name| name.eq_ignore_ascii_case(trace::TRACEPARENT))
        || sets(trace::TRACEPARENT);
    let traced = trace.filter(|_| !has_trace);
    let mut headers: Vec<(String, String)> = traced
        .iter()
        .flat_map(|t| t.headers())
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    for (name, value) in sent {
        let lower = name.to_ascii_lowercase();
        if NOT_FORWARDED.contains(&lower.as_str()) || sets(&lower) {
            continue;
        }
        // A `tracestate` belongs to the `traceparent` it came with.
        if traced.is_some() && lower == trace::TRACESTATE {
            continue;
        }
        headers.push((name.clone(), value.clone()));
    }
    headers.extend(service.headers.iter().cloned());
    Ok(Planned {
        url: format!("{}{}", service.url, path),
        method,
        headers,
    })
}

/// A response's headers as a handler gets them: lowercase names, with a
/// repeated header's values joined by `, `.
pub fn response_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        let Ok(value) = value.to_str() else { continue };
        out.entry(name.as_str().to_string())
            .and_modify(|all| {
                all.push_str(", ");
                all.push_str(value);
            })
            .or_insert_with(|| value.to_string());
    }
    out
}

/// What a handler is told when a service's answer is over `max_response`.
pub fn too_large(service: &Outbound) -> String {
    format!(
        "the response is larger than [outbound.{}].max_response",
        service.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    fn sent(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn names(planned: &Planned) -> Vec<(&str, &str)> {
        planned
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn the_services_headers_win_and_connection_headers_stay() {
        let service = Outbound::open(
            "billing",
            "https://billing.example/api/",
            vec![("authorization".into(), "Bearer real".into())],
        );
        let planned = plan(
            &service,
            "post",
            "/charges?limit=2",
            &sent(&[
                ("Authorization", "Bearer mine"),
                ("Host", "evil.example"),
                ("X-Trace", "t1"),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(planned.method, "POST");
        assert_eq!(planned.url, "https://billing.example/api/charges?limit=2");
        assert_eq!(
            names(&planned),
            [("X-Trace", "t1"), ("authorization", "Bearer real")]
        );
        assert_eq!(
            plan(&service, "GET", "/a/../b", &sent(&[]), None)
                .unwrap_err()
                .0,
            403
        );
    }

    #[test]
    fn the_requests_trace_unless_the_handler_sends_its_own() {
        let service = Outbound::open("open", "http://svc", Vec::new());
        let trace = TraceContext {
            traceparent: TRACE.into(),
            tracestate: Some("vendor=1".into()),
        };
        let with = |headers: &[(&str, &str)]| {
            plan(&service, "GET", "/", &sent(headers), Some(&trace)).unwrap()
        };
        assert_eq!(
            names(&with(&[])),
            [("traceparent", TRACE), ("tracestate", "vendor=1")]
        );
        assert_eq!(
            names(&with(&[("tracestate", "mine=1")])),
            [("traceparent", TRACE), ("tracestate", "vendor=1")]
        );
        assert_eq!(
            names(&with(&[("Traceparent", "00-own")])),
            [("Traceparent", "00-own")]
        );
        assert!(
            plan(&service, "GET", "/", &sent(&[]), None)
                .unwrap()
                .headers
                .is_empty()
        );
    }

    #[test]
    fn unknown_services_name_the_table() {
        let mut services = BTreeMap::new();
        assert_eq!(
            service(&services, "stripe").unwrap_err(),
            (
                404,
                "no [outbound.stripe] in the config (it declares none)".to_string()
            )
        );
        services.insert(
            "billing".to_string(),
            Arc::new(Outbound::open("billing", "http://b", Vec::new())),
        );
        assert!(
            service(&services, "stripe")
                .unwrap_err()
                .1
                .contains("it declares: billing")
        );
    }
}
