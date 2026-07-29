ARG BASE_IMAGE=debian:stable-slim

FROM ${BASE_IMAGE}

ARG TARGETARCH
COPY dist/${TARGETARCH}/dcd /usr/local/bin/dcd

ENTRYPOINT ["/usr/local/bin/dcd"]
CMD ["--help"]
