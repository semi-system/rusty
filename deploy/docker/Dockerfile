# syntax=docker/dockerfile:1.7
#
# Production image of Rusty (multi-call binary `rusty`, also `rustyd` / `rustyctl`).
#
# Build from the repository root:
#   docker build -f deploy/docker/Dockerfile -t rusty:local .
#   docker buildx build --platform linux/amd64,linux/arm64 -f deploy/docker/Dockerfile .
#
# Design (see docs/docker.md):
#  * The compile stage always runs on the BUILD platform (no QEMU for the compile) and cross
#    compiles to the TARGET platform's musl triple with zig as the C compiler/linker (ring and the
#    bundled SQLite are the only C code; zig ships musl headers, so the result is fully static).
#  * Every base image is pinned by digest (deploy/images.lock; scripts/check-docker-pins.sh keeps
#    this file and the lock in step). zig is pinned by SHA-256.
#  * Dependencies are resolved with `--locked`: a stale Cargo.lock fails the build.
#  * The final image is distroless "static": no shell, no package manager, a numeric non-root user,
#    CA certificates, nothing else. It runs with `--read-only` (data lives in the two volumes).

# ---- compile stage ---------------------------------------------------------------------------
# Rust version = rust-toolchain.toml `channel` (checked by scripts/check-docker-pins.sh).
FROM --platform=$BUILDPLATFORM rust:1.99.1-alpine3.22@sha256:0fd356c1b3a83a6a6a2a4113ede8166eac6697a663fa822c04f0c202e63fcc01 AS build

ARG TARGETARCH
# zig 0.15.2 (https://ziglang.org/download/index.json), SHA-256 of the official tarballs.
ARG ZIG_VERSION=0.15.2
ARG ZIG_SHA256_X86_64=02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239
ARG ZIG_SHA256_AARCH64=958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f
# 0: production build without the self-signed dev certificate. 1: keep default features (the
# `dev-cert` feature) so tools/acceptance/local-run.sh can run against the image. Never publish 1.
ARG DEV_CERT=0
# Reproducibility: pass the commit timestamp (git log -1 --format=%ct).
ARG SOURCE_DATE_EPOCH=0

RUN apk add --no-cache curl xz
RUN set -eu; \
    case "$(uname -m)" in \
      x86_64)  zarch=x86_64;  zsha="$ZIG_SHA256_X86_64" ;; \
      aarch64) zarch=aarch64; zsha="$ZIG_SHA256_AARCH64" ;; \
      *) echo "unsupported build host $(uname -m)" >&2; exit 1 ;; \
    esac; \
    f="zig-$zarch-linux-$ZIG_VERSION"; \
    curl -fsSL --proto '=https' --tlsv1.2 -o /tmp/zig.tar.xz "https://ziglang.org/download/$ZIG_VERSION/$f.tar.xz"; \
    echo "$zsha  /tmp/zig.tar.xz" | sha256sum -c -; \
    mkdir /opt/zig; tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1; rm /tmp/zig.tar.xz

# Target triple and zig target for the platform being built.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl  > /triple; echo x86_64-linux-musl  > /ztarget ;; \
      arm64) echo aarch64-unknown-linux-musl > /triple; echo aarch64-linux-musl > /ztarget ;; \
      *) echo "unsupported TARGETARCH '$TARGETARCH'" >&2; exit 1 ;; \
    esac; \
    rustup target add "$(cat /triple)"; \
    printf '#!/bin/sh\nexec /opt/zig/zig ar "$@"\n' > /usr/local/bin/zigar; \
    chmod 0755 /usr/local/bin/zigar

