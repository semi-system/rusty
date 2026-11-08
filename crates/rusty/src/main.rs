//! `rusty`: multi-call entry point. All logic is in the library (`rusty::cli`).
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    rusty::cli::main(std::env::args_os().collect())
}
