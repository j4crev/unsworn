//! `unsworn-core` is the pure heart of Unsworn.
//!
//! It parses and validates config, builds the route table from every route
//! source, matches requests, merges policies, validates requests and checks
//! credentials.
//!
//! **Rule:** this crate never performs I/O. No sockets, no processes, no
//! direct filesystem access and no clock. Files come through the [`Host`]
//! trait and the current time is passed in. That keeps it compilable to
//! `wasm32`, so the same core can run in the `unsworn` binary, inside an
//! edge isolate, or in a serverless shim. See `docs/architecture.md`.
//! CI builds the core for `wasm32-unknown-unknown`, and `clippy.toml` rejects
//! calls that would compile there but panic, such as `Instant::now`.

pub mod auth;
pub mod calls;
pub mod codegen;
pub mod config;
pub mod diag;
pub mod export;
pub mod mcp;
pub mod openapi;
pub mod outbound;
pub mod pipeline;
pub mod plan;
pub mod policy;
pub mod project;
pub mod routefiles;
pub mod rules;
pub mod store;
pub mod targets;
pub mod telemetry;
pub mod trace;
pub mod units;
pub mod validate;

pub use diag::{Diagnostic, Level};
pub use project::{
    Compiled, Host, MatchResult, Matched, Project, Route, compile, compile_symbolic, compile_with,
};

/// The Unsworn version, shared by the core and every host.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `Send` on native targets, nothing on wasm32.
///
/// Native hosts run requests on a multi-threaded runtime, so the futures
/// hosts hand the core must be `Send`. In a JavaScript host everything runs
/// on one thread and platform values (requests, KV bindings) aren't `Send`,
/// so wasm32 drops the bound.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync` on native targets, nothing on wasm32. See [`MaybeSend`].
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync + ?Sized> MaybeSync for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSync for T {}

#[cfg(test)]
mod auto_traits {
    fn send_sync<T: Send + Sync>() {}
    fn hr_send<T: ?Sized>()
    where
        for<'a> &'a T: Send,
    {
    }

    #[test]
    fn project_is_send_and_sync() {
        send_sync::<crate::Project>();
        send_sync::<crate::validate::Validation>();
        hr_send::<crate::Project>();
        hr_send::<crate::validate::LocationRules>();
    }
}
