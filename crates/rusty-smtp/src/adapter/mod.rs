//! Adapters to third-party protocol libraries (see scripts/wrapped-libs.txt; the gate is
//! `scripts/check-wrapped-libs.sh`): `smtp-proto` in `smtp`, the DKIM library in `auth` (signing keys),
//! and in `dns` the resolver, SPF/DKIM/DMARC verification and the pre-seeded record cache. One module
//! per concern, nothing of a library in a public signature.
pub(crate) mod auth;
pub(crate) mod dns;
pub(crate) mod smtp;
