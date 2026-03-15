# --- Build from source (used by: docker build) ---
FROM rust:alpine AS build
WORKDIR /build
RUN apk add --no-cache musl-dev
COPY . .
RUN cargo build --release \
    && strip target/release/jeha \
    && cp target/release/jeha /jeha

# --- Use pre-built binary (used by: CI with --build-arg PREBUILT=1) ---
FROM alpine AS prebuilt
ARG TARGETARCH
ARG TARGETVARIANT
WORKDIR /
COPY jeha-${TARGETARCH}${TARGETVARIANT} /jeha
RUN chmod +x /jeha

# --- Final image ---
ARG PREBUILT
FROM ${PREBUILT:+prebuilt}${PREBUILT:-build} AS selected

FROM scratch
COPY --from=selected /jeha /jeha
ENTRYPOINT ["/jeha"]
CMD ["run", "--config", "/config.toml"]
