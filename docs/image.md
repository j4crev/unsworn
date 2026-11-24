# A container image: `unsworn build --target image`

A project with handlers in two or three languages needs an image with
each runtime in it, each one's dependencies installed, compiled workers
built, and the files owned so that handler code can't change them.
`unsworn build --target image` writes that Dockerfile from the config and
the project's own files:

```
$ unsworn build --target image
✓ wrote dist/image/Dockerfile: bun 1.4.4, python 3.13, go 1.27.1 (built in the image)
  build    docker build -f dist/image/Dockerfile -t myapi .
  run      docker run -p 8080:8080 -e STRIPE_KEY -e JWT_SECRET myapi
```

It writes two files and stops there. It doesn't run Docker, and the
Dockerfile is yours: read it, keep it in git, change it. Running the
command again writes it afresh from the config.

## What the Dockerfile does

- **A stage for each compiled runtime.** For a runtime with a `build`
  command that runs `go` or `cargo`, a stage with that toolchain runs the
  command, and only the worker it produces (the file the runtime's
  `command` names) is copied into the image.
- **A stage for JavaScript dependencies.** `node_modules` is installed
  from the lockfile (`bun.lock`, `package-lock.json`, `pnpm-lock.yaml` or
  `yarn.lock`), without dev dependencies, and copied in.
- **The interpreters the runtimes use.** Bun and Node are one file
  each, copied from their official images. For Python the image starts
  from the official Python image, and `requirements.txt` (or `uv.lock`)
  is installed. Without either, only the SDK is.
- **The `unsworn` binary,** from this version's official image.
- **The project, owned by root,** in `/app`. Unsworn and every handler
  run as the user `unsworn`, so a flaw in a handler can't rewrite the
  config, or another handler, for the next start.
- **`UNSWORN_NO_BUILD=1`,** since the workers are already built and
  `/app` can't be written to.

A runtime that no route uses is left out. For a runtime whose command
Unsworn doesn't recognise, the Dockerfile has a `TODO` where to install
what it needs, and the command says so.

Next to it is `Dockerfile.dockerignore`, which BuildKit reads for a
Dockerfile of that name. It keeps `.git`, `node_modules`, build output
and `.env` files out of the build.

## Versions

Each runtime's version comes from the first of these that says:

| Runtime | Looked for |
|---|---|
| Bun | `.bun-version`, `.tool-versions`, `packageManager` in `package.json` |
| Node | `.nvmrc`, `.node-version`, `.tool-versions` |
| Python | `.python-version`, `.tool-versions` |
| Go | `.tool-versions`, the `go` line in `go.mod` |
| Rust | `.tool-versions`, `rust-toolchain.toml` |

Otherwise it's the version installed where you ran the command, and the
command tells you, since that's a version nothing in the project
records. If the tool isn't installed either, it's the newest of a major
version.

## Before you ship it

- **Mark compiled runtimes `eager = true`.** A worker that's missing from
  the image then stops the container at startup, instead of failing the
  first request to it. The command reminds you.
- **Pass the environment at run time.** The `run` line lists every
  `{env.NAME}` the config reads. None of them is in the image.
- **Check what `COPY . .` copies.** Everything in the project directory
  that `Dockerfile.dockerignore` doesn't name goes into the image.

## Options

```sh
unsworn build --target image --out docker        # write docker/Dockerfile instead
unsworn build --target image --from registry.example/unsworn:0.7.0   # take the binary from another image
```

## Limits

- **Debian only.** Every image it uses is the Debian (`bookworm`) one,
  so the pieces fit together. There's no Alpine or distroless variant.
- **It reads commands, it doesn't run them.** A runtime is recognised by
  the program its `command` or `build` names: `bun`, `node`, `python3`,
  `go`, `cargo`.
- **One project directory.** A handler that imports from outside the
  project, or a Cargo workspace above it, isn't in the build context.
- **Images aren't pinned by digest.** Tags name a version, which is what
  the project's files say. Add digests if your policy asks for them.
