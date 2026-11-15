//! Unsworn's core as a WebAssembly module, for hosts written in JavaScript:
//! Cloudflare Workers now, serverless functions later.
//!
//! The module routes requests with the same [`unsworn_core::pipeline`] the
//! binary uses. The JS host hands it each request with a `Bridge`, an
//! object that does the I/O: calling handlers in the same isolate, the
//! store, assets, outgoing requests. So the wasm is the same everywhere and
//! each platform only writes its bridge (see `sdk/js/src/cloudflare`).
//!
//! Built with `wasm-bindgen --target web`; see `crates/unsworn-wasm/build.sh`.

mod files;
mod statics;

#[cfg(target_arch = "wasm32")]
mod bridge;
#[cfg(target_arch = "wasm32")]
mod exports;
#[cfg(target_arch = "wasm32")]
mod host;
#[cfg(target_arch = "wasm32")]
mod log;

pub use files::Files;
pub use statics::candidates as static_candidates;
