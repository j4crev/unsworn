//! `unsworn build --target image`: the Dockerfile it writes for a project.

mod common;

use common::Project;

const CONFIG: &str = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run node_modules/@unswornjs/sdk/src/worker.ts"

[runtimes.go]
extensions = [".go"]
build = "go build -o .unsworn/go-worker ./handlers"
command = ".unsworn/go-worker"

[auth.key]
type = "api_key"
keys = "{env.IMAGE_TEST_KEY_THAT_IS_NOT_SET}"

[[route]]
path = "/a"
handler = "src/a.ts"
auth = "key"

[[route]]
path = "/b"
handler = "handlers/b.go#B"
"#;

#[test]
fn writes_a_dockerfile_from_the_config_and_the_projects_files() {
    let p = Project::new(&[
        ("unsworn.toml", CONFIG),
        ("src/a.ts", ""),
        ("handlers/b.go", ""),
        ("package.json", r#"{"packageManager": "bun@1.2.3"}"#),
        ("bun.lock", ""),
        ("go.mod", "module x\n\ngo 1.25.0\n"),
    ]);
    // The variable the config reads isn't set, and isn't needed.
    let out = p.command(&["build", "--target", "image"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    assert!(
        stdout.contains("✓ wrote dist/image/Dockerfile: bun 1.2.3, go 1.25.0 (built in the image)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("docker build -f dist/image/Dockerfile -t "),
        "{stdout}"
    );
    assert!(
        stdout.contains("docker run -p 8080:8080 -e IMAGE_TEST_KEY_THAT_IS_NOT_SET "),
        "{stdout}"
    );
    // A compiled worker that's missing should stop the container.
    assert!(
        stderr.contains("[runtimes.go]: set `eager = true`"),
        "{stderr}"
    );

    let dockerfile = std::fs::read_to_string(p.path().join("dist/image/Dockerfile")).unwrap();
    for wanted in [
        "FROM golang:1.25.0-bookworm AS build-go\n",
        "RUN go build -o .unsworn/go-worker ./handlers\n",
        "FROM oven/bun:1.2.3 AS js-deps\n",
        "RUN bun install --frozen-lockfile --production\n",
        &format!(
            "COPY --from=ghcr.io/j4crev/unsworn:v{} /usr/local/bin/unsworn /usr/local/bin/unsworn\n",
            env!("CARGO_PKG_VERSION")
        ),
        "COPY --from=build-go /app/.unsworn/go-worker .unsworn/go-worker\n",
        "USER unsworn\n",
        "ENV UNSWORN_NO_BUILD=1\n",
    ] {
        assert!(
            dockerfile.contains(wanted),
            "missing {wanted:?} in\n{dockerfile}"
        );
    }
    let ignore =
        std::fs::read_to_string(p.path().join("dist/image/Dockerfile.dockerignore")).unwrap();
    for line in ["node_modules", ".git", ".env", "dist/image"] {
        assert!(ignore.lines().any(|l| l == line), "{ignore}");
    }

    // Somewhere else, and with another image for the binary.
    let out = p.command(&[
        "build",
        "--target",
        "image",
        "--out",
        "docker",
        "--from",
        "registry.example/unsworn:1",
    ]);
    assert!(out.status.success());
    let dockerfile = std::fs::read_to_string(p.path().join("docker/Dockerfile")).unwrap();
    assert!(
        dockerfile.contains("COPY --from=registry.example/unsworn:1 /usr/local/bin/unsworn"),
        "{dockerfile}"
    );

    // An invalid project has no image.
    p.write(
        "unsworn.toml",
        "[[route]]\npath = \"/x\"\nhandler = \"missing.ts\"\n",
    );
    assert!(!p.command(&["build", "--target", "image"]).status.success());
    // And the other targets are still named when one is mistyped.
    let out = p.command(&["build", "--target", "docker"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("and image for a container"), "{err}");
}
