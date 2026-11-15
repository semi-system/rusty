#![no_main]
use libfuzzer_sys::fuzz_target;

// PROXY protocol v2 headers from the network: never a panic, only bounded and well-formed headers accepted, an accepted address round-trips.
fuzz_target!(|data: &[u8]| {
    rusty_svc::proxy::check_invariants(data);
});
