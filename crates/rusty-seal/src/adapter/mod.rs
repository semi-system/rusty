//! The only module that names the HPKE library. Everything it takes and returns is plain bytes; nothing here is `pub`.
//!
//! Suite (RFC 9180): DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305, base mode (no sender authentication:
//! anyone who knows the public key can seal, which is the point; whoever may write is decided by database privileges).

use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem as _, OpModeR, OpModeS, Serializable};
use zeroize::Zeroizing;

type Kem = X25519HkdfSha256;

/// Length of an X25519 key (public or secret) and of the encapsulated key.
pub(crate) const KEY_LEN: usize = 32;

/// The same fixed failure for everything that goes wrong on the way in: nothing tells a caller what was wrong.
pub(crate) struct Refused;

/// A new key pair: `(secret, public)`. The secret is the serialisation of the private key (clamped, as the library writes it).
pub(crate) fn generate() -> (Zeroizing<[u8; KEY_LEN]>, [u8; KEY_LEN]) {
    let (sk, pk) = Kem::gen_keypair();
    (Zeroizing::new(to_array(&sk.to_bytes())), to_array(&pk.to_bytes()))
}

fn to_array(bytes: &[u8]) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    if let Some(dst) = out.get_mut(..bytes.len().min(KEY_LEN)) {
        dst.copy_from_slice(bytes.get(..dst.len()).unwrap_or_default());
    }
    out
}

/// The public key of a secret key.
pub(crate) fn public_of(secret: &[u8; KEY_LEN]) -> Result<[u8; KEY_LEN], Refused> {
    let sk = <Kem as hpke::Kem>::PrivateKey::from_bytes(secret).map_err(|_| Refused)?;
    Ok(to_array(&<Kem as hpke::Kem>::sk_to_pk(&sk).to_bytes()))
}

/// Does `public` parse as a key we would seal to? (A point of small order would make the shared secret known: refused.)
pub(crate) fn check_public(public: &[u8; KEY_LEN]) -> Result<(), Refused> {
    let pk = <Kem as hpke::Kem>::PublicKey::from_bytes(public).map_err(|_| Refused)?;
    // Sealing to it is the test: the library refuses a degenerate shared secret when it encapsulates.
    hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, Kem>(&OpModeS::Base, &pk, b"rusty-seal-probe", b"", b"").map(|_| ()).map_err(|_| Refused)
}

/// Seal `plaintext` to `public`: `(encapsulated key, ciphertext with its tag)`.
pub(crate) fn seal(public: &[u8; KEY_LEN], info: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<([u8; KEY_LEN], Vec<u8>), Refused> {
    let pk = <Kem as hpke::Kem>::PublicKey::from_bytes(public).map_err(|_| Refused)?;
    let (enc, ct) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, Kem>(&OpModeS::Base, &pk, info, plaintext, aad).map_err(|_| Refused)?;
    Ok((to_array(&enc.to_bytes()), ct))
}

/// Open what [`seal`] made. Any failure is [`Refused`].
pub(crate) fn open(secret: &[u8; KEY_LEN], enc: &[u8; KEY_LEN], info: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, Refused> {
    let sk = <Kem as hpke::Kem>::PrivateKey::from_bytes(secret).map_err(|_| Refused)?;
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(enc).map_err(|_| Refused)?;
    hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, Kem>(&OpModeR::Base, &sk, &enc, info, ciphertext, aad)
        .map(Zeroizing::new)
        .map_err(|_| Refused)
}
