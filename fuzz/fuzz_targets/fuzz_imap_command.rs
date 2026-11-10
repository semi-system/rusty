#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| rusty_imap::fuzz_checks::command(data));
