FROM rust:alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release --target $(uname -m)-unknown-linux-musl && \
    cp target/$(uname -m)-unknown-linux-musl/release/jeha /jeha

FROM scratch

COPY --from=builder /jeha /jeha

ENTRYPOINT ["/jeha"]
CMD ["run", "--config", "/config.toml"]
