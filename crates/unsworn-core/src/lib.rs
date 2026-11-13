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

pub mod auth;
pub mod config;
pub mod diag;
pub mod export;
pub mod openapi;
pub mod policy;
pub mod project;
pub mod routefiles;
pub mod rules;
pub mod targets;
pub mod units;
pub mod validate;

pub use diag::{Diagnostic, Level};
pub use project::{Compiled, Host, MatchResult, Matched, Project, Route, compile};

/// The Unsworn version, shared by the core and every host.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
