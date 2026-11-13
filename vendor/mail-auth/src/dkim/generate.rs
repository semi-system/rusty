/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! DKIM and DKIM2 key pair generation (feature `generate`).

use crate::crypto::CryptoError;
use crate::{
    Error,
    crypto::{Ed25519Key, MlDsa44Key},
};
use encodify::base64;

/// A freshly generated DKIM or DKIM2 signing key pair.
///
/// The private key is loaded into a signing key such as
/// [`RsaKey`](crate::crypto::RsaKey) or [`Ed25519Key`]; the public key is
/// published, base64 encoded, in the `p=` tag of the
/// `<selector>._domainkey.<domain>` TXT record (RFC 6376, Section 3.6.1).
pub struct DkimKeyPair {
    private_key: Vec<u8>,
    public_key: Vec<u8>,
}

impl DkimKeyPair {
    /// Generates an Ed25519 key pair (RFC 8463). The private key is encoded
    /// as PKCS#8 DER; the public key is the raw 32 byte key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] when key generation fails.
    pub fn generate_ed25519() -> crate::Result<Self> {
        let pkcs8_der = Ed25519Key::generate_pkcs8()
            .map_err(|err| Error::Crypto(CryptoError::Library(err.to_string())))?;
        let key = Ed25519Key::from_pkcs8_der(&pkcs8_der).unwrap();

        Ok(DkimKeyPair {
            private_key: pkcs8_der,
            public_key: key.public_key(),
        })
    }

    /// Generates an ML-DSA-44 key pair for the DKIM2-only `mldsa44`
    /// algorithm, published with `k=mldsa44`. The private key is encoded as
    /// RFC 9881 seed-only PKCS #8 DER; the public key is the raw 1312-byte
    /// ML-DSA-44 key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] when key generation fails.
    pub fn generate_mldsa44() -> crate::Result<Self> {
        let pkcs8_der = MlDsa44Key::generate_pkcs8()?;
        let public_key = MlDsa44Key::from_pkcs8_der(&pkcs8_der)?.public_key()?;

        Ok(DkimKeyPair {
            private_key: pkcs8_der,
            public_key,
        })
    }

    /// Returns the public key: PKCS#1 DER for RSA, raw 32 bytes for Ed25519,
    /// raw 1312 bytes for ML-DSA-44.
    pub fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    /// Returns the private key: PKCS#1 DER for RSA, PKCS#8 DER for Ed25519
    /// and ML-DSA-44/Ed25519.
    pub fn private_key(&self) -> &[u8] {
        &self.private_key
    }

    /// Consumes the pair and returns `(private_key, public_key)`.
    pub fn into_inner(self) -> (Vec<u8>, Vec<u8>) {
        (self.private_key, self.public_key)
    }

    /// Returns the public key base64 encoded, ready for the `p=` tag of the
    /// key record.
    pub fn encoded_public_key(&self) -> String {
        base64::STANDARD.encode(&self.public_key)
    }
}

