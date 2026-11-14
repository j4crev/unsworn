# The official Unsworn base image: the `unsworn` binary and nothing else.
# Projects add the runtimes their handlers need:
#
#   FROM ghcr.io/j4crev/unsworn:latest
#   RUN apt-get update && apt-get install -y --no-install-recommends python3 \
#       && rm -rf /var/lib/apt/lists/*
#   COPY . /app
#
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --profile dist --locked -p unsworn

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/dist/unsworn /usr/local/bin/unsworn
WORKDIR /app
ENV UNSWORN_LOG=info
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["unsworn"]
CMD ["serve", "--listen", "0.0.0.0:8080"]
