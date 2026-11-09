# Two images from one build:
#   docker build --target server  -t waypost-server .
#   docker build --target builder -t waypost-builder .
FROM rust:1.98.1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked

# Server: one binary, runs as non-root, reads the index from /index.
FROM debian:trixie-20251020-slim AS server
RUN useradd --uid 10001 --no-create-home --shell /usr/sbin/nologin waypost
COPY --from=build /src/target/release/waypost-server /usr/local/bin/waypost-server
USER 10001
ENV WAYPOST_INDEX=/index/waypost.wpi WAYPOST_LISTEN=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/waypost-server"]

# Builder: osmium-tool and the index builder. Writes /index/waypost.wpi.
FROM debian:trixie-20251020-slim AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends osmium-tool curl ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 10001 --create-home waypost \
 && mkdir /index && chown 10001 /index
COPY --from=build /src/target/release/waypost-builder /usr/local/bin/waypost-builder
COPY scripts/prep.sh scripts/build-index.sh /usr/local/bin/
USER 10001
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/build-index.sh"]
