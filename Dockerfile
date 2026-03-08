FROM rust:alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build
COPY . .

ARG VERSION
ARG TARGETARCH
ARG TARGETVARIANT
RUN if [ -n "$VERSION" ]; then \
      cp "jeha-${TARGETARCH}${TARGETVARIANT}" /jeha; \
    else \
      cargo build --release --target $(uname -m)-unknown-linux-musl && \
      cp target/$(uname -m)-unknown-linux-musl/release/jeha /jeha; \
    fi && \
    chmod +x /jeha

FROM scratch

COPY --from=builder /jeha /jeha

ENTRYPOINT ["/jeha"]
CMD ["run", "--config", "/config.toml"]
