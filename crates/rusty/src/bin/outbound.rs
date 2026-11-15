//! `rusty-outbound`: the rusty-outbound service on its own (docs/superpowers/plans/2026-11-14-per-service-images-design.md). All logic is in the library (`rusty::cli`).
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    rusty::cli::main_role(rusty_core::config::Role::Outbound, std::env::args_os().collect())
}
