//! Sealed secrets (design section 14.1): a secret encrypted to the outbound service's public key, bound to its owner and
//! purpose, that the admin can write and never read back. Everything the design promises is a test here: the
//! round trip, the binding to the context, the refusal of tampered, truncated, misdirected and garbage input (with one
//! fixed error and no plaintext in it), key rotation by key id, and keys that never print.

use proptest::prelude::*;
use rusty_seal::{generate, Context, SealingKey, Unsealer, UnsealingKey, MAX_PLAINTEXT, OVERHEAD};

fn ctx() -> Context<'static> {
    Context::new("domain", 42, "dkim-private-key").unwrap()
}

fn pair() -> (UnsealingKey, SealingKey) {
    generate()
}

// ------------------------------------------------------------------------------------------ the round trip

#[test]
fn a_sealed_secret_opens_to_what_was_sealed_for_every_size() {
    let (sk, pk) = pair();
    let u = Unsealer::new(vec![sk]).unwrap();
    for len in [0usize, 1, 15, 16, 31, 32, 33, 1000, 4096, MAX_PLAINTEXT] {
        let secret: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        let sealed = pk.seal(&ctx(), &secret).unwrap();
        assert_eq!(sealed.len(), len + OVERHEAD, "the overhead is fixed");
        let opened = u.unseal(&ctx(), &sealed).unwrap();
        assert_eq!(opened.as_slice(), secret.as_slice(), "{len}");
    }
}

#[test]
fn sealing_twice_gives_different_bytes_that_both_open() {
    let (sk, pk) = pair();
    let a = pk.seal(&ctx(), b"same secret").unwrap();
    let b = pk.seal(&ctx(), b"same secret").unwrap();
    assert_ne!(a, b, "a fresh ephemeral key every time");
    let u = Unsealer::new(vec![sk]).unwrap();
    assert_eq!(u.unseal(&ctx(), &a).unwrap().as_slice(), b"same secret");
    assert_eq!(u.unseal(&ctx(), &b).unwrap().as_slice(), b"same secret");
}

#[test]
fn a_plaintext_over_the_limit_is_refused_when_sealing() {
    let (_sk, pk) = pair();
    assert!(pk.seal(&ctx(), &vec![0u8; MAX_PLAINTEXT + 1]).is_err());
}

// ---------------------------------------------------------------------------------- binding to the context

#[test]
fn a_secret_cannot_be_opened_for_another_owner_kind_or_purpose() {
    let (sk, pk) = pair();
    let sealed = pk.seal(&ctx(), b"relay password").unwrap();
    let u = Unsealer::new(vec![sk]).unwrap();
    for other in [
        Context::new("domain", 43, "dkim-private-key").unwrap(),
        Context::new("domain", 42, "relay-password").unwrap(),
        Context::new("tenant", 42, "dkim-private-key").unwrap(),
        Context::new("domain", -42, "dkim-private-key").unwrap(),
    ] {
        assert!(u.unseal(&other, &sealed).is_err(), "{other:?}");
    }
    assert!(u.unseal(&ctx(), &sealed).is_ok(), "and the right one still works");
}

#[test]
fn a_context_is_validated_and_cannot_be_confused_by_its_spelling() {
    for bad in [("", "p"), ("k", ""), ("a b", "p"), ("k", "p\n"), ("K", "p"), ("k", "p/q")] {
        assert!(Context::new(bad.0, 1, bad.1).is_err(), "{bad:?}");
    }
    assert!(Context::new(&"a".repeat(33), 1, "p").is_err());
    // ("ab", 1, "c") and ("a", ..., "bc") must not collide: the fields are length-prefixed.
    let (sk, pk) = pair();
    let u = Unsealer::new(vec![sk]).unwrap();
    let sealed = pk.seal(&Context::new("ab", 1, "c").unwrap(), b"x").unwrap();
    assert!(u.unseal(&Context::new("a", 1, "bc").unwrap(), &sealed).is_err());
}

// ------------------------------------------------------------------------------------------ refusals

