#!/usr/bin/env sh
# Builds the front end twice from a clean output directory and compares the hashes of every file: the same input must give the same bytes
# (design section 8). Run from web/admin after `npm ci --ignore-scripts`.
set -eu
cd "$(dirname "$0")/.."
sum() { (cd dist && find . -type f | LC_ALL=C sort | while IFS= read -r f; do sha256sum "$f"; done); }
rm -rf dist && npx vite build >/dev/null && first="$(sum)"
rm -rf dist && npx vite build >/dev/null && second="$(sum)"
if [ "$first" != "$second" ]; then
  echo "the build is not reproducible:" >&2
  printf '%s\n%s\n' "$first" "$second" | sort | uniq -u >&2
  exit 1
fi
echo "build reproducible: $(printf '%s\n' "$first" | wc -l) files"
