FROM scratch

ARG TARGETARCH
ARG TARGETVARIANT

COPY jeha-${TARGETARCH}${TARGETVARIANT} /jeha

ENTRYPOINT ["/jeha"]
CMD ["run", "--config", "/config.toml"]
