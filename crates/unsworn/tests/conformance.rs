//! Runs the protocol conformance suite against every SDK runtime that's
//! installed on this machine.

mod common;

use std::process::Command;

use common::{has, repo_root};

fn run(runtime: &str, handlers: &str) {
    let out = Command::new(env!("CARGO_BIN_EXE_unsworn"))
        .current_dir(repo_root())
        .args([
            "conformance",
            "-c",
            "tests/conformance.toml",
            "--runtime",
            runtime,
            "--handlers",
            handlers,
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{runtime} failed conformance:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains(" 0 failed"), "{stdout}");
}

#[test]
fn raw_python_worker() {
    if has("python3") {
        run("raw-python", "examples/raw-python/worker.py");
    }
}

#[test]
fn python_sdk() {
    if has("python3") {
        run("python-jsonl", "sdk/python/conformance/handlers.py");
        run("python-binary", "sdk/python/conformance/handlers.py");
    }
}

#[test]
fn js_sdk_bun() {
    if has("bun") {
        run("bun-jsonl", "sdk/js/conformance/handlers.ts");
        run("bun-binary", "sdk/js/conformance/handlers.ts");
    }
}

#[test]
fn js_sdk_node() {
    let new_enough = Command::new("node")
        .args([
            "-e",
            "process.exit(Number(process.versions.node.split('.')[0]) >= 23 ? 0 : 1)",
        ])
        .status()
        .is_ok_and(|s| s.success());
    if new_enough {
        run("node-binary", "sdk/js/conformance/handlers.ts");
    }
}

#[test]
fn js_sdk_deno() {
    if has("deno") {
        run("deno-jsonl", "sdk/js/conformance/handlers.ts");
    }
}

#[test]
fn go_sdk() {
    // `go` takes `version`, not `--version`.
    let has_go = Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success());
    if has_go {
        run("go-binary", "sdk/go/conformance/handlers.go");
        run("go-jsonl", "sdk/go/conformance/handlers.go");
    }
}

#[test]
fn rust_sdk() {
    run("rust-binary", "sdk/rust/examples/conformance.rs");
    run("rust-jsonl", "sdk/rust/examples/conformance.rs");
}
