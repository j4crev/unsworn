//! Functions: what a call from one handler to another is checked against
//! and turned into, the same on every host.
//!
//! A host does the I/O in between. It finds the request the call is made
//! for in its own record, asks [`prepare`] for the request to send, runs
//! the function's handler as it runs any handler, and gives the answer to
//! [`finish`].

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Map, Value};
use unsworn_proto::{Json, Request, Response};

use crate::config::ResponseValidation;
use crate::pipeline::CallError;
use crate::project::Function;
use crate::trace::{self, TraceContext};

/// How deep calls may nest: a function calling a function, and so on. A
/// handler that calls itself would otherwise only stop at its timeout.
pub const MAX_CALL_DEPTH: u64 = 16;

/// The request a function is called for, as the host recorded it.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    /// The handler that's calling.
    pub handler: String,
    /// The function gets the same context: verified credentials, and what
    /// `before` handlers added.
    pub context: Map<String, Value>,
    /// The trace the request belongs to, which the function joins.
    pub trace: Option<TraceContext>,
}

impl Caller {
    /// With the trace the handler named for this call, if it named one,
    /// in place of the request's.
    pub fn with_trace(mut self, given: Option<TraceContext>) -> Self {
        if given.is_some() {
            self.trace = given;
        }
        self
    }
}

/// The trace a handler names for a call: its own span's, usually. `None`
/// unless the `traceparent` is one a receiver would accept, and a
/// `tracestate` counts only beside one.
pub fn given_trace(traceparent: Option<&str>, tracestate: Option<&str>) -> Option<TraceContext> {
    TraceContext::from_headers(|name| match name {
        trace::TRACEPARENT => traceparent,
        trace::TRACESTATE => tracestate,
        _ => None,
    })
}

/// A call that didn't succeed: the status the caller sees, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub status: u16,
    pub error: String,
    /// For a `422`, the input's errors.
    pub json: Value,
}

impl Failure {
    pub fn new(status: u16, error: impl Into<String>) -> Self {
        Failure {
            status,
            error: error.into(),
            json: Value::Null,
        }
    }
}

/// "it declares: a, b", for an error about a name that isn't declared.
pub fn declared<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let known: Vec<&str> = names.map(String::as_str).collect();
    match known.as_slice() {
        [] => "it declares none".to_string(),
        _ => format!("it declares: {}", known.join(", ")),
    }
}

/// Finds the function, checks the input against its schema, and builds the
/// request its handler gets: `route` and `path` are `fn:<name>`, the
/// context is the caller's plus `caller`, and the headers are the caller's
/// trace.
pub fn prepare(
    functions: &BTreeMap<String, Arc<Function>>,
    name: &str,
    input: &Value,
    caller: Caller,
) -> Result<(Arc<Function>, Request), Failure> {
    let Some(function) = functions.get(name) else {
        // Name the table to add, since a handler's `call` doesn't say
        // where its functions come from.
        return Err(Failure::new(
            404,
            format!(
                "no [[function]] named \"{name}\" in the config ({})",
                declared(functions.keys())
            ),
        ));
    };
    let depth = caller
        .context
        .get("caller")
        .and_then(|c| c["depth"].as_u64());
    let depth = depth.unwrap_or(0) + 1;
    if depth > MAX_CALL_DEPTH {
        return Err(Failure::new(
            508,
            format!(
                "calls are nested more than {MAX_CALL_DEPTH} deep: does a function call itself?"
            ),
        ));
    }
    if let Some(rule) = &function.input
        && let Err(errors) = rule.check(input, "input")
    {
        let errors: Vec<Value> = errors
            .iter()
            .map(|e| serde_json::json!({ "location": e.location, "message": e.message }))
            .collect();
        return Err(Failure {
            status: 422,
            error: format!(
                "the input doesn't match the schema of the function \"{}\"",
                function.name
            ),
            json: serde_json::json!({ "errors": errors }),
        });
    }
    let mut context = caller.context;
    context.insert(
        "caller".into(),
        serde_json::json!({
            "function": function.name,
            "from": caller.handler,
            "depth": depth,
        }),
    );
    let route = format!("fn:{}", function.name);
    let input = serde_json::to_vec(input).unwrap_or_else(|_| b"null".to_vec());
    let request = Request {
        handler: function.handler.raw.clone(),
        route: route.clone(),
        method: "POST".into(),
        path: route,
        context,
        // A function has no HTTP request of its own. These are the
        // calling request's, so the function's spans join its trace, and
        // so do those of whatever the function calls in turn.
        headers: caller
            .trace
            .iter()
            .flat_map(TraceContext::headers)
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        json: Some(Json::raw(Bytes::from(input))),
        ..Request::default()
    };
    Ok((function.clone(), request))
}

