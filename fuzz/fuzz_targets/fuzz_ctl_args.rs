#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_ctl::{parse_args, Command, PasswordSource};

fuzz_target!(|data: &[u8]| {
    // Arguments are NUL-separated; invalid UTF-8 cannot occur in a `String` argument vector.
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let args: Vec<String> = text.split('\0').map(str::to_owned).collect();
    // Must never panic. An accepted command line keeps the documented guarantees.
    let Ok(p) = parse_args(&args) else {
        return;
    };
    match p.command {
        Command::AccountSetPassword { password, .. } => assert_ne!(password, PasswordSource::None),
        Command::DkimGenerate { selector, .. } | Command::DkimImport { selector, .. } => {
            assert!((1..=63).contains(&selector.len()));
            assert!(selector.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'));
        }
        _ => {}
    }
});
