ARG RUST_IMAGE=rust:1.94.1-bookworm
ARG DEBIAN_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS source-builder
WORKDIR /build

RUN apt-get update \
    && apt-get install --yes --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl

COPY --from=protocol . /build/TeamViewRelay-Protocol/proto
COPY . /build/TeamViewRelay-Squaremap-Source-Client
WORKDIR /build/TeamViewRelay-Squaremap-Source-Client
RUN cargo build --locked --release --target x86_64-unknown-linux-musl \
    --bin teamviewrelay-squaremap-source-client

FROM scratch AS source-runtime
COPY --from=source-builder /build/TeamViewRelay-Squaremap-Source-Client/target/x86_64-unknown-linux-musl/release/teamviewrelay-squaremap-source-client /teamviewrelay-squaremap-source-client
COPY --chown=65532:65532 docker-root/data /data
USER 65532:65532
ENTRYPOINT ["/teamviewrelay-squaremap-source-client"]
CMD ["--config", "/config/config.toml"]

FROM ${RUST_IMAGE} AS pass-cdn-builder
WORKDIR /build
COPY --from=protocol . /build/TeamViewRelay-Protocol/proto
COPY . /build/TeamViewRelay-Squaremap-Source-Client
WORKDIR /build/TeamViewRelay-Squaremap-Source-Client
RUN cargo build --locked --release --bin teamviewrelay-pass-cdn

FROM ${DEBIAN_IMAGE} AS pass-cdn-runtime
ENV HOME=/tmp \
    RUST_LOG=info

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates \
        chromium \
        fonts-liberation \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 app

COPY --from=pass-cdn-builder /build/TeamViewRelay-Squaremap-Source-Client/target/release/teamviewrelay-pass-cdn /usr/local/bin/teamviewrelay-pass-cdn
RUN chmod 0755 /usr/local/bin/teamviewrelay-pass-cdn

USER app
ENTRYPOINT ["/usr/local/bin/teamviewrelay-pass-cdn"]
CMD ["--serve", "--browser-mode", "resident", "--host", "0.0.0.0"]

# Keep the source client as the default result of `docker build .`.
FROM source-runtime AS final
