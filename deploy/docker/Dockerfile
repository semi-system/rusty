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
#    compiles to the TARGET platform's musl triple with zig as the C compiler/linker (ring is the
#    only C code; zig ships musl headers, so the result is fully static).
#  * Every base image is pinned by digest (deploy/images.lock; scripts/check-docker-pins.sh keeps
#    this file and the lock in step). zig is pinned by SHA-256.
#  * Dependencies are resolved with `--locked`: a stale Cargo.lock fails the build.
#  * The final image is baseless "static" (ghcr.io/semi-system/baseless-static): no shell, no package manager, a numeric non-root user,
#    CA certificates, nothing else. It runs with `--read-only` (data lives in the two volumes).

# Which image the build ends with (a stage name below): `--build-arg SERVICE=rusty-smtp-in` builds that service image as the default target, which is how CI publishes one image per service.
ARG SERVICE=rusty

# ---- front end stage -------------------------------------------------------------------------
# The admin web (web/admin): installed from the lockfile with no package script, checked by the same
# gates as CI (dependency allow-list, types current, bundle scan) and built. Runs on the BUILD platform:
# the output is plain files, the same for every target. The build is reproducible (scripts/ci-web.sh).
FROM --platform=$BUILDPLATFORM node:22-alpine3.22@sha256:cd7807368cf24826297cbad5dca1a44972ccfd770647db52a8c7589eb4599ac8 AS web
WORKDIR /src/web/admin
COPY web/admin/package.json web/admin/package-lock.json ./
RUN npm ci --ignore-scripts --no-audit --no-fund
COPY web/admin/ ./
COPY crates/rusty-admin/contract /src/crates/rusty-admin/contract
RUN node scripts/check-deps.mjs && node scripts/gen-types.mjs --check && npx vite build && node scripts/check-bundle.mjs

