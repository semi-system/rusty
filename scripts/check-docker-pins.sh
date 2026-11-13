#!/usr/bin/env bash
# Supply-chain pin check for the Dockerfile and the GitHub workflows. Usage: check-docker-pins.sh [ROOT]
#  1. every FROM of deploy/docker/Dockerfile names a base image by digest (image:tag@sha256:...) and
#     that digest is listed in deploy/images.lock (stage references and `scratch` are exempt);
#  2. the Rust image tag equals the `channel` of rust-toolchain.toml;
#  3. the zig download is pinned by SHA-256 (ARG ZIG_SHA256_*, 64 hex digits);
#  4. every `uses:` of a third-party action in .github/workflows is pinned by a full 40-hex commit
#     SHA and carries a `# version` comment; every `image:` with a digest is in images.lock and every
#     `image:` is digest-pinned;
#  5. every workflow has top-level `permissions: {}` and none uses pull_request_target.
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
fail=0
bad() { echo "FAIL: $*" >&2; fail=1; }

DOCKERFILE=deploy/docker/Dockerfile
LOCK=deploy/images.lock
[ -f "$DOCKERFILE" ] || { echo "FAIL: $DOCKERFILE missing" >&2; exit 1; }
[ -f "$LOCK" ] || { echo "FAIL: $LOCK missing" >&2; exit 1; }

in_lock() { grep -v '^#' "$LOCK" | grep -qF "sha256:$1"; }

# --- 1 and 2: FROM lines
stages=""
while IFS= read -r line; do
  # FROM [--platform=x] ref [AS name]
  read -ra w <<<"$line"
  ref=""
  for t in "${w[@]:1}"; do case "$t" in --*) ;; *) ref="$t"; break ;; esac; done
  name=""
  for i in "${!w[@]}"; do
    case "${w[$i]}" in [Aa][Ss]) name="${w[$((i + 1))]:-}" ;; esac
  done
  if [ "$ref" != scratch ] && ! grep -qxF "$ref" <<<"$stages"; then
    if [[ "$ref" =~ @sha256:([0-9a-f]{64})$ ]]; then
      in_lock "${BASH_REMATCH[1]}" || bad "$DOCKERFILE: digest of '$ref' is not in $LOCK"
    else
      bad "$DOCKERFILE: base image '$ref' is not pinned by digest"
    fi
    if [[ "$ref" =~ ^rust:([0-9]+\.[0-9]+\.[0-9]+)- ]]; then
      chan="$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)"
      [ "${BASH_REMATCH[1]}" = "$chan" ] || bad "$DOCKERFILE: rust ${BASH_REMATCH[1]} differs from rust-toolchain.toml channel '$chan'"
    fi
  fi
  [ -z "$name" ] || stages="$stages"$'\n'"$name"
done < <(grep -E '^FROM ' "$DOCKERFILE")

# --- 3: zig pin
n=0
while IFS= read -r line; do
  n=$((n + 1))
  [[ "$line" =~ ^ARG\ ZIG_SHA256_[A-Z0-9_]+=[0-9a-f]{64}$ ]] || bad "$DOCKERFILE: malformed zig checksum line: $line"
done < <(grep -E '^ARG ZIG_SHA256_' "$DOCKERFILE")
[ "$n" -ge 1 ] || bad "$DOCKERFILE: no ARG ZIG_SHA256_* pin"

# --- 4 and 5: workflows
shopt -s nullglob
for wf in .github/workflows/*.yml .github/workflows/*.yaml; do
  while IFS= read -r l; do
    spec="${l#*uses:}"; spec="${spec# }"
    ref="${spec%%[[:space:]]*}"
    case "$ref" in ./*) continue ;; esac
    if ! [[ "$ref" =~ @[0-9a-f]{40}$ ]]; then bad "$wf: '$ref' is not pinned by a full commit SHA"; continue; fi
    [[ "$spec" == *"# "* ]] || bad "$wf: '$ref' has no '# version' comment"
  done < <(grep -E '^[[:space:]-]*uses:' "$wf" || true)
  while IFS= read -r l; do
    ref="${l#*image:}"; ref="${ref# }"; ref="${ref%%[[:space:]]*}"
    case "$ref" in *'$'*) continue ;; esac # an action input built from expressions, not a container image
    if [[ "$ref" =~ @sha256:([0-9a-f]{64})$ ]]; then
      in_lock "${BASH_REMATCH[1]}" || bad "$wf: image digest of '$ref' is not in $LOCK"
    else
      bad "$wf: image '$ref' is not pinned by digest"
    fi
  done < <(grep -E '^[[:space:]]+image:[[:space:]]+[^$[:space:]]' "$wf" | grep -v 'image-' || true)
  grep -qE '^permissions: *\{\} *(#.*)?$' "$wf" || bad "$wf: missing top-level 'permissions: {}'"
  if grep -qE '^[[:space:]]*pull_request_target' "$wf"; then bad "$wf: pull_request_target is forbidden"; fi
done

[ "$fail" = 0 ] || exit 1
echo "docker/workflow pins OK"
