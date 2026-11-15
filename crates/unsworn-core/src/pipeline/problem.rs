//! RFC 9457 problem responses, the error answers Unsworn itself gives.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use serde_json::{Value, json};

use crate::auth::{AuthFailure, Scheme, www_authenticate};
use crate::validate::FieldError;

/// An error response: a status, a human-readable detail, any field errors,
/// and headers that go with it (`Retry-After`, `Allow`, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub status: StatusCode,
    pub detail: String,
    pub errors: Option<Vec<FieldError>>,
    pub headers: HeaderMap,
}

impl Problem {
    pub const CONTENT_TYPE: &'static str = "application/problem+json";

    pub fn new(status: StatusCode, detail: impl Into<String>) -> Problem {
        Problem {
            status,
            detail: detail.into(),
            errors: None,
            headers: HeaderMap::new(),
        }
    }

    pub fn with_errors(mut self, errors: Option<&[FieldError]>) -> Problem {
        self.errors = errors.map(<[FieldError]>::to_vec);
        self
    }

    /// Adds a header. Invalid values are dropped.
    pub fn header(mut self, name: HeaderName, value: impl AsRef<str>) -> Problem {
        if let Ok(v) = HeaderValue::from_str(value.as_ref()) {
            self.headers.insert(name, v);
        }
        self
    }

    /// The JSON body.
    pub fn body(&self) -> Value {
        let mut body = json!({
            "type": "about:blank",
            "title": self.status.canonical_reason().unwrap_or("Error"),
            "status": self.status.as_u16(),
            "detail": self.detail,
        });
        if let Some(errors) = &self.errors {
            body["errors"] = json!(errors);
        }
        body
    }

    /// The encoded body.
    pub fn body_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.body()).unwrap_or_default()
    }
}

/// Whole seconds to wait, at least 1.
pub fn retry_secs(retry_after: Duration) -> u64 {
    retry_after.as_secs_f64().ceil().max(1.0) as u64
}

pub fn too_many_requests(retry_after: Duration) -> Problem {
    Problem::new(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded")
        .header(header::RETRY_AFTER, retry_secs(retry_after).to_string())
}

pub fn rate_limit_unavailable() -> Problem {
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "rate limiting is unavailable",
    )
}

/// The answer to failed authentication, with `WWW-Authenticate` for JWT
/// schemes.
pub fn auth_failure(failure: &AuthFailure, schemes: &BTreeMap<String, Arc<Scheme>>) -> Problem {
    let (status, detail) = match failure {
        AuthFailure::Unauthorized { reason, .. } => (StatusCode::UNAUTHORIZED, reason.clone()),
        AuthFailure::InsufficientScope { scopes, .. } => (
            StatusCode::FORBIDDEN,
            format!("this requires the scopes: {}", scopes.join(" ")),
        ),
        AuthFailure::Unavailable { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "credentials can't be verified right now".to_string(),
        ),
    };
    let mut problem = Problem::new(status, detail);
    if let Some(h) = www_authenticate(failure, schemes) {
        problem = problem.header(header::WWW_AUTHENTICATE, h);
    }
    problem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_and_headers() {
        let p = too_many_requests(Duration::from_millis(1200));
        assert_eq!(p.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(p.headers[header::RETRY_AFTER], "2");
        let body = p.body();
        assert_eq!(body["status"], 429);
        assert_eq!(body["title"], "Too Many Requests");
        assert!(body.get("errors").is_none());

        let errors = [FieldError {
            location: "query.limit".into(),
            message: "too big".into(),
        }];
        let p = Problem::new(StatusCode::BAD_REQUEST, "bad").with_errors(Some(&errors));
        assert_eq!(p.body()["errors"][0]["location"], "query.limit");
    }

    #[test]
    fn retry_is_at_least_a_second() {
        assert_eq!(retry_secs(Duration::from_millis(10)), 1);
        assert_eq!(retry_secs(Duration::from_secs(3)), 3);
    }
}
