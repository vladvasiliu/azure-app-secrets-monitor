ARG RUST_VERSION="1.70.0"
ARG DEBIAN_VERSION="bullseye"

ARG VERSION="v0.0.1"


FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} as builder

ARG VERSION

WORKDIR /code
COPY Cargo.toml Cargo.lock /code/
COPY src /code/src/

SHELL ["/bin/bash", "-c", "-o", "pipefail"]
# remove leading v from version number and use it as the crate version
RUN CRATE_VERSION=$(echo ${VERSION} | sed "s/v\(.*\)/\1/") &&\
    sed -ri "s/^version = \".*\"/version = \"${CRATE_VERSION}\"/" Cargo.toml &&\
    # Fix crates.io update bug on aarch64
    cargo --config net.git-fetch-with-cli=true build --release


# hadolint ignore=DL3007
FROM gcr.io/distroless/cc-debian11:latest

LABEL org.opencontainers.image.title="Azure App Secrets Monitor"
LABEL org.opencontainers.image.description="Exports Azure App Secrets expiration dates as Prometheus metrics."
LABEL org.opencontainers.image.source="https://github.com/vladvasiliu/azure-app-secrets-monitor"
LABEL org.opencontainers.image.authors="Vlad Vasiliu"

EXPOSE 9912
WORKDIR /


COPY --from=builder /code/target/release/azure-app-secrets-monitor /

CMD ["/azure-app-secrets-monitor"]
