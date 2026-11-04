#!/usr/bin/env bash
# Script test: scripts/check-parametrised.sh passes for parametrised, helper-driven and marked
# tests and fails for an unmarked one, an empty marker reason and a marker that is too far away.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
GATE="$PWD/scripts/check-parametrised.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() { rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/rusty-store/tests" "$tmp/t/crates/rusty-directory/tests"
  printf '//! ok\n#[tokio::test]\nasync fn a() {\n    for_each_env(|e| async move {\n        let _ = e;\n    })\n    .await;\n}\n' > "$tmp/t/crates/rusty-store/tests/a.rs"
  printf '#[tokio::test]\nasync fn d() {\n    for_each_backend(|b| async move { let _ = b; }).await;\n}\n' > "$tmp/t/crates/rusty-directory/tests/d.rs"; }
expect() { # name, expected rc
  (cd "$tmp/t" && bash "$GATE" "$tmp/t" >/dev/null 2>&1); local rc=$?
  if [ "$rc" -ne "$2" ]; then echo "parametrised-gate: '$1': expected rc $2, got $rc" >&2; exit 1; fi; }
mk; expect "clean tree" 0
mk; printf '#[tokio::test]\nasync fn bare() {\n    assert_eq!(1, 1);\n}\n' >> "$tmp/t/crates/rusty-store/tests/a.rs"; expect "unmarked test" 1
mk; printf '#[test]\nfn bare() {\n    assert_eq!(1, 1);\n}\n' >> "$tmp/t/crates/rusty-store/tests/a.rs"; expect "unmarked sync test" 1
mk; printf '// backend-neutral: pure unit test\n#[test]\nfn bare() {\n    assert_eq!(1, 1);\n}\n' >> "$tmp/t/crates/rusty-store/tests/a.rs"; expect "marker above the attribute" 0
mk; printf '#[test]\n// backend-neutral: pure unit test\nfn bare() {\n    assert_eq!(1, 1);\n}\n' >> "$tmp/t/crates/rusty-store/tests/a.rs"; expect "marker between attribute and fn" 0
mk; printf '#[test]\n// backend-neutral:\nfn bare() {\n    assert_eq!(1, 1);\n}\n' >> "$tmp/t/crates/rusty-store/tests/a.rs"; expect "empty reason does not count" 1
mk; printf '// backend-neutral: far away\n\n\n\n\n#[test]\nfn bare() {\n    assert_eq!(1, 1);\n}\n' >> "$tmp/t/crates/rusty-store/tests/a.rs"; expect "marker too far above" 1
mk; printf '//! backend-neutral: whole file\n#[test]\nfn bare() {\n    assert_eq!(1, 1);\n}\n' > "$tmp/t/crates/rusty-store/tests/b.rs"; expect "file-level marker" 0
mk; printf '//! backend-neutral:\n#[test]\nfn bare() {\n    assert_eq!(1, 1);\n}\n' > "$tmp/t/crates/rusty-store/tests/b.rs"; expect "empty file-level reason" 1
mk; printf 'async fn helper() {\n    for_each_env(|e| async move { let _ = e; }).await;\n}\n#[tokio::test]\nasync fn viahelper() {\n    helper().await;\n}\n' > "$tmp/t/crates/rusty-store/tests/b.rs"; expect "shared helper that runs the per-backend iterator" 0
mk; printf 'async fn helper() {\n    println!("x");\n}\n#[tokio::test]\nasync fn viahelper() {\n    helper().await;\n}\n' > "$tmp/t/crates/rusty-store/tests/b.rs"; expect "helper without the iterator" 1
mk; printf 'async fn helper(n: u8) {\n    for_each_env(|e| async move { let _ = e; }).await;\n}\nproptest! {\n    #[test]\n    fn prop(n in 0u8..4) {\n        block_on(helper(n));\n    }\n}\n' > "$tmp/t/crates/rusty-store/tests/b.rs"; expect "proptest through a helper" 0
mk; printf 'proptest! {\n    #[test]\n    fn prop(n in 0u8..4) {\n        assert!(n < 4);\n    }\n}\n' > "$tmp/t/crates/rusty-store/tests/b.rs"; expect "unmarked proptest" 1
mk; printf '#[tokio::test]\nasync fn single() { for_each_backend(|b| async move { let _ = b; }).await; }\n' > "$tmp/t/crates/rusty-store/tests/b.rs"; expect "one-line fn" 0
echo "parametrised-gate: OK"
