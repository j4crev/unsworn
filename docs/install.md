# Installing Unsworn

Unsworn runs on Linux (x86_64 and arm64), and on macOS for development.

```sh
curl -fsSL https://raw.githubusercontent.com/j4crev/unsworn/main/install.sh | sh
```

By default, the installer verifies the release's build attestation if the
GitHub CLI is installed and signed in. Otherwise, it installs without
that check. To require successful verification before installing, set
`UNSWORN_VERIFY=1`. To skip verification, set `UNSWORN_VERIFY=0`.
See [`SECURITY.md`](../SECURITY.md).

With Rust 1.88 or newer, `cargo install unsworn` works too. To try
unreleased changes, build from `main`:

```sh
cargo install --git https://github.com/j4crev/unsworn unsworn
```

## macOS

On macOS everything works that doesn't read Linux's `/proc`. What differs:

- **A handler isn't kept from the server's secrets.** macOS shows a
  process's environment to any other process of the same user, and has
  no switch for it as Linux has. Develop there; deploy on Linux.
- **Pools grow only when calls wait.** A worker that takes many requests
  at once is added to on Linux when it uses a whole core, which is read
  from `/proc`. Set `workers` to a number to try more on a Mac.
- **`unsworn top` has no CPU or memory figures,** for the same reason.

## Containers

`unsworn build --target image` writes a Dockerfile for the project: each
runtime at the version your project names, dependencies from their
lockfiles, compiled workers built in stages of their own (see
[`image.md`](image.md)). Or extend the `ghcr.io/j4crev/unsworn` base image
yourself.

Either way it runs as the unprivileged user `unsworn`, and the project in
`/app` stays owned by root, so handler code can't change the rules it runs
under. For the same reason it skips runtimes' `build` steps at startup:
compiled workers are built into the image.