#[test]
fn another_key_cannot_open_it_and_the_error_says_nothing() {
    let (_sk, pk) = pair();
    let (other, _) = pair();
    let sealed = pk.seal(&ctx(), b"top secret value").unwrap();
    let err = Unsealer::new(vec![other]).unwrap().unseal(&ctx(), &sealed).unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("top secret"), "{text}");
    assert!(text.contains("cannot be opened"), "{text}");
}

#[test]
fn every_single_byte_changed_is_refused_with_the_same_error() {
    let (sk, pk) = pair();
    let sealed = pk.seal(&ctx(), b"a secret of some length 0123456789").unwrap();
    let u = Unsealer::new(vec![sk]).unwrap();
    let mut messages = std::collections::BTreeSet::new();
    for i in 0..sealed.len() {
        let mut t = sealed.clone();
        t[i] ^= 0x01;
        let e = u.unseal(&ctx(), &t).unwrap_err();
        messages.insert(e.to_string());
    }
    assert_eq!(messages.len(), 1, "one fixed error whatever was damaged (no oracle): {messages:?}");
}

#[test]
fn truncated_extended_and_garbage_input_is_refused() {
    let (sk, pk) = pair();
    let sealed = pk.seal(&ctx(), b"abc").unwrap();
    let u = Unsealer::new(vec![sk]).unwrap();
    for n in 0..sealed.len() {
        assert!(u.unseal(&ctx(), &sealed[..n]).is_err(), "truncated to {n}");
    }
    let mut longer = sealed.clone();
    longer.push(0);
    assert!(u.unseal(&ctx(), &longer).is_err());
    assert!(u.unseal(&ctx(), &[]).is_err());
    assert!(u.unseal(&ctx(), &[0u8; 200]).is_err());
}

#[test]
fn an_unknown_format_version_is_refused() {
    let (sk, pk) = pair();
    let mut sealed = pk.seal(&ctx(), b"abc").unwrap();
    assert_eq!(sealed[0], 1, "version 1");
    sealed[0] = 2;
    assert!(Unsealer::new(vec![sk]).unwrap().unseal(&ctx(), &sealed).is_err());
}

