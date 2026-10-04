# syntax=docker/dockerfile:1.7

# Final runtime image. Overridable so a flow-capable agent can build on a glibc
# base (distroless/cc) that the Lightpanda/v8 engine needs; default stays a
# minimal static-distroless for the control plane and non-flow agents.
ARG FINAL_IMAGE=gcr.io/distroless/static-debian12:nonroot

# Alpine builder = musl libc native → static binaries without crt-static gymnastics.
# cargo-chef splits dependency compile (slow, rarely invalidated) from app compile
# (fast, invalidated on every src change). Unchanged-deps rebuilds drop from ~5min
# to ~30s.
FROM rust:1-alpine AS chef
WORKDIR /usr/src/uptimepage
ENV CARGO_TERM_COLOR=never
# `curl` is needed at build time: utoipa-swagger-ui's build script shells out
# to it to fetch the Swagger UI assets bundle. `bash` runs scripts/fetch-
# tailwind.sh (which detects musl libc and downloads the matching upstream
# asset, so no glibc shim is required).
# `mold` = fast linker for the app/dep compile (cargo-chef already caches the
# dep layer, so sccache would be redundant here — and its cache mount isn't
# carried by `cache-to: type=gha`, so it'd be cold every CI run = pure tax).
# RUSTFLAGS is set in the `chef` base so every descendant compile stage
# inherits it (the build context doesn't COPY .cargo/).
RUN apk add --no-cache musl-dev pkgconfig curl bash mold
ENV RUSTFLAGS="-Clink-arg=-fuse-ld=mold"
# In the base so the cook and build stages share one profile; dev overrides for speed.
ARG CARGO_PROFILE_RELEASE_LTO=fat
ARG CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
ARG CARGO_PROFILE_RELEASE_OPT_LEVEL=3
ENV CARGO_PROFILE_RELEASE_LTO=${CARGO_PROFILE_RELEASE_LTO} \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CARGO_PROFILE_RELEASE_CODEGEN_UNITS} \
    CARGO_PROFILE_RELEASE_OPT_LEVEL=${CARGO_PROFILE_RELEASE_OPT_LEVEL}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-chef --locked --version ^0.1

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /usr/src/uptimepage/recipe.json recipe.json
# Cook dependencies only — this layer caches as long as Cargo.toml/lock + the
# bench/bin shape don't change.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/uptimepage/target \
    cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
COPY migrations ./migrations
COPY config ./config
COPY static ./static
COPY assets ./assets
COPY templates ./templates
COPY locales ./locales
# build.rs runs scripts/fetch-tailwind.sh then bakes static/css/app.css.
# legal.rs and docs.rs `include_str!` the markdown under docs/, plus
# THIRD-PARTY-LICENSES.md, and i18n.rs the catalogues under locales/, at
# compile time, so those files must be in the build context here (the
# planner/cook stage stays deps-only — it never compiles the local crate).
COPY build.rs ./
COPY scripts ./scripts
COPY docs ./docs
COPY THIRD-PARTY-LICENSES.md ./
# AGPL-3.0 §13: bake the exact source identity so the running binary's
# footer offers the Corresponding Source. CI passes the commit/repo; an
# empty default lets a bare `docker build` fall back to build.rs's git
# probe (and to upstream) without failing.
ARG SM_SOURCE_COMMIT=
ARG SM_SOURCE_URL=
ENV SM_SOURCE_COMMIT=${SM_SOURCE_COMMIT} \
    SM_SOURCE_URL=${SM_SOURCE_URL}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/uptimepage/target \
    cargo build --release --bins && \
    mkdir -p /out && \
    cp target/release/uptimepage /out/uptimepage && \
    cp target/release/loadtest /out/loadtest

# distroless-static is ~2 MB and ships ca-certificates + /etc/passwd → enough for
# a static Rust binary that talks HTTPS via rustls-native-certs.
# Optional flow-monitor engine. Off by default so the control-plane image ships
# no browser. A flow-capable agent builds with `--build-arg WITH_LIGHTPANDA=true`
# plus the glibc base (`--build-arg FINAL_IMAGE=gcr.io/distroless/cc-debian12:nonroot`).
# The binary is picked per TARGETARCH so the image runs on both amd64 and arm64
# hosts. Fetched from our own release mirror, not upstream: Lightpanda publishes
# only a rolling `nightly` tag, so pulling it directly would break the build the
# moment upstream re-rolls (checksum mismatch). The mirror holds the exact pinned
# bytes; bump the tag + both checksums together to move to a newer engine. The
# stable 0.3.x line predates the runtime egress flags the engine passes, so we
# track nightly rather than a stable release.
FROM alpine:3.21 AS lightpanda
ARG WITH_LIGHTPANDA=false
ARG TARGETARCH
ARG LIGHTPANDA_MIRROR_TAG=lightpanda-mirror-2026-07-09
ARG LIGHTPANDA_SHA256_AMD64=55b358a7a3bcabcfb7ab5038708f035e20713ebcbb699dcda864035048ffd455
ARG LIGHTPANDA_SHA256_ARM64=2937ae335d2790bb316c59312d141f123a400f5db8bef4f7dbd5f8fb6d92917a
RUN mkdir -p /lp && \
    if [ "$WITH_LIGHTPANDA" = "true" ]; then \
        apk add --no-cache curl && \
        case "$TARGETARCH" in \
          amd64) asset=lightpanda-x86_64-linux;  sum="$LIGHTPANDA_SHA256_AMD64" ;; \
          arm64) asset=lightpanda-aarch64-linux; sum="$LIGHTPANDA_SHA256_ARM64" ;; \
          *) echo "no lightpanda build for arch: ${TARGETARCH:-unknown}" >&2; exit 1 ;; \
        esac && \
        curl -fsSL -o /lp/lightpanda \
          "https://github.com/uptimepage/uptimepage/releases/download/${LIGHTPANDA_MIRROR_TAG}/${asset}" && \
        echo "${sum}  /lp/lightpanda" | sha256sum -c - && \
        chmod 0755 /lp/lightpanda ; \
    else \
        touch /lp/.keep ; \
    fi

FROM ${FINAL_IMAGE}
WORKDIR /app

COPY --from=builder /out/uptimepage /usr/local/bin/uptimepage
COPY --from=builder /out/loadtest /usr/local/bin/loadtest
COPY --from=builder /usr/src/uptimepage/config /app/config
COPY --from=builder /usr/src/uptimepage/migrations /app/migrations
# The Lightpanda binary when WITH_LIGHTPANDA=true; otherwise just a .keep marker.
COPY --from=lightpanda /lp/ /usr/local/bin/

ENV UPTIMEPAGE_SERVER__API_BIND=0.0.0.0:8080 \
    UPTIMEPAGE_SERVER__METRICS_BIND=0.0.0.0:9090

EXPOSE 8080 9090
USER nonroot
ENTRYPOINT ["/usr/local/bin/uptimepage"]
