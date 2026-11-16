//! `rusty-outbound`: the rusty-outbound service on its own (docs/design-per-service-images.md). All logic is in the library (`rusty::cli`).
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    rusty::cli::main_role(rusty_core::config::Role::Outbound, std::env::args_os().collect())
}
