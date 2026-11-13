//! The `unsworn` binary: ingress (HTTP), dispatch (worker processes) and the
//! CLI, wrapped around `unsworn-core`. See `docs/architecture.md`.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V" | "version") => {
            println!("unsworn {}", unsworn_core::VERSION);
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "unsworn {}: not implemented yet. See docs/ for the design.",
                unsworn_core::VERSION
            );
            ExitCode::FAILURE
        }
    }
}
