# Local patches to mail-auth 0.15.1

Upstream: https://github.com/stalwartlabs/mail-auth (Apache-2.0 OR MIT, see LICENSES/). This directory is the
published 0.15.1 source with the changes below; it replaces the crates.io copy through `[patch.crates-io]` in
the workspace `Cargo.toml`.

Why: the crate's only crypto backend without the aws-lc C stack ("rust-crypto") uses the RustCrypto `rsa` crate,
which carries RUSTSEC-2023-0071 (Marvin timing side channel, no fixed 0.9 release) and has no fixed version
reachable from mail-auth 0.15. Rusty does not accept an advisory ignore for it, so the RSA operations are
moved to `ring` (already in the tree for TLS; constant-time RSA).

Changes (and nothing else):
1. `Cargo.toml`: dependency `rsa` removed, `ring` 0.17 added; feature `rust-crypto` uses `ring` instead of `rsa`;
   feature `generate` = `["rand"]` (Ed25519 key generation only); examples, benches, tests and dev-dependencies
   dropped.
2. `src/crypto/rust_crypto.rs`: `RsaKey` signs with `ring::signature::RsaKeyPair` (PKCS #1 or PKCS #8 input),
   `RsaPublicKey` verifies with `ring::signature` (`RSA_PKCS1_1024_8192_SHA256` / SHA1 legacy variants); small
   DER helpers unwrap an SPKI and read the modulus size. Ed25519, ML-DSA and the hashes are unchanged.
3. `src/dkim/generate.rs`: `DkimKeyPair::generate_rsa` and its test removed (RSA key generation lives in
   `rusty-smtp`, which does not depend on `rsa` either). `src/error.rs`: the `From<rsa::errors::Error>` impl removed.

Upstreaming: replacing `rsa` by `ring` behind the same feature is a candidate pull request. When mail-auth
releases a version whose backend is free of RUSTSEC-2023-0071, drop this directory and the patch entry.
