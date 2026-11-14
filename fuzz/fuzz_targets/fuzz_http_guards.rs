#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rusty_http::fuzzing::check_guards(data);
});
