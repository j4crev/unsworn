//! Handling one request: the checks and decisions every host makes the same
//! way, whatever its ingress, dispatch and store.
//!
//! Everything here is pure. A host reads the request, calls these, and does
//! the I/O they ask for (reading bodies, calling handlers, counting rate
//! limits). See `docs/architecture.md`.

mod checks;
mod client_ip;
mod problem;

pub use checks::{
    access, cache_key, check_body, check_content_length, decode_param, finish_headers, header_map,
    ip_allowed, key_value, preflight_headers, query_pairs, rate_limit_bucket, redirect_location,
    response_head, validate_inputs,
};
pub use client_ip::ClientIp;
pub use problem::{Problem, auth_failure, rate_limit_unavailable, retry_secs, too_many_requests};
