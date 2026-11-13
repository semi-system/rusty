//! Stable-toolchain stand-in for fuzz/fuzz_targets/fuzz_ctl_args.rs: same entry point and
//! invariants, driven by proptest and the committed seeds. Run by scripts/ci.sh when
//! cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use rusty_ctl::{parse_args, Command, PasswordSource};

const SEEDS: [&str; 7] = [
    include_str!("../../../fuzz/seeds/fuzz_ctl_args/domain_add"),
    include_str!("../../../fuzz/seeds/fuzz_ctl_args/account_add"),
    include_str!("../../../fuzz/seeds/fuzz_ctl_args/dev_json"),
    include_str!("../../../fuzz/seeds/fuzz_ctl_args/dkim"),
    include_str!("../../../fuzz/seeds/fuzz_ctl_args/dkim_import"),
    include_str!("../../../fuzz/seeds/fuzz_ctl_args/password_argv"),
    include_str!("../../../fuzz/seeds/fuzz_ctl_args/alias_add"),
];

fn check(text: &str) {
    let args: Vec<String> = text.split('\0').map(str::to_owned).collect();
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
}

// backend-neutral: argument parsing only, no database
#[test]
fn seeds_pass() {
    for s in SEEDS {
        check(s);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    // backend-neutral: argument parsing only, no database
    #[test]
    fn arbitrary_text_never_panics(s in "\\PC{0,200}") {
        check(&s);
    }

    // backend-neutral: argument parsing only, no database
    #[test]
    fn mutated_seed_never_panics(which in 0usize..6, pos in 0usize..200, ch in any::<char>(), cut in 0usize..200) {
        let seed = SEEDS.get(which).copied().unwrap_or("");
        let mut chars: Vec<char> = seed.chars().collect();
        if !chars.is_empty() {
            let at = pos % chars.len();
            if let Some(slot) = chars.get_mut(at) {
                *slot = ch;
            }
            let keep = chars.len().saturating_sub(cut % chars.len()).max(1);
            chars.truncate(keep);
        }
        check(&chars.into_iter().collect::<String>());
    }
}
