#!/usr/bin/env bash
# Install pinned cargo tools into a private root so the CI can cache the binaries:
#   CI_TOOLS_ROOT=$HOME/.ci-tools bash scripts/ci-tool.sh cargo-deny cargo-audit
# Versions come from scripts/ci-tools.txt. A tool whose recorded version already matches is not
# rebuilt (compiling cargo-deny takes more than two minutes). `cargo install --locked` verifies the
# crates.io checksums of every dependency from the registry index. The caller puts
# $CI_TOOLS_ROOT/bin on PATH. Test hook: CI_TOOL_INSTALL_CMD replaces `cargo install` (prints args).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
ROOT="${CI_TOOLS_ROOT:?set CI_TOOLS_ROOT}"
[ "$#" -ge 1 ] || { echo "usage: ci-tool.sh TOOL..." >&2; exit 2; }
mkdir -p "$ROOT/bin" "$ROOT/versions"
for tool in "$@"; do
  ver="$(awk -v t="$tool" '$1 == t { print $2 }' scripts/ci-tools.txt)"
  [[ "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "ci-tool: '$tool' is not pinned in scripts/ci-tools.txt" >&2; exit 1; }
  if [ -x "$ROOT/bin/$tool" ] && [ "$(cat "$ROOT/versions/$tool" 2>/dev/null || true)" = "$ver" ]; then
    echo "ci-tool: $tool $ver already installed (cache hit)"
    continue
  fi
  echo "ci-tool: installing $tool $ver"
  if [ -n "${CI_TOOL_INSTALL_CMD:-}" ]; then
    "$CI_TOOL_INSTALL_CMD" install --locked --root "$ROOT" --version "$ver" "$tool"
    mkdir -p "$ROOT/bin"; [ -e "$ROOT/bin/$tool" ] || : > "$ROOT/bin/$tool"; chmod +x "$ROOT/bin/$tool"
  else
    cargo install --locked --root "$ROOT" --version "$ver" "$tool"
  fi
  printf '%s\n' "$ver" > "$ROOT/versions/$tool"
done