# zig is the C compiler and linker for the target (no sanitizer runtime: -fno-sanitize=undefined).
# Compiler wrapper: cc-rs' own `--target=...` is dropped (zig's -target is authoritative).
COPY --chmod=0755 <<'EOF' /usr/local/bin/zigcc
#!/bin/sh
n=$#; i=0
while [ "$i" -lt "$n" ]; do a=$1; shift; i=$((i+1)); case "$a" in --target=*) ;; *) set -- "$@" "$a" ;; esac; done
exec /opt/zig/zig cc -target "$(cat /ztarget)" -fno-sanitize=undefined "$@"
EOF
# Linker wrapper: zig supplies the C runtime start files for the musl target, so the copies rustc
# adds from its own self-contained directory are dropped (they would define _start twice).
COPY --chmod=0755 <<'EOF' /usr/local/bin/zigld
#!/bin/sh
n=$#; i=0
while [ "$i" -lt "$n" ]; do a=$1; shift; i=$((i+1)); case "$a" in */self-contained/*.o|--target=*) ;; *) set -- "$@" "$a" ;; esac; done
exec /opt/zig/zig cc -target "$(cat /ztarget)" -fno-sanitize=undefined "$@"
EOF

WORKDIR /src
COPY . .
# The toolchain is the one in the base image tag; do not let rustup fetch another.
RUN rm -f rust-toolchain.toml

ENV CARGO_TERM_COLOR=never \
    CARGO_PROFILE_RELEASE_STRIP=true \
    CARGO_PROFILE_RELEASE_DEBUG=false \
    ZIG_GLOBAL_CACHE_DIR=/tmp/zig-cache \
    ZIG_LOCAL_CACHE_DIR=/tmp/zig-cache
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry \
    set -eu; \
    triple="$(cat /triple)"; tu="$(echo "$triple" | tr 'a-z-' 'A-Z_')"; tl="$(echo "$triple" | tr '-' '_')"; \
    export "CARGO_TARGET_${tu}_LINKER=/usr/local/bin/zigld" "CC_${tl}=/usr/local/bin/zigcc" "AR_${tl}=/usr/local/bin/zigar" \
           "CFLAGS_${tl}=-O2" "RUSTFLAGS=-C target-feature=+crt-static --remap-path-prefix=/src=/build --remap-path-prefix=/usr/local/cargo=/cargo"; \
    if [ "$DEV_CERT" = 1 ]; then feat=""; else feat="--no-default-features"; fi; \
    cargo build --release --locked --target "$triple" -p rusty $feat --target-dir /out; \
    install -D "/out/$triple/release/rusty" /rootfs/usr/local/bin/rusty; \
    ln -s rusty /rootfs/usr/local/bin/rustyd; ln -s rusty /rootfs/usr/local/bin/rustyctl; \
    mkdir -p /rootfs/var/lib/rusty /rootfs/etc/rusty; \
    chmod 0700 /rootfs/var/lib/rusty; chmod 0755 /rootfs/etc/rusty; \
    find /rootfs -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

# ---- release artefact stage ------------------------------------------------------------------
# `docker buildx build --target binary --output type=local,dest=out .` exports just the binary
# (release.yml). Not the default target: the last stage below is.
FROM scratch AS binary
COPY --from=build /rootfs/usr/local/bin/rusty /rusty

# ---- final stage -----------------------------------------------------------------------------
# distroless static (Debian 13): CA certificates, tzdata, /tmp, no shell. Default user 65532.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:25ac8f0c3b2334a9eb05452266bd4697864a8c8ef0ef71e5e6480d7f92868329

ARG VERSION=dev
ARG REVISION=unknown
ARG SOURCE_URL=
ARG CREATED=

LABEL org.opencontainers.image.title="rusty" \
      org.opencontainers.image.description="Rusty mail server (SMTP, IMAP) as one static binary" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.source="$SOURCE_URL" \
      org.opencontainers.image.created="$CREATED" \
      org.opencontainers.image.base.name="gcr.io/distroless/static-debian13:nonroot"

# State directory 0700 and the config directory belong to the numeric runtime user, so named
# volumes mounted over them are initialised with the right owner. /etc/rusty is only read.
COPY --from=build --chown=65532:65532 /rootfs/var/lib/rusty /var/lib/rusty
COPY --from=build --chown=65532:65532 /rootfs/etc/rusty /etc/rusty
COPY --from=build /rootfs/usr/local/bin/ /usr/local/bin/

USER 65532:65532
WORKDIR /var/lib/rusty
VOLUME ["/var/lib/rusty", "/etc/rusty"]
# SMTP, submissions, submission, IMAP, IMAPS. Ports below 1024 need `cap_add: NET_BIND_SERVICE`
# (or the sysctl net.ipv4.ip_unprivileged_port_start=0) because the process is not root.
EXPOSE 25 465 587 143 993
STOPSIGNAL SIGTERM
# Exec form, no shell: `rusty healthcheck` probes every enabled listener from the same config.
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/rusty", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/rusty"]
CMD ["d", "--config", "/etc/rusty/rusty.toml"]
