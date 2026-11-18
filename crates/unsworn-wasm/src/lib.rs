//! Unsworn's core as a WebAssembly module, for hosts written in JavaScript:
//! Cloudflare Workers now, serverless functions later.
//!
//! The module routes requests with the same [`unsworn_core::pipeline`] the
//! binary uses. The JS host hands it each request with a `Bridge`, an
//! object that does the I/O: calling handlers in the same isolate, the
//! store, assets, outgoing requests. So the wasm is the same everywhere and
//! each platform only writes its bridge (see `sdk/js/src/cloudflare`).
//!
//! It loads a bundle from `unsworn build`, whose config documents are JSON
//! (see [`unsworn_core::bundle`]), so the core is built here without its
//! TOML and YAML parsers.
//!
//! Built with `wasm-bindgen --target web`; see `crates/unsworn-wasm/build.sh`.

mod statics;

#[cfg(target_arch = "wasm32")]
mod bridge;
#[cfg(target_arch = "wasm32")]
mod exports;
#[cfg(target_arch = "wasm32")]
mod host;
#[cfg(target_arch = "wasm32")]
mod log;

pub use statics::candidates as static_candidates;
pub use unsworn_core::bundle::Files;
