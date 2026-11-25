# Multi-stage build: a pinned Rust builder compiles a static musl binary, and
# the final image is FROM scratch and carries only that binary.
# Renovate (.github/renovate.json) moves the tag and the digest together.
FROM rust:1.100-alpine@sha256:cfbeb9c8c956f048a081ae034a7c8ffbd60242f3ff76b645d3a41523f665f761 AS build

# The dependency tree needs a C toolchain under musl (ring, via rustls).
RUN apk add --no-cache musl-dev build-base

WORKDIR /build

# Dependency layer: compile the dependencies once against a stub crate, then
# throw the stub away. BuildKit gives COPYed files their checkout mtime,
# which can be older than the stub build's artifacts, so cargo's freshness
# check would skip the real build and ship the stub (measured the hard way on
# secure-file-share). The touch after the real COPY defeats that check, and
# the smoke run makes a stub fail the image build loudly.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && printf 'fn main() {}' > src/main.rs \
    && printf '' > src/lib.rs \
    && cargo build --release --locked

COPY . .
RUN find src -type f -exec touch {} + \
    && cargo build --release --locked \
    && ! ./target/release/hce >/tmp/smoke.log 2>&1 \
    && grep -q 'HCE_CLIENT_ID' /tmp/smoke.log

FROM scratch
# ARG must be re-declared after the final FROM, or the labels expand empty.
ARG VERSION=dev
ARG COMMIT=unknown
LABEL org.opencontainers.image.title="hce" \
    org.opencontainers.image.description="Prometheus exporter for Siemens and Bosch Home Connect appliances" \
    org.opencontainers.image.source="https://github.com/Yornik/Siemens-home-connect-exporter" \
    org.opencontainers.image.licenses="MIT" \
    org.opencontainers.image.version="${VERSION}" \
    org.opencontainers.image.revision="${COMMIT}"
COPY --from=build /build/target/release/hce /hce
# reqwest uses rustls with compiled-in webpki root certificates, so the image
# needs no CA bundle file.
USER 65532:65532
ENV HCE_LISTEN_ADDR=0.0.0.0:9780
EXPOSE 9780
ENTRYPOINT ["/hce"]
