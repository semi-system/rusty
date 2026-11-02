#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(env) = rusty_mime::parse_envelope(data) {
        for field in [&env.from, &env.to] {
            assert_eq!(field.to_lowercase(), *field, "address field not lowercase");
            assert!(!field.contains([char::from(60), char::from(62)]), "angle bracket in address");
        }
    }
});
