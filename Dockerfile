ARG RUST_IMAGE=rust:1.94-bookworm

FROM ${RUST_IMAGE} AS builder
WORKDIR /build

RUN apt-get update \
    && apt-get install --yes --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl

COPY --from=protocol . /build/TeamViewRelay-Protocol/proto
COPY . /build/TeamViewRelay-Squaremap-Source-Client
WORKDIR /build/TeamViewRelay-Squaremap-Source-Client
RUN cargo build --locked --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /build/TeamViewRelay-Squaremap-Source-Client/target/x86_64-unknown-linux-musl/release/teamviewrelay-squaremap-source-client /teamviewrelay-squaremap-source-client
USER 65532:65532
ENTRYPOINT ["/teamviewrelay-squaremap-source-client"]
CMD ["--config", "/config/config.toml"]
