/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Signing and verification keys backed by the pure Rust `rsa`, `ed25519-dalek`,
//! `ml-dsa`, `sha1` and `sha2` crates, used when `aws-lc-rs` is not enabled.

use super::{
    Algorithm, CryptoError, ED25519_PUBLIC_KEY_LEN, HashContext, HashImpl, HashOutput,
    MLDSA44_MESSAGE_CAPACITY, Sha1, Sha256, Sha512, SigningKey, VerifyingKey,
    mldsa::{self, CONTEXT, SIGNATURE_LEN, Seed, library_error},
    rsa_private_key_heap_size,
};
use crate::{
    Error, Result,
    dkim::Canonicalization,
    headers::{Writable, Writer},
};
use ed25519_dalek::Signer;
use ml_dsa::{EncodedVerifyingKey, ExpandedSigningKey, MlDsa44};
use ring::rand::SystemRandom;
use ring::signature::{self, RsaKeyPair};
use rustls_pki_types::PrivateKeyDer;
use sha2::Digest;
use std::array::TryFromSliceError;
use std::marker::PhantomData;

// RUSTY PATCH (see ../../PATCHES.md): the RSA operations use `ring` instead of the RustCrypto `rsa`
// crate (RUSTSEC-2023-0071, Marvin timing attack). Signing is ring's constant-time implementation;
// verification takes a PKCS #1 `RSAPublicKey` (an SPKI wrapper is stripped first).

/// Minimal DER reader: the content of the element at the start of `der` and the rest.
fn der_element(der: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&t, rest) = der.split_first()?;
    if t != tag {
        return None;
    }
    let (&l0, rest) = rest.split_first()?;
    let (len, rest) = if l0 < 0x80 {
        (usize::from(l0), rest)
    } else {
        let n = usize::from(l0 & 0x7f);
        if n == 0 || n > 3 || rest.len() < n {
            return None;
        }
        let (lb, rest) = rest.split_at(n);
        (lb.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)), rest)
    };
    if rest.len() < len {
        return None;
    }
    let (content, tail) = rest.split_at(len);
    Some((content, tail))
}

/// `RSAPublicKey` DER from either a bare `RSAPublicKey` or a `SubjectPublicKeyInfo`.
fn rsa_public_key_pkcs1(bytes: &[u8]) -> Option<Vec<u8>> {
    let (seq, _) = der_element(bytes, 0x30)?;
    // SPKI: SEQUENCE { SEQUENCE (algorithm), BIT STRING }.
    if let Some((_alg, after_alg)) = der_element(seq, 0x30) {
        let (bits, _) = der_element(after_alg, 0x03)?;
        let (&unused, inner) = bits.split_first()?;
        return (unused == 0).then(|| inner.to_vec());
    }
    Some(bytes.to_vec())
}

/// Modulus length in bits of a PKCS #1 `RSAPublicKey`.
fn rsa_modulus_bits(pkcs1: &[u8]) -> Option<usize> {
    let (seq, _) = der_element(pkcs1, 0x30)?;
    let (n, _) = der_element(seq, 0x02)?;
    let n = match n.split_first() {
        Some((0, rest)) => rest,
        _ => n,
    };
    let first = *n.first()?;
    Some((n.len() - 1) * 8 + (8 - first.leading_zeros() as usize))
}

/// An RSA private key for `rsa-sha256` signing (RFC 6376 Section 3.3.2).
///
/// `T` selects the hash function; only `RsaKey<Sha256>` implements
/// [`SigningKey`]. Load a key with [`from_key_der`](Self::from_key_der).
#[derive(Debug)]
pub struct RsaKey<T> {
    inner: RsaKeyPair,
    padding: PhantomData<T>,
}

