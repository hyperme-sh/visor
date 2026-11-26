# Debian profile shares the stock distribution baseline used by container/Dockerfile.
ARG CLOUD_BASE=debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
# Native, static Conductor built from the same verified source as desktop packages.
FROM golang:1.26.5-alpine@sha256:0178a641fbb4858c5f1b48e34bdaabe0350a330a1b1149aabd498d0699ff5fb2 AS conductor
RUN apk add --no-cache curl
WORKDIR /source
RUN curl --fail --silent --show-error --location https://codeload.github.com/hyperme-sh/conductor/tar.gz/7545fc2eded4429a2bfa14fdc9b44f8a5f3a6663 -o source.tar.gz && \
    echo '80014b6b65d3eb10f83568043e0e3090db04534034ce11fb27b2f86dda5b5905  source.tar.gz' | sha256sum -c - && \
    tar xzf source.tar.gz --strip-components=1 && \
    CGO_ENABLED=0 go build -buildvcs=false -mod=readonly -trimpath -o /conductor ./cmd/conductor && \
    /conductor help | grep -F capability-review
FROM ghcr.io/goreleaser/nfpm:v2.47.0@sha256:a662cb167d7b6d3a83920c83d76b12d02b8ac5dd2c13e5c62c15270b23f6df0c AS nfpm
# Native musl unit/build lane; no emulation or desktop processes.
FROM rust:1.90.0-alpine3.22@sha256:b4b54b176a74db7e5c68fdfe6029be39a02ccbcfe72b6e5a3e18e2c61b57ae26 AS build
ARG EXPECTED_ARCH
RUN test "$(uname -m)" = "$EXPECTED_ARCH" && rustc -vV | grep "host: ${EXPECTED_ARCH}-unknown-linux-musl"
RUN apk add --no-cache build-base python3
WORKDIR /src
COPY Cargo.toml Cargo.lock LICENSE ./
COPY src src
COPY session session
COPY local-storage local-storage
COPY local-transport local-transport
COPY native/crash/guard native/crash/guard
COPY native/crash/retention native/crash/retention
COPY inverse-client inverse-client
COPY companion companion
COPY shared shared
COPY third_party third_party
COPY tests tests
COPY ci ci
COPY packaging packaging
COPY docs/headless.md docs/headless.md
COPY docs/host-health.md docs/host-health.md
COPY --from=conductor /conductor /bundle/conductor
COPY --from=conductor /source/LICENSE /bundle/conductor-LICENSE
COPY --from=nfpm /usr/bin/nfpm /usr/local/bin/nfpm
RUN cargo tree --locked -p hypermesh-visor --no-default-features --features headless --prefix none > /tmp/dependencies && \
    ! grep -E '^(x11rb|zbus|keyring|secret-service) ' /tmp/dependencies
RUN cargo test --locked -p hypermesh-session --no-default-features --lib && \
    cargo test --locked -p hypermesh-visor --no-default-features --features headless --lib && \
    cargo build --release --locked -p hypermesh-visor --no-default-features --features headless
