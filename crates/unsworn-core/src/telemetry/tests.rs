use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::*;
use crate::pipeline::{CacheStep, Rule};

const SAMPLED: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const UNSAMPLED: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";

/// Numbers that differ and spread over the whole range (SplitMix64).
fn numbers() -> impl FnMut() -> u64 {
    let mut state = 7u64;
    move || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// A recorder that keeps what it's given, with a clock that moves a
/// millisecond each time it's read.
#[derive(Clone, Default)]
struct Kept(Rc<Inner>);

#[derive(Default)]
struct Inner {
    spans: RefCell<Vec<SpanRecord>>,
    read: Cell<u64>,
    drawn: Cell<u64>,
}

impl Recorder for Kept {
    fn record(&self, span: SpanRecord) {
        self.0.spans.borrow_mut().push(span);
    }

    fn now(&self) -> Moment {
        let read = self.0.read.get() + 1;
        self.0.read.set(read);
        Moment {
            unix_nano: 1_000_000_000 + read * 1_000_000,
            monotonic: Duration::from_millis(read),
        }
    }

    fn random(&self) -> u64 {
        let drawn = self.0.drawn.get() + 1;
        self.0.drawn.set(drawn);
        drawn
    }
}

fn headers(traceparent: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(value) = traceparent {
        headers.insert(TRACEPARENT, value.parse().unwrap());
    }
    headers
}

fn decided(sample: f64, traceparent: Option<&str>) -> Option<Started> {
    decide(sample, &headers(traceparent), &mut numbers())
}

#[test]
fn an_incoming_traceparent_decides() {
    // Whatever `sample` is.
    for sample in [0.0, 1.0] {
        let s = decided(sample, Some(SAMPLED)).unwrap();
        assert_eq!(s.trace_id.hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(s.parent.unwrap().hex(), "00f067aa0ba902b7");
        assert_ne!(Some(s.span_id), s.parent);
        assert!(!s.new_root);
        assert!(decided(sample, Some(UNSAMPLED)).is_none());
    }
    // Other flags don't matter, only the sampled one.
    let flags = |f: &str| decided(0.0, Some(&format!("{}{f}", &SAMPLED[..53])));
    assert!(flags("03").is_some());
    assert!(flags("02").is_none());
}

#[test]
fn without_one_sample_decides() {
    for incoming in [None, Some("nonsense")] {
        assert!(decided(0.0, incoming).is_none());
        let s = decided(1.0, incoming).unwrap();
        assert!(s.new_root);
        assert_eq!(s.parent, None);
    }
    let mut random = numbers();
    let taken = (0..10_000)
        .filter(|_| decide(0.5, &headers(None), &mut random).is_some())
        .count();
    assert!((4_500..=5_500).contains(&taken), "{taken}");
}

#[test]
fn the_traceparent_passed_on_names_this_span() {
    let s = decided(1.0, None).unwrap();
    assert_ne!(s.trace_id.0, [0; 16]);
    assert_ne!(s.span_id.0, [0; 8]);
    let sent = s.traceparent();
    assert!(valid_traceparent(&sent), "{sent}");
    assert_eq!(
        sent,
        format!("00-{}-{}-01", s.trace_id.hex(), s.span_id.hex())
    );
}

#[test]
fn an_id_is_never_all_zeros() {
    // A trace id takes two numbers, a span id one.
    let mut numbers = [0, 0, 5, 6, 0, 7].into_iter();
    let mut random = move || numbers.next().unwrap();
    let trace_id = TraceId::random(&mut random);
    assert_eq!((trace_id.0[7], trace_id.0[15]), (5, 6));
    assert_eq!(SpanId::random(&mut random).0[7], 7);
}

#[test]
fn a_spans_name_and_path() {
    let ended = |method, path, answer| {
        let started = decided(1.0, None).unwrap();
        SpanRecord::new(
            &started,
            Ended {
                start_unix_nano: 1_000_000_000,
                elapsed: Duration::from_millis(2),
                method,
                path,
                tls: false,
                client: "127.0.0.1".parse().unwrap(),
                version: http::Version::HTTP_11,
                request_id: "r",
                answer,
            },
        )
    };
    let answered = |method, route, path| {
        let answer = Outcome {
            route,
            status: 200,
            runtime: None,
            in_handlers: Duration::ZERO,
        };
        ended(method, path, Some(answer))
    };
    let span = answered("GET", "/users/{id}", "/users/7");
    assert_eq!(span.name, "GET /users/{id}");
    assert_eq!(span.kind, Kind::Server);
    assert_eq!(span.start_unix_nano, 1_000_000_000);
    assert_eq!(span.end_unix_nano, 1_002_000_000);
    let span = answered("BREW", "(none)", "/pot");
    assert_eq!(span.name, "_OTHER");
    assert_eq!(attr(&span, "http.route"), None);
    // Cut between characters.
    let long = format!("/{}", "\u{e9}".repeat(MAX_TEXT));
    let span = answered("GET", "(none)", &long);
    let Some(Attr::Str(path)) = attr(&span, "url.path") else {
        panic!("{span:?}");
    };
    assert_eq!(path.len(), MAX_TEXT - 1);
    // Never answered: no route or status to name.
    let span = ended("GET", "/users/7", None);
    assert_eq!(span.name, "GET");
    assert_eq!(attr(&span, "http.response.status_code"), None);
    assert_eq!(attr(&span, "unsworn.in_handlers_ms"), None);
    assert_eq!(span.error.as_deref(), Some(ABANDONED));
}

fn attr<'a>(span: &'a SpanRecord, key: &str) -> Option<&'a Attr> {
    span.attributes
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, value)| value)
}