impl<T: HashImpl> RsaKey<T> {
    /// Loads an RSA private key from DER, in PKCS #1 (`RSAPrivateKey`) or
    /// PKCS #8 (`PrivateKeyInfo`) form. Parse PEM input first with
    /// [`PrivateKeyDer::from_pem_slice`](rustls_pki_types::pem::PemObject::from_pem_slice).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] with [`CryptoError::Library`] if the key is
    /// in another format (such as SEC1) or cannot be decoded.
    pub fn from_key_der(key_der: PrivateKeyDer<'_>) -> Result<Self> {
        let inner = match key_der {
            PrivateKeyDer::Pkcs1(der) => RsaKeyPair::from_der(der.secret_pkcs1_der())
                .map_err(|err| Error::Crypto(CryptoError::Library(err.to_string())))?,
            PrivateKeyDer::Pkcs8(der) => RsaKeyPair::from_pkcs8(der.secret_pkcs8_der())
                .map_err(|err| Error::Crypto(CryptoError::Library(err.to_string())))?,
            _ => {
                return Err(Error::Crypto(CryptoError::Library(
                    "Unsupported RSA key format".to_string(),
                )));
            }
        };

        Ok(RsaKey {
            inner,
            padding: PhantomData,
        })
    }

    /// Returns the public key of the RSA key pair, encoded as a DER PKCS #1
    /// `RSAPublicKey`. Returns an empty vector if encoding fails.
    pub fn public_key(&self) -> Vec<u8> {
        signature::KeyPair::public_key(&self.inner).as_ref().to_vec()
    }
}

impl SigningKey for RsaKey<Sha256> {
    type Hasher = Sha256;

    fn sign(&self, input: impl Writable) -> Result<Vec<u8>> {
        // ring hashes the message itself (RSA_PKCS1_SHA256), so the signed bytes are collected.
        let mut data = Vec::with_capacity(512);
        input.write(&mut data);
        let mut sig = vec![0u8; self.inner.public().modulus_len()];
        self.inner
            .sign(&signature::RSA_PKCS1_SHA256, &SystemRandom::new(), &data, &mut sig)
            .map_err(|err| Error::Crypto(CryptoError::Library(err.to_string())))?;
        Ok(sig)
    }

    fn algorithm(&self) -> Algorithm {
        Algorithm::RsaSha256
    }

    fn heap_size(&self) -> usize {
        rsa_private_key_heap_size(self.inner.public().modulus_len())
    }
}

/// An Ed25519 private key for `ed25519-sha256` signing (RFC 8463).
///
/// The signature is computed over the SHA-256 hash of the signed data, as
/// RFC 8463 Section 3 requires.
pub struct Ed25519Key {
    inner: ed25519_dalek::SigningKey,
}

impl Ed25519Key {
    /// Generates a new random Ed25519 key pair and returns it as PKCS #8 DER
    /// bytes, ready for [`from_pkcs8_der`](Self::from_pkcs8_der).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] with [`CryptoError::Library`] if key
    /// generation or encoding fails.
    pub fn generate_pkcs8() -> Result<Vec<u8>> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use rand::Rng;

        let mut seed = [0u8; ed25519_dalek::SECRET_KEY_LENGTH];
        rand::rng().fill_bytes(&mut seed);

        Ok(ed25519_dalek::SigningKey::from_bytes(&seed)
            .to_pkcs8_der()
            .map_err(|err| Error::Crypto(CryptoError::Library(err.to_string())))?
            .as_bytes()
            .to_vec())
    }

    /// Loads an Ed25519 key pair from PKCS #8 DER bytes (v1 or v2), checking
    /// that the embedded public key, when present, matches the private key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] with [`CryptoError::Library`] if the key
    /// cannot be decoded.
    pub fn from_pkcs8_der(pkcs8_der: &[u8]) -> Result<Self> {
        use ed25519_dalek::pkcs8::DecodePrivateKey;

        Ok(Self {
            inner: ed25519_dalek::SigningKey::from_pkcs8_der(pkcs8_der)
                .map_err(|err| Error::Crypto(CryptoError::Library(err.to_string())))?,
        })
    }

    /// Loads an Ed25519 key pair from PKCS #8 v1 or v2 DER bytes. On this
    /// backend it is identical to [`from_pkcs8_der`](Self::from_pkcs8_der).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] with [`CryptoError::Library`] if the key
    /// cannot be decoded.
    pub fn from_pkcs8_maybe_unchecked_der(pkcs8_der: &[u8]) -> Result<Self> {
        Self::from_pkcs8_der(pkcs8_der)
    }

    /// Builds an Ed25519 key pair from its 32-byte seed (private key) and
    /// 32-byte public key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] with [`CryptoError::Library`] if either
    /// value has the wrong length or the public key does not match the seed.
    pub fn from_seed_and_public_key(seed: &[u8], public_key: &[u8]) -> Result<Self> {
        let inner = ed25519_dalek::SigningKey::from_bytes(seed.try_into().map_err(
            |err: TryFromSliceError| Error::Crypto(CryptoError::Library(err.to_string())),
        )?);

        if inner.verifying_key().as_bytes().as_slice() != public_key {
            return Err(Error::Crypto(CryptoError::Library(
                "Ed25519 public key does not match the provided seed".to_string(),
            )));
        }

        Ok(Self { inner })
    }

    /// Returns the 32-byte raw Ed25519 public key, as published in the `p=`
    /// tag of a DKIM key record.
    pub fn public_key(&self) -> Vec<u8> {
        self.inner.verifying_key().to_bytes().to_vec()
    }
}

