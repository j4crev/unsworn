//! Comparing a route's answer with its shadow's.
//!
//! A route with `shadow` has a second handler that gets a copy of each
//! request. The client is answered by the route's own handler; the
//! shadow's answer is only compared with it. That's how a handler
//! rewritten in another language is tried on real requests before it
//! takes over. The core decides which requests to copy and what "the same
//! answer" means; the host runs the second call and keeps count.

use bytes::Bytes;
use serde_json::Value;

use super::host::{Body, Deadline};
use crate::policy::HandlerRef;

/// A handler's answer, as far as it's compared.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub status: u16,
    /// `None` for a streamed body, which isn't compared.
    pub body: Option<Bytes>,
}

impl Answer {
    /// What a handler answered: the bytes the client gets, wherever the
    /// host left them. A host that calls handlers in its own process keeps
    /// JSON and text in the response itself.
    pub fn of<B>(response: &unsworn_proto::Response, body: &Body<B>) -> Answer {
        let body = match (body, &response.json, &response.text) {
            (Body::Empty, Some(json), _) => Some(json.to_bytes()),
            (Body::Empty, None, Some(text)) => Some(Bytes::from(text.clone())),
            (Body::Empty, None, None) => Some(Bytes::new()),
            (Body::Bytes(bytes), _, _) => Some(bytes.clone()),
            (Body::Host(_), _, _) => None,
        };
        Answer {
            status: response.status,
            body,
        }
    }
}

/// A request to send to a route's shadow handler, with the answer the
/// route's own handler gave to the same request.
pub struct ShadowCall {
    /// The route's label, as in the access log.
    pub route: String,
    pub method: String,
    pub handler: HandlerRef,
    pub request: unsworn_proto::Request,
    /// The request's raw body, if it had one that isn't in `request`.
    pub body: Option<Bytes>,
    pub deadline: Deadline,
    pub primary: Answer,
    /// How long the route's own handler took.
    pub primary_time: std::time::Duration,
}

/// Whether two answers are the same, and if not, where they first differ.
pub fn compare(primary: &Answer, shadow: &Answer) -> Result<(), String> {
    if primary.status != shadow.status {
        return Err(format!(
            "status {} from the handler, {} from its shadow",
            primary.status, shadow.status
        ));
    }
    let (Some(a), Some(b)) = (&primary.body, &shadow.body) else {
        return Ok(());
    };
    if a == b {
        return Ok(());
    }
    // As JSON, where two languages write one value in different ways:
    // another order of keys, `1.0` for `1`, other spacing.
    let json = |bytes: &Bytes| serde_json::from_slice::<Value>(bytes).ok();
    match (json(a), json(b)) {
        (Some(a), Some(b)) => match difference(&a, &b, "$") {
            Some(at) => Err(at),
            None => Ok(()),
        },
        _ => Err(format!(
            "the bodies differ ({} and {} bytes)",
            a.len(),
            b.len()
        )),
    }
}

fn short(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= 60 {
        return text;
    }
    let cut: String = text.chars().take(57).collect();
    format!("{cut}...")
}

/// Where two JSON values first differ, as a path and the two values.
fn difference(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (key, value) in x {
                let at = format!("{path}.{key}");
                match y.get(key) {
                    Some(other) => {
                        if let Some(found) = difference(value, other, &at) {
                            return Some(found);
                        }
                    }
                    None => return Some(format!("{at} is missing from the shadow's answer")),
                }
            }
            y.keys()
                .find(|key| !x.contains_key(*key))
                .map(|key| format!("{path}.{key} is only in the shadow's answer"))
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!(
                    "{path} has {} items, and {} in the shadow's answer",
                    x.len(),
                    y.len()
                ));
            }
            x.iter()
                .zip(y)
                .enumerate()
                .find_map(|(i, (v, w))| difference(v, w, &format!("{path}[{i}]")))
        }
        // `1` and `1.0` are one number.
        (Value::Number(x), Value::Number(y)) if x.as_f64() == y.as_f64() => None,
        _ if a == b => None,
        _ => Some(format!(
            "{path} is {}, and {} in the shadow's answer",
            short(a),
            short(b)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(status: u16, body: &str) -> Answer {
        Answer {
            status,
            body: Some(Bytes::copy_from_slice(body.as_bytes())),
        }
    }

    #[test]
    fn the_same_json_written_differently_is_the_same() {
        let a = answer(200, r#"{"id": 1, "tags": ["a", "b"], "score": 2.0}"#);
        let b = answer(200, r#"{"score":2,"tags":["a","b"],"id":1.0}"#);
        assert_eq!(compare(&a, &b), Ok(()));
        // And bytes that are equal, JSON or not.
        assert_eq!(
            compare(&answer(200, "hello"), &answer(200, "hello")),
            Ok(())
        );
    }

    #[test]
    fn differences_say_where() {
        let a = answer(
            200,
            r#"{"user": {"name": "Ada", "roles": ["admin", "dev"]}}"#,
        );
        let cases = [
            (
                answer(404, "{}"),
                "status 200 from the handler, 404 from its shadow",
            ),
            (
                answer(
                    200,
                    r#"{"user": {"name": "ada", "roles": ["admin", "dev"]}}"#,
                ),
                r#"$.user.name is "Ada", and "ada" in the shadow's answer"#,
            ),
            (
                answer(
                    200,
                    r#"{"user": {"name": "Ada", "roles": ["admin", "ops"]}}"#,
                ),
                r#"$.user.roles[1] is "dev", and "ops" in the shadow's answer"#,
            ),
            (
                answer(200, r#"{"user": {"name": "Ada", "roles": ["admin"]}}"#),
                "$.user.roles has 2 items, and 1 in the shadow's answer",
            ),
            (
                answer(200, r#"{"user": {"roles": ["admin", "dev"]}}"#),
                "$.user.name is missing from the shadow's answer",
            ),
            (
                answer(
                    200,
                    r#"{"user": {"name": "Ada", "roles": ["admin", "dev"]}, "v": 2}"#,
                ),
                "$.v is only in the shadow's answer",
            ),
            (
                answer(200, "not json"),
                "the bodies differ (52 and 8 bytes)",
            ),
        ];
        for (shadow, want) in cases {
            assert_eq!(compare(&a, &shadow), Err(want.to_string()));
        }
        // A long value is cut short.
        let long = format!(r#"{{"text": "{}"}}"#, "x".repeat(200));
        let got = compare(&answer(200, &long), &answer(200, r#"{"text": 1}"#)).unwrap_err();
        assert!(got.len() < 120 && got.contains("..."), "{got}");
    }

    #[test]
    fn a_streamed_body_is_compared_by_status_only() {
        let streamed = Answer {
            status: 200,
            body: None,
        };
        assert_eq!(compare(&streamed, &answer(200, "anything")), Ok(()));
        assert!(compare(&streamed, &answer(500, "")).is_err());
    }
}
