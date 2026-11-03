#![forbid(unsafe_code)]

use std::path::Path;

fn main() {
    let mut args = std::env::args_os();
    let _program = args.next();
    let Some(path) = args.next() else {
        eprintln!("usage: compose-check <compose.yaml>");
        std::process::exit(2);
    };
    if args.next().is_some() {
        eprintln!("usage: compose-check <compose.yaml>");
        std::process::exit(2);
    }
    match compose_check::check_path(Path::new(&path)) {
        Ok(None) => {}
        Ok(Some(violation)) => {
            eprintln!("{}: {}", violation.rule, violation.message);
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("COMPOSE_INVALID: cannot read or parse compose file");
            std::process::exit(2);
        }
    }
}
