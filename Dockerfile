ARG RUST_VERSION="1.63.0"
ARG DEBIAN_VERSION="bullseye"

ARG VERSION="0.0.1"


FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} as builder

ARG VERSION

WORKDIR /code
COPY . /code

SHELL ["/bin/bash", "-c", "-o", "pipefail"]
# remove leading v from version number and use it as the crate version
RUN CRATE_VERSION=$(echo ${VERSION} | sed "s/v\(.*\)/\1/") &&\
    sed -ri "s/^version = \".*\"/version = \"${CRATE_VERSION}\"/" Cargo.toml &&\
    cargo build --release


FROM debian:${DEBIAN_VERSION}-slim

ARG VERSION
ARG BUILD_DATE
ARG GIT_HASH

LABEL org.opencontainers.image.version="$VERSION"
LABEL org.opencontainers.image.created="$BUILD_DATE"
LABEL org.opencontainers.image.revision="$GIT_HASH"
LABEL org.opencontainers.image.title="Azure App Secrets Monitor"
LABEL org.opencontainers.image.description="Exports Azure App Secrets expiration dates as Prometheus metrics."
LABEL org.opencontainers.image.source="https://github.com/vladvasiliu/azure-app-secrets-monitor"
LABEL org.opencontainers.image.authors="Vlad Vasiliu"

EXPOSE 9912
WORKDIR /

RUN apt-get update && apt-get install --no-install-recommends -y curl=7.74.0-1.3+deb11u2 ca-certificates=20210119 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /code/target/release/azure-app-secrets-monitor /

CMD ["/azure-app-secrets-monitor"]
