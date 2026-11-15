# The official Unsworn base image: the `unsworn` binary and nothing else.
# Projects add the runtimes their handlers need. Unsworn, and so every
# handler, runs as the unprivileged user `unsworn`, so switch to root to
# install packages and copy the project, and back afterwards:
#
#   FROM ghcr.io/j4crev/unsworn:latest
#   USER root
#   RUN apt-get update && apt-get install -y --no-install-recommends python3 \
#       && rm -rf /var/lib/apt/lists/*
#   COPY . /app
#   USER unsworn
#
# The project stays owned by root, so handlers can read it but not change
# it: a flaw in one can't rewrite `unsworn.toml`, or another handler, for
# the next start. That means runtimes' `build` steps can't write there at
# startup either, so the image skips them (UNSWORN_NO_BUILD=1). Build
# compiled workers in the image instead, a `RUN` before `USER unsworn` or
# an earlier stage, e.g. `RUN go build -o .unsworn/go-worker ./routes`.
# `ENV UNSWORN_NO_BUILD=0` brings startup builds back, for a `build` that
# writes somewhere `unsworn` can.
#
# The base images are pinned by digest, which Dependabot keeps current.
FROM rust:1-bookworm@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea AS build
WORKDIR /src
COPY . .
RUN cargo build --profile dist --locked -p unsworn

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --create-home unsworn \
    && install -d -o root -g root -m 755 /app
COPY --from=build /src/target/dist/unsworn /usr/local/bin/unsworn
WORKDIR /app
USER unsworn
ENV UNSWORN_LOG=info
ENV UNSWORN_NO_BUILD=1
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["unsworn"]
CMD ["serve", "--listen", "0.0.0.0:8080"]
