//! Entry point shared by the fuzz target (`fuzz/fuzz_targets/fuzz_managesieve_command.rs`) and its stable-toolchain stand-in (`tests/fuzz_smoke.rs`): the same invariants whoever drives them.

use crate::command::{parse, Arg, Limits, Parsed};

/// The parser is total and bounded: it never panics, never takes more than it was given, never hands out more than its limits, and a command it accepted is the same command whatever follows it.
pub fn command(data: &[u8]) {
    let small = Limits { max_literal: 64, max_line: 96, max_args: 3 };
    for limits in [Limits::default(), small] {
        for bare in [false, true] {
            match parse(data, &limits, bare) {
                Parsed::Need | Parsed::Bad(_) => {}
                Parsed::Done(c, used) => {
                    assert!(used <= data.len() && used >= 2, "took {used} of {}", data.len());
                    assert!(data.get(..used).is_some_and(|d| d.ends_with(b"\r\n")));
                    assert!(c.args.len() <= limits.max_args);
                    assert!(c.name == c.name.to_ascii_uppercase());
                    assert!(c.args.iter().all(|a| match a {
                        Arg::Str(s) => s.len() <= limits.max_literal.max(limits.max_line),
                        Arg::Atom(a) => !a.is_empty() && a.len() <= limits.max_line,
                    }));
                    // What follows the command does not change it, and a shorter prefix is not a whole command.
                    let own = data.get(..used).unwrap_or(&[]);
                    assert_eq!(parse(own, &limits, bare), Parsed::Done(c, used));
                    assert!(matches!(parse(data.get(..used - 1).unwrap_or(&[]), &limits, bare), Parsed::Need), "a prefix of a command is incomplete");
                }
            }
        }
    }
}
