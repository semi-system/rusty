#![no_main]
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use rusty_seal::{generate, Context, SealingKey, Unsealer, UnsealingKey};

// What the outbound service opens comes out of the database: it must never panic on any bytes, and only what was sealed to this key and this context opens.
// Also the key text formats (public key in the configuration, key file bytes).
struct Fixture {
    unsealer: Unsealer,
    sealing: SealingKey,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let (private, public) = generate();
        Fixture { unsealer: Unsealer::new(vec![private]).unwrap(), sealing: public }
    })
}

fuzz_target!(|data: &[u8]| {
    let f = fixture();
    let ctx = Context::new("domain", 7, "dkim-0123456789abcdef01234567").unwrap();
    // Arbitrary bytes as a sealed blob: an error or (astronomically unlikely) a value, never a panic.
    let _ = f.unsealer.unseal(&ctx, data);
    // A genuine seal of the input opens to the input in its own context and to nothing in another.
    if data.len() <= 4096 {
        let sealed = f.sealing.seal(&ctx, data).unwrap();
        assert_eq!(f.unsealer.unseal(&ctx, &sealed).unwrap().as_slice(), data);
        let other = Context::new("domain", 8, "dkim-0123456789abcdef01234567").unwrap();
        assert!(f.unsealer.unseal(&other, &sealed).is_err());
    }
    // The text formats.
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = SealingKey::from_text(text);
        let _ = UnsealingKey::from_text(text);
    }
    let _ = UnsealingKey::from_file_bytes(data.to_vec());
});
