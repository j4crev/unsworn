//! Handling one request: the checks and decisions every host makes the same
//! way, whatever its ingress, dispatch and store.
//!
//! A host builds a [`Router`] for each loaded project and calls
//! [`Router::handle`] for each request, passing a [`Host`] that does the I/O
//! the core never does itself: reading bodies, calling handlers, proxying,
//! fetching JWKS documents, and the store. Everything else, from matching to
//! the final headers, happens here, so every host answers the same way. See
//! `docs/architecture.md`.
//!
//! The checks the flow is built from are public too, for hosts that need
//! one on its own.

mod auth;
mod checks;
mod client_ip;
mod flow;
pub mod host;
mod problem;
pub mod proxy;
pub mod shadow;

pub use auth::KeyCache;
pub use flow::{Handled, Router, json_response, problem_response};
pub use host::{
    BODY_IDLE, Body, BodyError, CacheStep, CallError, Deadline, FetchError, Host, ProxyRequest,
    Reply, Response, Rule, StaticRequest, Step,
};

pub(crate) use checks::NOT_UNRESERVED;
pub use checks::{
    JsonBody, access, bad_json, cache_key, check_body, check_content_length, check_json_text,
    decode_param, finish_headers, header_map, ip_allowed, key_value, normalize_path,
    preflight_headers, query_pairs, rate_limit_bucket, redirect_location, response_head, unmet,
    validate_inputs,
};
pub use client_ip::ClientIp;
pub use problem::{Problem, auth_failure, rate_limit_unavailable, retry_secs, too_many_requests};

#[cfg(test)]
mod tests;
