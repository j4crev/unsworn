# The official Unsworn base image: the `unsworn` binary and nothing else.
# Projects add the runtimes their handlers need. Unsworn, and so every
# handler, runs as the unprivileged user `unsworn`, so switch to root to
# install packages, and back afterwards:
#
#   FROM ghcr.io/j4crev/unsworn:latest
#   USER root
#   RUN apt-get update && apt-get install -y --no-install-recommends python3 \
#       && rm -rf /var/lib/apt/lists/*
#   USER unsworn
#   COPY --chown=unsworn . /app
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
    && install -d -o unsworn -g unsworn /app
COPY --from=build /src/target/dist/unsworn /usr/local/bin/unsworn
WORKDIR /app
USER unsworn
ENV UNSWORN_LOG=info
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["unsworn"]
CMD ["serve", "--listen", "0.0.0.0:8080"]