impl SigningKey for Ed25519Key {
    type Hasher = Sha256;

    fn sign(&self, input: impl Writable) -> Result<Vec<u8>> {
        let hash = self.hash(input);
        Ok(self.inner.sign(hash.as_ref()).to_bytes().to_vec())
    }

    fn algorithm(&self) -> Algorithm {
        Algorithm::Ed25519Sha256
    }
}

pub(crate) struct RsaPublicKey {
    /// PKCS #1 `RSAPublicKey` DER.
    pkcs1: Vec<u8>,
    bits: usize,
}

/// Smallest and largest RSA modulus accepted.
const RSA_MIN_MODULUS_BITS: usize = 1024;
const RSA_MAX_MODULUS_BITS: usize = 8192;

impl RsaPublicKey {
    pub(crate) fn verifying_key_from_bytes(
        bytes: &[u8],
    ) -> Result<Box<dyn VerifyingKey + Send + Sync>> {
        let malformed = || Error::Crypto(CryptoError::MalformedPublicKey);
        let pkcs1 = rsa_public_key_pkcs1(bytes).ok_or_else(malformed)?;
        let bits = rsa_modulus_bits(&pkcs1).ok_or_else(malformed)?;
        if !(RSA_MIN_MODULUS_BITS..=RSA_MAX_MODULUS_BITS).contains(&bits) {
            return Err(malformed());
        }
        Ok(Box::new(RsaPublicKey { pkcs1, bits }))
    }

    fn verify_digest(&self, data: &[u8], sig: &[u8], algorithm: Algorithm) -> Result<()> {
        let alg: &'static dyn signature::VerificationAlgorithm = match algorithm {
            Algorithm::RsaSha256 => &signature::RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
            Algorithm::RsaSha1 => &signature::RSA_PKCS1_1024_8192_SHA1_FOR_LEGACY_USE_ONLY,
            Algorithm::Ed25519Sha256 | Algorithm::MlDsa44 => {
                return Err(Error::Crypto(CryptoError::IncompatibleAlgorithms));
            }
        };
        match algorithm {
            Algorithm::RsaSha256 | Algorithm::RsaSha1 => {
                signature::UnparsedPublicKey::new(alg, &self.pkcs1)
                    .verify(data, sig)
                    .map_err(|_| Error::Crypto(CryptoError::FailedVerification))
            }
            Algorithm::Ed25519Sha256 | Algorithm::MlDsa44 => {
                Err(Error::Crypto(CryptoError::IncompatibleAlgorithms))
            }
        }
    }
}

impl VerifyingKey for RsaPublicKey {
    fn verify<'a>(
        &self,
        headers: &mut dyn Iterator<Item = (&'a [u8], &'a [u8])>,
        signature: &[u8],
        canonicalization: Canonicalization,
        algorithm: Algorithm,
    ) -> Result<()> {
        let mut data = Vec::with_capacity(256);
        canonicalization.canonicalize_headers(headers, &mut data);
        self.verify_digest(&data, signature, algorithm)
    }

    fn verify_bytes(&self, input: &[u8], signature: &[u8], algorithm: Algorithm) -> Result<()> {
        self.verify_digest(input, signature, algorithm)
    }

    fn public_key_bits(&self) -> usize {
        self.bits
    }

    fn heap_size(&self) -> usize {
        self.pkcs1.len()
    }
}

pub(crate) struct Ed25519PublicKey {
    inner: ed25519_dalek::VerifyingKey,
}

