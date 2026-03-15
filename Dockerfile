FROM alpine AS builder
ARG TARGETARCH
ARG TARGETVARIANT
COPY jeha-${TARGETARCH}${TARGETVARIANT} /jeha
RUN chmod +x /jeha

FROM scratch
COPY --from=builder /jeha /jeha
ENTRYPOINT ["/jeha"]
CMD ["run", "--config", "/config.toml"]
