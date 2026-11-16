#!/usr/bin/env bash
# The images of a release, pinned by digest: for the tag v1.2.3 of REPO, each service image ghcr.io/REPO[-suffix]:1.2.3 is looked up, its keyless cosign signature (made by docker.yml of this repository
# on this tag) is verified, and one line per image is written in the format of deploy/images.lock:
#   ghcr.io/owner/rusty-imap:1.2.3  ghcr.io/owner/rusty-imap@sha256:<digest>  <date>  (release v1.2.3)
# docker.yml and release.yml start together on a tag: this script waits (up to PIN_TIMEOUT_SECS, default 3000) for an image to exist AND to carry its signature, so it cannot pin one that is not finished.
#
#   scripts/pin-release-images.sh TAG REPO OUTFILE          (REPO is owner/name, as in GITHUB_REPOSITORY)
set -euo pipefail
tag="${1:?usage: pin-release-images.sh TAG REPO OUTFILE}"; repo="${2:?}"; out="${3:?}"
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9.-]+)?$ ]] || { echo "pin-release-images: TAG must look like v1.2.3 (the image tag is 1.2.3)" >&2; exit 2; }
[[ "$repo" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || { echo "pin-release-images: REPO is owner/name" >&2; exit 2; }
version="${tag#v}"
base="ghcr.io/${repo,,}"
suffixes=("" -smtp-in -submission -imap -outbound -gc -admin)   # the services of docker.yml
timeout="${PIN_TIMEOUT_SECS:-3000}"; interval="${PIN_INTERVAL_SECS:-30}"
identity="https://github.com/${repo}/.github/workflows/docker.yml@refs/tags/${tag}"
issuer="https://token.actions.githubusercontent.com"
today="$(date -u +%F)"
: > "$out"
deadline=$(( $(date +%s) + timeout ))
for s in "${suffixes[@]}"; do
  ref="$base$s:$version"
  digest=""
  while :; do
    if d="$(docker buildx imagetools inspect "$ref" --format '{{.Manifest.Digest}}' 2>/dev/null)" && [[ "$d" =~ ^sha256:[0-9a-f]{64}$ ]]; then
      # The signature is made after the push: an image without it is not finished.
      if cosign verify --certificate-identity "$identity" --certificate-oidc-issuer "$issuer" "$base$s@$d" >/dev/null 2>&1; then digest="$d"; break; fi
    fi
    [ "$(date +%s)" -lt "$deadline" ] || { echo "pin-release-images: $ref has no image with a valid signature of $identity (waited ${timeout}s)" >&2; exit 1; }
    sleep "$interval"
  done
  printf '%s  %s@%s  %s  (release %s)\n' "$ref" "$base$s" "$digest" "$today" "$tag" >> "$out"
  echo "pin-release-images: $ref = $digest"
done