impl Ed25519PublicKey {
    pub(crate) fn verifying_key_from_bytes(
        bytes: &[u8],
    ) -> Result<Box<dyn VerifyingKey + Send + Sync>> {
        let key: &[u8; ED25519_PUBLIC_KEY_LEN] = bytes
            .try_into()
            .map_err(|_| Error::Crypto(CryptoError::MalformedPublicKey))?;
        Ok(Box::new(Ed25519PublicKey {
            inner: ed25519_dalek::VerifyingKey::from_bytes(key)
                .map_err(|_| Error::Crypto(CryptoError::MalformedPublicKey))?,
        }))
    }

    fn verify_digest(&self, data: &[u8], signature: &[u8]) -> Result<()> {
        let signature = ed25519_dalek::Signature::from_bytes(signature.try_into().map_err(
            |err: TryFromSliceError| Error::Crypto(CryptoError::Library(err.to_string())),
        )?);

        self.inner
            .verify_strict(sha2::Sha256::digest(data).as_slice(), &signature)
            .map_err(|_| Error::Crypto(CryptoError::FailedVerification))
    }
}

impl VerifyingKey for Ed25519PublicKey {
    fn verify<'a>(
        &self,
        headers: &mut dyn Iterator<Item = (&'a [u8], &'a [u8])>,
        signature: &[u8],
        canonicalization: Canonicalization,
        algorithm: Algorithm,
    ) -> Result<()> {
        if !matches!(algorithm, Algorithm::Ed25519Sha256) {
            return Err(Error::Crypto(CryptoError::IncompatibleAlgorithms));
        }

        let mut data = Vec::with_capacity(256);
        canonicalization.canonicalize_headers(headers, &mut data);
        self.verify_digest(&data, signature)
    }

    fn verify_bytes(&self, input: &[u8], signature: &[u8], algorithm: Algorithm) -> Result<()> {
        if !matches!(algorithm, Algorithm::Ed25519Sha256) {
            return Err(Error::Crypto(CryptoError::IncompatibleAlgorithms));
        }

        self.verify_digest(input, signature)
    }
}

/// An ML-DSA-44 private key for `mldsa44` signing (DKIM2 only,
/// draft-ietf-dkim-dkim2-spec Section 3.4).
///
/// Signatures use "pure" ML-DSA-44 (FIPS 204) with the `DKIM2` context
/// string. They are hedged (randomized), as the specification recommends, so
/// signing the same input twice gives different signatures.
pub struct MlDsa44Key {
    inner: ExpandedSigningKey<MlDsa44>,
}

impl MlDsa44Key {
    /// Generates a new random key and returns it as PKCS #8 DER bytes (the
    /// RFC 9881 seed-only private key encoding), ready for
    /// [`from_pkcs8_der`](Self::from_pkcs8_der).
    ///
    /// # Errors
    ///
    /// Never fails on this backend; the signature matches the other backends.
    pub fn generate_pkcs8() -> Result<Vec<u8>> {
        use rand::Rng;

        let mut seed = Seed::default();
        rand::rng().fill_bytes(&mut seed);
        Ok(mldsa::pkcs8_der(&seed))
    }

    /// Loads a key from PKCS #8 DER bytes holding an ML-DSA-44 private key in
    /// the RFC 9881 seed-only or both (seed and expanded key) form. The key
    /// is derived from the seed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] with [`CryptoError::Library`] if the key
    /// cannot be decoded or has no seed.
    pub fn from_pkcs8_der(pkcs8_der: &[u8]) -> Result<Self> {
        mldsa::seed_from_pkcs8_der(pkcs8_der).map(Self::from_seed)
    }

    /// Loads a key from its 32-byte seed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Crypto`] with [`CryptoError::Library`] if the seed
    /// has the wrong length.
    pub fn from_raw_private_key(raw: &[u8]) -> Result<Self> {
        mldsa::seed_from_raw(raw).map(Self::from_seed)
    }

    fn from_seed(seed: &Seed) -> Self {
        Self {
            inner: ExpandedSigningKey::from_seed(&(*seed).into()),
        }
    }

    /// Returns the 1312-byte ML-DSA-44 public key, as published in the `p=`
    /// tag of a `k=mldsa44` key record.
    ///
    /// # Errors
    ///
    /// Never fails on this backend; the signature matches the other backends.
    pub fn public_key(&self) -> Result<Vec<u8>> {
        Ok(self.inner.verifying_key().encode().to_vec())
    }
}

