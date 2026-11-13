//! `unsworn-core` is the pure heart of Unsworn.
//!
//! It parses and validates config, matches routes, validates requests and
//! decides what should happen to each request: respond, proxy, or call a
//! handler with a validated context.
//!
//! **Rule:** this crate never performs I/O. No sockets, no processes, no
//! filesystem, no clocks it does not receive as input. That keeps it
//! compilable to both native targets and `wasm32`, so the same core can run
//! in the `unsworn` binary, inside a Cloudflare Worker, or in a serverless shim.
//! See `docs/architecture.md`.

/// The Unsworn version, shared by the core and every host.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
