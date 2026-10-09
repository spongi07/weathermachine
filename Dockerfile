# Weather Machine — production image.
#   build stage : Rust service (release) + Rust/WebAssembly dashboard
#   final stage : distroless (glibc + CA certificates only), non-root, no shell
#
# Build:   docker build -t weather-machine .
# Run:     docker run --rm -p 8080:8080 weather-machine demo
# Deploy:  see docker-compose.yml and docs/deployment/portainer.md
#
# Nothing is pulled from Docker Hub, whose anonymous pull limit stopped CI on
# 9 October 2026: the Rust image comes from Google's mirror of Docker Hub
# (mirror.gcr.io), and BuildKit's built-in Dockerfile frontend is used (no
# `# syntax` image; it reads `RUN --mount` too).

ARG RUST_VERSION=1.94.1
ARG WASM_BINDGEN_VERSION=0.2.129

FROM mirror.gcr.io/library/rust:${RUST_VERSION}-trixie AS build
ARG WASM_BINDGEN_VERSION
ENV CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    CARGO_NET_RETRY=10
# cmake/clang: native build of the aws-lc TLS crypto provider.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Toolchain (components + wasm32 target) exactly as pinned in rust-toolchain.toml.
COPY rust-toolchain.toml ./
RUN rustup toolchain install && rustup target list --installed
# wasm-bindgen CLI must equal the wasm-bindgen crate version pinned in ui/Cargo.toml.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo install wasm-bindgen-cli --version "${WASM_BINDGEN_VERSION}" --locked
COPY . .
# The commit the image is built from (CI passes it): the dashboard and the
# startup log show the version as "0.1.0+<commit>".
ARG WM_GIT_SHA=""
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    --mount=type=cache,target=/src/ui/target,sharing=locked \
    cargo build --release --locked -p wm-app \
 && mkdir -p /out/data/models /out/data/research \
 && touch /out/data/models/.keep /out/data/research/.keep \
 && cp target/release/weather-machine /out/weather-machine \
 && ./ui/build.sh \
 && cp -r ui/dist /out/ui \
 && /out/weather-machine --version

FROM gcr.io/distroless/cc-debian13:nonroot
LABEL org.opencontainers.image.title="Weather Machine" \
      org.opencontainers.image.description="Automated research and paper trading of Polymarket daily-high temperature markets (Rust)" \
      org.opencontainers.image.source="https://github.com/spongi07/weathermachine" \
      org.opencontainers.image.licenses="LicenseRef-Proprietary"
WORKDIR /app
COPY --from=build /out/weather-machine /usr/local/bin/weather-machine
COPY --from=build /out/ui /app/ui
COPY configs /app/configs
# Writable data directories (models, research inputs/outputs) owned by the
# runtime user; a fresh named volume mounted at /data inherits them.
COPY --from=build --chown=65532:65532 /out/data /data
ENV WM_CONFIG=/app/configs/weather-machine.toml \
    WM_UI_DIR=/app/ui \
    WM_HTTP_BIND=0.0.0.0:8080 \
    WM_LOG_FORMAT=json
USER nonroot:nonroot
EXPOSE 8080
# No shell or curl in the image: the binary probes its own /healthz.
HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
  CMD ["/usr/local/bin/weather-machine", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/weather-machine"]
CMD ["run"]