impl SigningKey for MlDsa44Key {
    type Hasher = Sha256;

    fn sign(&self, input: impl Writable) -> Result<Vec<u8>> {
        let mut message = Vec::with_capacity(MLDSA44_MESSAGE_CAPACITY);
        input.write(&mut message);
        self.inner
            .sign_randomized(&message, CONTEXT, &mut rand::rng())
            .map(|signature| signature.encode().to_vec())
            .map_err(|_| library_error("ML-DSA-44 signing failed"))
    }

    fn algorithm(&self) -> Algorithm {
        Algorithm::MlDsa44
    }

    fn heap_size(&self) -> usize {
        MLDSA44_PRIVATE_KEY_HEAP_SIZE
    }
}

pub(crate) struct MlDsa44PublicKey {
    inner: ml_dsa::VerifyingKey<MlDsa44>,
}

const MLDSA44_PRIVATE_KEY_HEAP_SIZE: usize = 20480;
const MLDSA44_PUBLIC_KEY_HEAP_SIZE: usize = 18432;

impl MlDsa44PublicKey {
    pub(crate) fn verifying_key_from_bytes(
        bytes: &[u8],
    ) -> Result<Box<dyn VerifyingKey + Send + Sync>> {
        let key = mldsa::public_key_from_bytes(bytes)?;
        Ok(Box::new(Self {
            inner: ml_dsa::VerifyingKey::decode(&EncodedVerifyingKey::<MlDsa44>::from(*key)),
        }))
    }
}

impl VerifyingKey for MlDsa44PublicKey {
    fn verify<'a>(
        &self,
        _: &mut dyn Iterator<Item = (&'a [u8], &'a [u8])>,
        _: &[u8],
        _: Canonicalization,
        _: Algorithm,
    ) -> Result<()> {
        Err(Error::Crypto(CryptoError::IncompatibleAlgorithms))
    }

    fn verify_bytes(&self, input: &[u8], signature: &[u8], algorithm: Algorithm) -> Result<()> {
        if !matches!(algorithm, Algorithm::MlDsa44) {
            return Err(Error::Crypto(CryptoError::IncompatibleAlgorithms));
        }
        let signature = <&[u8; SIGNATURE_LEN]>::try_from(signature)
            .ok()
            .and_then(|signature| ml_dsa::Signature::<MlDsa44>::try_from(signature.as_slice()).ok())
            .ok_or(Error::Crypto(CryptoError::FailedVerification))?;
        if self.inner.verify_with_context(input, CONTEXT, &signature) {
            Ok(())
        } else {
            Err(Error::Crypto(CryptoError::FailedVerification))
        }
    }

    fn heap_size(&self) -> usize {
        MLDSA44_PUBLIC_KEY_HEAP_SIZE
    }
}

impl Writer for sha1::Sha1 {
    fn write(&mut self, buf: &[u8]) {
        self.update(buf);
    }
}

impl Writer for sha2::Sha256 {
    fn write(&mut self, buf: &[u8]) {
        self.update(buf);
    }
}

impl Writer for sha2::Sha512 {
    fn write(&mut self, buf: &[u8]) {
        self.update(buf);
    }
}

impl HashImpl for Sha1 {
    type Context = sha1::Sha1;

    fn hasher() -> Self::Context {
        <Self::Context as Digest>::new()
    }
}

impl HashImpl for Sha256 {
    type Context = sha2::Sha256;

    fn hasher() -> Self::Context {
        <Self::Context as Digest>::new()
    }
}

impl HashImpl for Sha512 {
    type Context = sha2::Sha512;

    fn hasher() -> Self::Context {
        <Self::Context as Digest>::new()
    }
}

impl HashContext for sha2::Sha512 {
    fn complete(self) -> HashOutput {
        HashOutput::RustCryptoSha512(self.finalize())
    }
}

impl HashContext for sha1::Sha1 {
    fn complete(self) -> HashOutput {
        HashOutput::RustCryptoSha1(self.finalize())
    }
}

impl HashContext for sha2::Sha256 {
    fn complete(self) -> HashOutput {
        HashOutput::RustCryptoSha256(self.finalize())
    }
}
