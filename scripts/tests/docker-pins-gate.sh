#!/usr/bin/env bash
# Self-test of scripts/check-docker-pins.sh on a scratch tree.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
D1=$(printf 'a%.0s' $(seq 64)); D2=$(printf 'b%.0s' $(seq 64)); D3=$(printf 'c%.0s' $(seq 64))
SHA=$(printf '1%.0s' $(seq 40))
mk() {
  rm -rf "$tmp/t"; mkdir -p "$tmp/t/deploy/docker" "$tmp/t/.github/workflows"
  printf 'channel = "1.99.1"\n' > "$tmp/t/rust-toolchain.toml"
  printf '# lock\nrust:1.99.1-alpine  rust@sha256:%s  2026-11-10\ndistroless  gcr.io/x@sha256:%s  2026-11-10\npostgres:18  postgres@sha256:%s  2026-11-10\n' "$D1" "$D2" "$D3" > "$tmp/t/deploy/images.lock"
  printf 'FROM --platform=$BUILDPLATFORM rust:1.99.1-alpine@sha256:%s AS build\nARG ZIG_SHA256_X86_64=%s\nFROM scratch AS binary\nCOPY --from=build /a /a\nFROM gcr.io/x:nonroot@sha256:%s\nCOPY --from=build /a /b\n' "$D1" "$D1" "$D2" > "$tmp/t/deploy/docker/Dockerfile"
  printf 'name: t\non: [push]\npermissions: {}\njobs:\n  a:\n    runs-on: ubuntu-24.04\n    services:\n      pg:\n        image: postgres@sha256:%s\n    steps:\n      - uses: actions/checkout@%s # v1.2.3\n      - uses: ./local\n' "$D3" "$SHA" > "$tmp/t/.github/workflows/a.yml"
}
expect() { local want="$2" rc=0; bash "$here/scripts/check-docker-pins.sh" "$tmp/t" >/dev/null 2>&1 || rc=$?; [ "$rc" = "$want" ] || { echo "FAIL: $1 (rc=$rc want=$want)" >&2; exit 1; }; }
wf() { echo "$tmp/t/.github/workflows/a.yml"; }
df() { echo "$tmp/t/deploy/docker/Dockerfile"; }

mk; expect "clean tree" 0
mk; sed -i 's/^FROM gcr.io\/x:nonroot@sha256:.*/FROM gcr.io\/x:nonroot/' "$(df)"; expect "base image without digest" 1
mk; sed -i "s/^FROM gcr.io.*/FROM gcr.io\/x:nonroot@sha256:$(printf 'd%.0s' $(seq 64))/" "$(df)"; expect "digest missing from images.lock" 1
mk; sed -i 's/rust:1.99.1-alpine/rust:1.98.0-alpine/' "$(df)"; expect "rust tag differs from rust-toolchain.toml" 1
mk; sed -i '/ZIG_SHA256/d' "$(df)"; expect "no zig checksum" 1
mk; sed -i 's/ZIG_SHA256_X86_64=.*/ZIG_SHA256_X86_64=abc/' "$(df)"; expect "malformed zig checksum" 1
mk; sed -i 's/@1111111111111111111111111111111111111111 # v1.2.3/@v4/' "$(wf)"; expect "action pinned by tag" 1
mk; sed -i 's/ # v1.2.3//' "$(wf)"; expect "action pinned by SHA without version comment" 1
mk; sed -i 's/^permissions: {}/permissions: read-all/' "$(wf)"; expect "no empty top-level permissions" 1
mk; sed -i 's/^on: \[push\]/on:\n  pull_request_target:/' "$(wf)"; expect "pull_request_target" 1
mk; sed -i 's/image: postgres@sha256:.*/image: postgres:18/' "$(wf)"; expect "service image without digest" 1
mk; sed -i "s/image: postgres@sha256:.*/image: postgres@sha256:$(printf 'e%.0s' $(seq 64))/" "$(wf)"; expect "service digest not in images.lock" 1
echo "docker-pins gate self-test OK"