#[test]
fn sealing_to_a_weak_public_key_is_refused() {
    // The all-zero point (and other low-order points) would give a known shared secret.
    assert!(SealingKey::from_bytes([0u8; 32]).is_err());
    let mut one = [0u8; 32];
    one[0] = 1;
    assert!(SealingKey::from_bytes(one).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn arbitrary_input_never_panics_and_is_never_opened(data in proptest::collection::vec(any::<u8>(), 0..400)) {
        let (sk, _pk) = pair();
        let u = Unsealer::new(vec![sk]).unwrap();
        prop_assert!(u.unseal(&ctx(), &data).is_err());
    }

    #[test]
    fn any_secret_round_trips(secret in proptest::collection::vec(any::<u8>(), 0..2000), id in any::<i64>()) {
        let (sk, pk) = pair();
        let c = Context::new("domain", id, "relay-password").unwrap();
        let sealed = pk.seal(&c, &secret).unwrap();
        let opened = Unsealer::new(vec![sk]).unwrap().unseal(&c, &sealed).unwrap();
        prop_assert_eq!(opened.as_slice(), secret.as_slice());
    }
}

// -------------------------------------------------------------------------------- key ids and rotation

#[test]
fn the_unsealer_picks_the_key_by_id_so_an_old_and_a_new_key_work_together() {
    let (old_sk, old_pk) = pair();
    let (new_sk, new_pk) = pair();
    assert_ne!(old_pk.id(), new_pk.id());
    let old = old_pk.seal(&ctx(), b"sealed last year").unwrap();
    let new = new_pk.seal(&ctx(), b"sealed today").unwrap();
    let u = Unsealer::new(vec![new_sk, old_sk]).unwrap();
    assert_eq!(u.unseal(&ctx(), &old).unwrap().as_slice(), b"sealed last year");
    assert_eq!(u.unseal(&ctx(), &new).unwrap().as_slice(), b"sealed today");
    assert_eq!(u.current_public().id(), new_pk.id(), "the first key seals new secrets");
    // A key that was retired is refused, not guessed.
    let (only_new, _) = (Unsealer::new(vec![pair().0]).unwrap(), ());
    assert!(only_new.unseal(&ctx(), &old).is_err());
}

#[test]
fn an_unsealer_needs_keys_and_refuses_duplicates_and_too_many() {
    assert!(Unsealer::new(vec![]).is_err());
    let (a, _) = pair();
    let dup = UnsealingKey::from_text(a.to_text().as_str()).unwrap();
    assert!(Unsealer::new(vec![a, dup]).is_err());
    let many: Vec<UnsealingKey> = (0..5).map(|_| pair().0).collect();
    assert!(Unsealer::new(many).is_err(), "at most 4 keys");
}

// ----------------------------------------------------------------------------------------- key material

#[test]
fn keys_round_trip_through_their_text_form_and_never_print_the_secret() {
    let (sk, pk) = pair();
    let secret_text = sk.to_text();
    assert!(secret_text.as_str().starts_with("rusty-unseal-v1:") && secret_text.len() == "rusty-unseal-v1:".len() + 64);
    let back = UnsealingKey::from_text(secret_text.as_str()).unwrap();
    assert_eq!(back.public().to_bytes(), pk.to_bytes(), "the same key pair");
    let pk_text = pk.to_text();
    assert!(pk_text.starts_with("rusty-seal-v1:"));
    assert_eq!(SealingKey::from_text(&pk_text).unwrap().to_bytes(), pk.to_bytes());
    // The secret never appears in Debug, and Debug names the key by id only.
    let hex_secret = &secret_text.as_str()["rusty-unseal-v1:".len()..];
    let shown = format!("{sk:?} {back:?}");
    assert!(!shown.contains(hex_secret) && shown.contains("redacted"), "{shown}");
    // Bad text of every shape is refused.
    for bad in ["", "rusty-unseal-v1:", "rusty-unseal-v1:zz", "rusty-seal-v1:00", &format!("rusty-unseal-v1:{}", "0".repeat(63)), &format!("other:{}", "0".repeat(64))] {
        assert!(UnsealingKey::from_text(bad).is_err(), "{bad:.30}");
    }
    assert!(SealingKey::from_text(secret_text.as_str()).is_err(), "a secret key text is not a public key");
}

#[test]
fn the_key_id_is_stable_and_short() {
    let (_sk, pk) = pair();
    assert_eq!(pk.id(), pk.id());
    assert_eq!(pk.id().to_hex().len(), 16);
    assert_eq!(SealingKey::from_bytes(pk.to_bytes()).unwrap().id(), pk.id());
}

#[test]
fn a_sealed_blob_made_by_an_earlier_build_still_opens() {
    // Golden: the format is part of the stored data. If this fails after an upgrade of the HPKE library or of this crate,
    // secrets already in the database could no longer be opened.
    let sk = UnsealingKey::from_text(GOLDEN_KEY).unwrap();
    let sealed = hex::decode(GOLDEN_SEALED).unwrap();
    let opened = Unsealer::new(vec![sk]).unwrap().unseal(&Context::new("domain", 7, "relay-password").unwrap(), &sealed).unwrap();
    assert_eq!(opened.as_slice(), b"correct horse battery staple");
}

const GOLDEN_KEY: &str = "rusty-unseal-v1:d01d9f9675d30b7ce72a6647d37e30ff8465935b6abaf09c9fe7d7f6d92d5656";
const GOLDEN_SEALED: &str = "010dd97b8dd4e6970787efe5b364a144b7578b1863d79b0520cada97af57c395ed0e49229a74f34364645f88be6363f1515a6026efa0dc0bb3194b5d5e79aa4ff6fa65a8d5c190820672b859408fa8cb82ab04423f";

#[test]
fn a_key_file_is_read_from_its_bytes_and_bad_bytes_are_refused() {
    let (sk, pk) = pair();
    let mut file = sk.to_text().as_str().as_bytes().to_vec();
    file.push(b'\n');
    let back = UnsealingKey::from_file_bytes(file).unwrap();
    assert_eq!(back.public().to_bytes(), pk.to_bytes());
    assert!(UnsealingKey::from_file_bytes(vec![0xff, 0xfe, 0x00]).is_err(), "not UTF-8");
    assert!(UnsealingKey::from_file_bytes(Vec::new()).is_err());
    assert!(UnsealingKey::from_file_bytes(b"rusty-unseal-v1:nothex".to_vec()).is_err());
}