# ---- compile stage ---------------------------------------------------------------------------
# Rust version = rust-toolchain.toml `channel` (checked by scripts/check-docker-pins.sh).
FROM --platform=$BUILDPLATFORM rust:1.99.1-alpine3.22@sha256:9a744dc47d1ac2a7d17fc3cedfc8b4928b1696dfcba3274ab25bfb40988a9ff0 AS build

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
# adds from its own self-contained directory are dropped (they would define _start twice), as is the
# GNU-ld-only Cortex-A53 erratum flag rustc passes for aarch64 (zig's lld rejects it).
COPY --chmod=0755 <<'EOF' /usr/local/bin/zigld
#!/bin/sh
n=$#; i=0
while [ "$i" -lt "$n" ]; do a=$1; shift; i=$((i+1)); case "$a" in */self-contained/*.o|--target=*|-Wl,--fix-cortex-a53-843419) ;; *) set -- "$@" "$a" ;; esac; done
exec /opt/zig/zig cc -target "$(cat /ztarget)" -fno-sanitize=undefined "$@"
EOF

WORKDIR /src
COPY . .
# The admin service embeds the built front end (rusty-admin/build.rs); RUSTY_ADMIN_WEB_REQUIRED=1 below makes a build without it an error.
COPY --from=web /src/web/admin/dist /src/web/admin/dist
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
    RUSTY_ADMIN_WEB_REQUIRED=1 cargo build --release --locked --target "$triple" -p rusty $feat -p rusty-admin --target-dir /out; \
    install -D "/out/$triple/release/rusty" /rootfs/usr/local/bin/rusty; \
    ln -s rusty /rootfs/usr/local/bin/rustyd; ln -s rusty /rootfs/usr/local/bin/rustyctl; \
    mkdir -p /rootfs/var/lib/rusty /rootfs/etc/rusty; \
    chmod 0700 /rootfs/var/lib/rusty; chmod 0755 /rootfs/etc/rusty; \
    install -D "/out/$triple/release/rusty-admin" /rootfs-admin/usr/local/bin/rusty-admin; \
    mkdir -p /rootfs-admin/etc/rusty; chmod 0755 /rootfs-admin/etc/rusty; \
    for svc in smtp-in submission imap outbound gc; do \
      install -D "/out/$triple/release/rusty-$svc" "/rootfs-$svc/usr/local/bin/rusty-$svc"; \
      mkdir -p "/rootfs-$svc/etc/rusty"; chmod 0755 "/rootfs-$svc/etc/rusty"; \
    done; \
    find /rootfs /rootfs-admin /rootfs-smtp-in /rootfs-submission /rootfs-imap /rootfs-outbound /rootfs-gc -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

# ---- release artefact stage ------------------------------------------------------------------
# `docker buildx build --target binary --output type=local,dest=out .` exports just the binary
# (release.yml). Not the default target: the last stage below is.
FROM scratch AS binary
COPY --from=build /rootfs/usr/local/bin/rusty /rusty

# ---- admin web image (docker build --target admin) -------------------------------------------
# One service, one image (image design): the admin web, its API and the embedded front end in one static
# binary. Same base as the mail image; it holds no mail data and needs no volume. TLS ends at the edge
# (listen address and public origin come from /etc/rusty/admin.toml). Not the default target: the last
# stage below is.
FROM ghcr.io/semi-system/baseless-static@sha256:0818dee927a7fdd28e358e3337c891b878ebc58e9e20094d236ae3fe03a09f1b AS admin

ARG VERSION=dev
ARG REVISION=unknown
ARG SOURCE_URL=
ARG CREATED=

LABEL org.opencontainers.image.title="rusty-admin" \
      org.opencontainers.image.description="Rusty admin web (API and front end) as one static binary" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.source="$SOURCE_URL" \
      org.opencontainers.image.created="$CREATED" \
      org.opencontainers.image.base.name="ghcr.io/semi-system/baseless-static"

COPY --from=build --chown=65532:65532 /rootfs-admin/etc/rusty /etc/rusty
COPY --from=build /rootfs-admin/usr/local/bin/ /usr/local/bin/

USER 65532:65532
WORKDIR /etc/rusty
VOLUME ["/etc/rusty"]
EXPOSE 8080
STOPSIGNAL SIGTERM
# Exec form, no shell: `rusty-admin healthcheck` asks the service's own listener (a 200 or 401 from /api/v1/session means up).
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/rusty-admin", "healthcheck", "--config", "/etc/rusty/admin.toml"]
# The setup token (first start with no administrator) is the one line on standard output; read it with `docker logs`.
ENTRYPOINT ["/usr/local/bin/rusty-admin"]
CMD ["--config", "/etc/rusty/admin.toml"]

# ---- one image per service (docs/superpowers/plans/2026-11-14-per-service-images-design.md) ------------------------------
# `docker build --target rusty-imap`, and so on: one static binary, no shell, numeric non-root user, a configuration file of its own that accepts only the sections the
# service uses (`/etc/rusty/<service>.toml`). Services with a port have the HEALTHCHECK of that port; outbound and gc have no port and their check only loads the configuration.
FROM ghcr.io/semi-system/baseless-static@sha256:0818dee927a7fdd28e358e3337c891b878ebc58e9e20094d236ae3fe03a09f1b AS rusty-smtp-in

ARG VERSION=dev
ARG REVISION=unknown
ARG SOURCE_URL=
ARG CREATED=

LABEL org.opencontainers.image.title="rusty-smtp-in" \
      org.opencontainers.image.description="Rusty unauthenticated inbound mail (port 25)." \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.source="$SOURCE_URL" \
      org.opencontainers.image.created="$CREATED" \
      org.opencontainers.image.base.name="ghcr.io/semi-system/baseless-static"

COPY --from=build --chown=65532:65532 /rootfs-smtp-in/etc/rusty /etc/rusty
COPY --from=build /rootfs-smtp-in/usr/local/bin/ /usr/local/bin/

USER 65532:65532
WORKDIR /etc/rusty
VOLUME ["/etc/rusty"]
EXPOSE 25
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/rusty-smtp-in", "healthcheck", "--config", "/etc/rusty/smtp-in.toml"]
ENTRYPOINT ["/usr/local/bin/rusty-smtp-in"]
CMD ["--config", "/etc/rusty/smtp-in.toml"]

FROM ghcr.io/semi-system/baseless-static@sha256:0818dee927a7fdd28e358e3337c891b878ebc58e9e20094d236ae3fe03a09f1b AS rusty-submission

ARG VERSION=dev
ARG REVISION=unknown
ARG SOURCE_URL=
ARG CREATED=

LABEL org.opencontainers.image.title="rusty-submission" \
      org.opencontainers.image.description="Rusty authenticated submission (ports 587 and 465)." \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.source="$SOURCE_URL" \
      org.opencontainers.image.created="$CREATED" \
      org.opencontainers.image.base.name="ghcr.io/semi-system/baseless-static"

COPY --from=build --chown=65532:65532 /rootfs-submission/etc/rusty /etc/rusty
COPY --from=build /rootfs-submission/usr/local/bin/ /usr/local/bin/

USER 65532:65532
WORKDIR /etc/rusty
VOLUME ["/etc/rusty"]
EXPOSE 587 465
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/rusty-submission", "healthcheck", "--config", "/etc/rusty/submission.toml"]
ENTRYPOINT ["/usr/local/bin/rusty-submission"]
CMD ["--config", "/etc/rusty/submission.toml"]

FROM ghcr.io/semi-system/baseless-static@sha256:0818dee927a7fdd28e358e3337c891b878ebc58e9e20094d236ae3fe03a09f1b AS rusty-imap

ARG VERSION=dev
ARG REVISION=unknown
ARG SOURCE_URL=
ARG CREATED=

LABEL org.opencontainers.image.title="rusty-imap" \
      org.opencontainers.image.description="Rusty IMAP (ports 143 and 993)." \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.source="$SOURCE_URL" \
      org.opencontainers.image.created="$CREATED" \
      org.opencontainers.image.base.name="ghcr.io/semi-system/baseless-static"

COPY --from=build --chown=65532:65532 /rootfs-imap/etc/rusty /etc/rusty
COPY --from=build /rootfs-imap/usr/local/bin/ /usr/local/bin/

USER 65532:65532
WORKDIR /etc/rusty
VOLUME ["/etc/rusty"]
EXPOSE 143 993
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/rusty-imap", "healthcheck", "--config", "/etc/rusty/imap.toml"]
ENTRYPOINT ["/usr/local/bin/rusty-imap"]
CMD ["--config", "/etc/rusty/imap.toml"]

FROM ghcr.io/semi-system/baseless-static@sha256:0818dee927a7fdd28e358e3337c891b878ebc58e9e20094d236ae3fe03a09f1b AS rusty-outbound

ARG VERSION=dev
ARG REVISION=unknown
ARG SOURCE_URL=
ARG CREATED=

LABEL org.opencontainers.image.title="rusty-outbound" \
      org.opencontainers.image.description="Rusty the outbound delivery worker (no listening port)." \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.source="$SOURCE_URL" \
      org.opencontainers.image.created="$CREATED" \
      org.opencontainers.image.base.name="ghcr.io/semi-system/baseless-static"

COPY --from=build --chown=65532:65532 /rootfs-outbound/etc/rusty /etc/rusty
COPY --from=build /rootfs-outbound/usr/local/bin/ /usr/local/bin/

USER 65532:65532
WORKDIR /etc/rusty
VOLUME ["/etc/rusty"]
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/rusty-outbound", "healthcheck", "--config", "/etc/rusty/outbound.toml"]
ENTRYPOINT ["/usr/local/bin/rusty-outbound"]
CMD ["--config", "/etc/rusty/outbound.toml"]

FROM ghcr.io/semi-system/baseless-static@sha256:0818dee927a7fdd28e358e3337c891b878ebc58e9e20094d236ae3fe03a09f1b AS rusty-gc

ARG VERSION=dev
ARG REVISION=unknown
ARG SOURCE_URL=
ARG CREATED=

LABEL org.opencontainers.image.title="rusty-gc" \
      org.opencontainers.image.description="Rusty garbage collection and periodic jobs (no listening port)." \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.source="$SOURCE_URL" \
      org.opencontainers.image.created="$CREATED" \
      org.opencontainers.image.base.name="ghcr.io/semi-system/baseless-static"

COPY --from=build --chown=65532:65532 /rootfs-gc/etc/rusty /etc/rusty
COPY --from=build /rootfs-gc/usr/local/bin/ /usr/local/bin/

USER 65532:65532
WORKDIR /etc/rusty
VOLUME ["/etc/rusty"]
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/rusty-gc", "healthcheck", "--config", "/etc/rusty/gc.toml"]
ENTRYPOINT ["/usr/local/bin/rusty-gc"]
CMD ["--config", "/etc/rusty/gc.toml"]

# ---- final stage -----------------------------------------------------------------------------
# baseless static: CA certificates, tzdata, /tmp, no shell, no package manager. Default user 65532.
FROM ghcr.io/semi-system/baseless-static@sha256:0818dee927a7fdd28e358e3337c891b878ebc58e9e20094d236ae3fe03a09f1b AS rusty

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
      org.opencontainers.image.base.name="ghcr.io/semi-system/baseless-static"

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

# ---- the default target ------------------------------------------------------------------------
# `docker build .` gives the multi-call mail image (SERVICE=rusty); CI builds each service image with `--build-arg SERVICE=<stage>`.
FROM ${SERVICE} AS selected
