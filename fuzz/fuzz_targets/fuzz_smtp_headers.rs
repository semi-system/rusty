#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rusty_smtp::fuzzing::check_headers(data);
});