/// What the caller sees when the function's handler couldn't be run, or
/// didn't answer.
pub fn failed(error: CallError) -> Failure {
    match error {
        CallError::Timeout => Failure::new(504, "the function timed out"),
        CallError::Unavailable(m) => Failure::new(503, format!("runtime unavailable: {m}")),
        e @ (CallError::WorkerFailed(_) | CallError::MissingHandler(_)) => {
            Failure::new(502, e.to_string())
        }
        CallError::Handler(message) => Failure::new(500, message),
        e => Failure::new(500, e.to_string()),
    }
}

/// A function that answered `continue`, which only a `before` handler may.
pub fn continued() -> Failure {
    Failure::new(
        502,
        "the function answered `continue`, which is for `before` handlers",
    )
}

/// What a function's handler sent back.
pub enum Answer {
    /// No body.
    Empty,
    /// A body that came whole. JSON or text, when `response` says so.
    Bytes(Bytes),
    /// A stream, which a function can't answer with.
    Stream,
}

/// The call's result from the function's answer: its status and JSON. The
/// output is checked against the function's schema as `validation` says;
/// a mismatch is logged, and with `enforce` it fails the call.
pub fn finish(
    function: &Function,
    validation: ResponseValidation,
    response: Response,
    answer: Answer,
) -> Result<(u16, Value), Failure> {
    let unreadable =
        |e: serde_json::Error| Failure::new(502, format!("the function's JSON can't be read: {e}"));
    let json = match answer {
        // A host that calls handlers in its own process leaves JSON and
        // text in the response itself.
        Answer::Empty => match response.json {
            Some(Json::Value(value)) => value,
            Some(Json::Raw(raw)) => serde_json::from_slice(raw.text()).map_err(unreadable)?,
            None => response.text.map_or(Value::Null, Value::String),
        },
        Answer::Bytes(bytes) if response.json.is_some() => {
            serde_json::from_slice(&bytes).map_err(unreadable)?
        }
        Answer::Bytes(_) if response.text.is_some() => {
            Value::String(response.text.unwrap_or_default())
        }
        Answer::Bytes(_) | Answer::Stream => {
            return Err(Failure::new(
                502,
                "a function answers with JSON, and this one sent raw bytes or a stream",
            ));
        }
    };
    if response.status < 400
        && let Some(rule) = &function.output
        && validation != ResponseValidation::Off
        && let Err(errors) = rule.check(&json, "output")
    {
        let detail: Vec<String> = errors
            .iter()
            .map(|e| format!("{}: {}", e.location, e.message))
            .collect();
        tracing::warn!(
            function = %function.name,
            "the function's output doesn't match its schema: {}",
            detail.join("; ")
        );
        if validation == ResponseValidation::Enforce {
            return Err(Failure::new(
                502,
                format!(
                    "the output of the function \"{}\" doesn't match its schema",
                    function.name
                ),
            ));
        }
    }
    Ok((response.status, json))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    const REQUESTS: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-1111111111111111-01";

    #[test]
    fn a_handlers_trace_replaces_the_requests() {
        let requests = given_trace(Some(REQUESTS), Some("vendor=request"));
        let after = |traceparent, tracestate| {
            let caller = Caller {
                trace: requests.clone(),
                ..Caller::default()
            };
            caller
                .with_trace(given_trace(traceparent, tracestate))
                .trace
        };
        let own = after(Some(OWN), Some("vendor=mine")).unwrap();
        assert_eq!(own.traceparent, OWN);
        assert_eq!(own.tracestate.as_deref(), Some("vendor=mine"));
        // The request's `tracestate` doesn't go with another `traceparent`.
        assert_eq!(after(Some(OWN), None).unwrap().tracestate, None);
        // Nothing a receiver would accept: the request's trace stays.
        for (traceparent, tracestate) in [
            (None, None),
            (Some("nonsense"), Some("vendor=mine")),
            (None, Some("vendor=mine")),
        ] {
            assert_eq!(after(traceparent, tracestate), requests);
        }
        assert_eq!(given_trace(Some("nonsense"), None), None);
    }
}
