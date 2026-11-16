# SPDX-License-Identifier: GPL-3.0-or-later
#
# Porecatu Relay container image (ADR-0005).
#
# Two ways to get to the same runtime image:
#
#   docker build .                         compiles the relay here (default
#                                          target, last stage). Alpine is musl,
#                                          so the binary is static.
#   docker build --target runtime-prebuilt uses the musl binaries the release
#                                          workflow already compiled, laid out
#                                          as dist/amd64/porecatu-relay and
#                                          dist/arm64/porecatu-relay. No
#                                          emulation, so the multi-arch image is
#                                          fast to build.
#
# Both end on distroless/static: no shell, no package manager, CA certificates
# included, non-root user 65532. Port 8080 without TLS, because the typical
# deployment sits behind a reverse proxy (ADR-0004); the baked-in config is
# packaging/docker/relay.toml and expects the token at
# /run/secrets/porecatu-relay-token.
#
# There is no HEALTHCHECK: distroless has no curl. Poll GET /healthz from
# outside (orchestrator).

# --- Build stage: static musl binary ---------------------------------------
# Same Rust version as rust-toolchain.toml; the tag pins it, so the toolchain
# file is not copied (it would make rustup fetch components).
FROM rust:1.98.0-alpine3.24@sha256:a10e64dd139b7387337c7fbe8aca31b959b57b2fd4c8ae20a02cf1d6ea424dce AS builder

RUN apk add --no-cache musl-dev

WORKDIR /src

# Only what the build needs: a COPY of the whole tree would drag in target/
# and the local .cargo/config.toml (a path patch that does not exist here).
COPY Cargo.toml Cargo.lock ./
COPY src ./src

ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build --release --locked \
    && cp target/release/porecatu-relay /porecatu-relay \
    && /porecatu-relay --version

# --- Runtime, from the release workflow's binaries -------------------------
FROM gcr.io/distroless/static:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931 AS runtime-prebuilt

ARG TARGETARCH
COPY --chmod=755 dist/${TARGETARCH}/porecatu-relay /porecatu-relay
COPY packaging/docker/relay.toml /etc/porecatu-relay/relay.toml

USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/porecatu-relay", "--config", "/etc/porecatu-relay/relay.toml"]

# --- Runtime, from the binary compiled above (default target) --------------
FROM gcr.io/distroless/static:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931 AS runtime

COPY --from=builder /porecatu-relay /porecatu-relay
COPY packaging/docker/relay.toml /etc/porecatu-relay/relay.toml

USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/porecatu-relay", "--config", "/etc/porecatu-relay/relay.toml"]