fn trace(traceparent: &str) -> TraceContext {
    TraceContext {
        traceparent: traceparent.to_string(),
        tracestate: Some("vendor=1".to_string()),
    }
}

#[test]
fn only_a_sampled_trace_is_a_parent() {
    let kept = Kept::default();
    for not in [None, Some(trace(UNSAMPLED)), Some(trace("nonsense"))] {
        assert!(Parent::from_trace(&kept, not.as_ref()).is_none(), "{not:?}");
    }
    let parent = Parent::from_trace(&kept, Some(&trace(SAMPLED))).unwrap();
    assert_eq!(parent.trace_id.hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(parent.span_id.hex(), "00f067aa0ba902b7");
}

#[test]
fn a_child_is_passed_on_and_recorded_below_its_parent() {
    let kept = Kept::default();
    let parent = Parent::from_trace(&kept, Some(&trace(SAMPLED))).unwrap();
    let child = parent.child(("GET".to_string(), Vec::new()));
    let sent = child.traceparent();
    assert_eq!(
        sent,
        format!(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-{}-01",
            child.span_id.hex()
        )
    );
    assert_ne!(child.span_id, parent.span_id);
    assert_eq!(child.context(), trace(&sent));
    let id = child.span_id;
    assert!(kept.0.spans.borrow().is_empty());
    child.answered(200);

    // Once.
    let spans = kept.0.spans.borrow();
    let [span] = spans.as_slice() else {
        panic!("{spans:?}");
    };
    assert_eq!(span.kind, Kind::Client);
    assert_eq!(span.trace_id, parent.trace_id);
    assert_eq!((span.span_id, span.parent), (id, Some(parent.span_id)));
    assert_eq!(span.name, "GET");
    assert_eq!(
        attr(span, "http.response.status_code"),
        Some(&Attr::Int(200))
    );
    assert_eq!(span.error, None);
    // A millisecond apart, by the recorder's clock.
    assert_eq!(span.end_unix_nano - span.start_unix_nano, 1_000_000);
}

#[test]
fn a_child_that_is_given_up_on_is_recorded_as_cancelled() {
    let kept = Kept::default();
    let parent = Parent::from_trace(&kept, Some(&trace(SAMPLED))).unwrap();
    drop(parent.child(("GET".to_string(), Vec::new())));
    assert_eq!(kept.0.spans.borrow()[0].error.as_deref(), Some(CANCELLED));
}

#[test]
fn steps_are_events_and_say_how_the_request_ended() {
    let steps = Steps::default();
    steps.note(Step::Authenticated("key"), 1);
    steps.note(Step::Checked, 2);
    steps.note(Step::Cache(CacheStep::Miss), 3);
    steps.note(Step::Cache(CacheStep::Stored), 4);
    steps.note(Step::Rejected(Rule::ResponseValidation), 5);
    let (events, attributes) = steps.take();
    let names: Vec<(&str, u64)> = events.iter().map(|e| (e.name, e.time_unix_nano)).collect();
    assert_eq!(
        names,
        [
            ("unsworn.authenticated", 1),
            ("unsworn.checked", 2),
            ("unsworn.cache", 3),
            ("unsworn.cache", 4),
            ("unsworn.rejected", 5),
        ]
    );
    assert_eq!(
        events[4].attributes,
        [("unsworn.rule", Attr::Static("response_validation"))]
    );
    assert!(events[1].attributes.is_empty());
    // The last of each, in the order first seen.
    assert_eq!(
        attributes,
        [
            ("unsworn.auth.scheme", Attr::Str("key".into())),
            ("unsworn.cache", Attr::Static("stored")),
            ("unsworn.rejected_by", Attr::Static("response_validation")),
        ]
    );
    assert!(steps.take().0.is_empty());
}

#[test]
fn a_span_names_an_upstream_without_its_credentials() {
    for (upstream, named) in [
        ("http://app:8080", "http://app:8080"),
        ("https://user:p%40ss@app/base", "https://app/base"),
        ("https://u:a@b@app", "https://app"),
        ("http://app/a@b?key=secret", "http://app/a@b"),
        ("unix:/run/app.sock", "unix:/run/app.sock"),
    ] {
        assert_eq!(without_credentials(upstream), named);
    }
}

#[test]
fn a_service_spans_address_and_path() {
    let (name, attributes) = service_span(
        "get",
        "billing",
        Some("https://user:secret@api.example.com/v1"),
        "/charges?key=secret",
    );
    assert_eq!(name, "GET");
    assert_eq!(
        attributes,
        [
            ("http.request.method", Attr::Static("GET")),
            ("unsworn.service", Attr::Str("billing".into())),
            ("server.address", Attr::Str("api.example.com".into())),
            ("url.path", Attr::Str("/charges".into())),
        ]
    );
}

#[test]
fn a_fetchs_trace_headers_are_read_and_taken_out() {
    let headers: BTreeMap<String, String> = [
        ("TraceParent", SAMPLED),
        ("Tracestate", "mine=1"),
        ("accept", "*/*"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .into_iter()
    .collect();
    assert_eq!(
        outbound_trace(&headers),
        Some(TraceContext {
            traceparent: SAMPLED.into(),
            tracestate: Some("mine=1".into()),
        })
    );
    let rest = strip_trace(&headers);
    assert_eq!(rest.keys().collect::<Vec<_>>(), ["accept"]);
}

#[test]
fn resource_attributes_from_the_config_and_environment() {
    let name = |configured, service, attributes| {
        resource_attributes(configured, service, attributes)[0]
            .1
            .clone()
    };
    let attributes = Some("service.name=from-attributes");
    assert_eq!(
        name(Some("configured"), Some("from-env"), attributes),
        "configured"
    );
    assert_eq!(name(None, Some("from-env"), attributes), "from-env");
    assert_eq!(name(None, Some(""), attributes), "from-attributes");
    assert_eq!(name(None, None, None), "unsworn");

    let all = resource_attributes(
        None,
        None,
        Some("team = a%2Cb%20c ,broken,=none,deployment.environment=prod,telemetry.sdk.name=x"),
    );
    let all: Vec<(&str, &str)> = all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        all,
        [
            ("service.name", "unsworn"),
            ("deployment.environment", "prod"),
            ("team", "a,b c"),
            ("telemetry.sdk.name", "unsworn"),
            ("telemetry.sdk.language", "rust"),
            ("telemetry.sdk.version", VERSION),
        ]
    );
}