RUN readelf -l target/release/hypermesh-visor > /tmp/program-headers && ! grep -q INTERP /tmp/program-headers
ARG SOURCE_REVISION
RUN readelf -l /bundle/conductor > /tmp/conductor-headers && ! grep -q INTERP /tmp/conductor-headers
RUN EXPECTED_ARCH="$EXPECTED_ARCH" SOURCE_REVISION="$SOURCE_REVISION" sh packaging/headless/package.sh
# Optional runtime target; retains no compiler, desktop or D-Bus packages.
FROM alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f AS runtime
COPY --from=build /output/*.apk /tmp/
RUN apk add --no-cache ca-certificates && apk add --no-network --allow-untrusted /tmp/*.apk && rm /tmp/*.apk && \
    addgroup -g 1000 visor && adduser -D -u 1000 -G visor visor && \
    mkdir -p /run/visor /var/lib/visor && chown visor:visor /run/visor /var/lib/visor && chmod 0700 /run/visor /var/lib/visor
ENV HYPERMESH_RUNTIME_DIR=/run/visor HYPERMESH_SESSION_SECRET_FILE=/var/lib/visor/session.json XDG_CONFIG_HOME=/var/lib/visor
USER 1000:1000
WORKDIR /var/lib/visor
ENTRYPOINT ["/usr/bin/hypermesh-visor-headless"]

# Cloud worker uses the same headless .deb on stock Debian. No enrollment data
# is a build input. Operators provide authority through existing local paths.
FROM ${CLOUD_BASE} AS cloud-worker
ARG CLOUD_BASE
ARG DEBIAN_SNAPSHOT=20261102T000000Z
ARG SOURCE_REVISION
LABEL org.opencontainers.image.source="https://github.com/hyperme-sh/visor" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.opencontainers.image.base.name="${CLOUD_BASE}" \
      sh.hyperme.visor.debian-snapshot="${DEBIAN_SNAPSHOT}" \
      sh.hyperme.visor.profile="base"
COPY --from=runtime /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /output/*.deb /output/build.txt /output/checksums.txt /tmp/visor-package/
COPY ci/cloud-worker/package-inventory.sh /usr/local/libexec/visor-package-inventory
RUN set -eu; \
    rm -f /etc/apt/sources.list.d/debian.sources; \
    printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s/ trixie main\ndeb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/%s/ trixie-security main\n' \
      "$DEBIAN_SNAPSHOT" "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list; \
    apt-get -o Acquire::ForceIPv4=true -o Acquire::https::Timeout=30 -o APT::Update::Error-Mode=any update; \
    apt-get -o Acquire::ForceIPv4=true -o Acquire::https::Timeout=30 install -y --no-install-recommends ca-certificates; \
    cd /tmp/visor-package; \
    awk '$2 ~ /[.]deb$/ || $2 == "build.txt"' checksums.txt > cloud-checksums.txt; \
    test "$(wc -l < cloud-checksums.txt)" -eq 2; \
    sha256sum -c cloud-checksums.txt; \
    dpkg -i ./*.deb; \
    install -d /usr/share/visor-worker; \
    install -m 0644 build.txt cloud-checksums.txt /usr/share/visor-worker/; \
    groupadd --gid 1000 visor; \
    useradd --uid 1000 --gid visor --home-dir /var/lib/visor --no-create-home --shell /usr/sbin/nologin visor; \
    install -d -o visor -g visor -m 0700 /run/visor /var/lib/visor; \
    rm -rf /var/lib/apt/lists/* /tmp/visor-package; \
    sh /usr/local/libexec/visor-package-inventory
ENV HOME=/var/lib/visor HYPERMESH_RUNTIME_DIR=/run/visor HYPERMESH_SESSION_SECRET_FILE=/var/lib/visor/session.json XDG_CONFIG_HOME=/var/lib/visor
USER 1000:1000
WORKDIR /var/lib/visor
HEALTHCHECK --interval=5s --timeout=3s --start-period=10s CMD ["/usr/bin/hypermesh-visor-headless", "worker-status"]
ENTRYPOINT ["/usr/bin/hypermesh-visor-headless"]

# The coding pack is an operator-built overlay, separately selected from base.
FROM cloud-worker AS cloud-coding
USER root
LABEL sh.hyperme.visor.profile="coding-v1"
RUN set -eu; \
    apt-get -o Acquire::ForceIPv4=true -o Acquire::https::Timeout=30 -o APT::Update::Error-Mode=any update; \
    apt-get -o Acquire::ForceIPv4=true -o Acquire::https::Timeout=30 install -y --no-install-recommends git shellcheck; \
    rm -rf /var/lib/apt/lists/*; \
    sh /usr/local/libexec/visor-package-inventory
USER 1000:1000
RUN bash --version && git --version && shellcheck --version

FROM scratch AS artifacts
COPY --from=build /output/ /
