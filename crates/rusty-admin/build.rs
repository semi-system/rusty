//! Embeds the built front end (`web/admin/dist`, or the directory named by `RUSTY_ADMIN_WEB_DIST`) into the binary, so the image holds one executable and
//! nothing on the server's disk can be edited to change the interface (design section 2).
//!
//! * No directory (a plain `cargo build` without Node): the table is empty and the service logs that no front end is built in. It is still a working
//!   API server; this is how the Rust tests and a developer's build run.
//! * `RUSTY_ADMIN_WEB_REQUIRED=1` makes an empty or missing front end a **build error**: the release and image builds set it, so an image can never ship
//!   without its interface.
//!
//! The generated file is a table of `(URL path, bytes)` made with `include_bytes!`, in a stable order.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(read) = fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let url = format!("/{}", rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"));
            out.push((url, path));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTY_ADMIN_WEB_DIST");
    println!("cargo:rerun-if-env-changed=RUSTY_ADMIN_WEB_REQUIRED");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let dist = env::var_os("RUSTY_ADMIN_WEB_DIST").map_or_else(|| manifest.join("..").join("..").join("web").join("admin").join("dist"), PathBuf::from);
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut files = Vec::new();
    walk(&dist, &dist, &mut files);
    files.sort();
    let has_index = files.iter().any(|(u, _)| u == "/index.html");
    if env::var("RUSTY_ADMIN_WEB_REQUIRED").is_ok_and(|v| v == "1") && !has_index {
        panic!("RUSTY_ADMIN_WEB_REQUIRED=1 but {} has no index.html: build the front end first (web/admin: npm ci --ignore-scripts && npx vite build)", dist.display());
    }
    // Without an index page the directory is not a front end (a stray file must not turn into a half interface).
    if !has_index {
        files.clear();
    }
    let mut code = String::from("/// The embedded front end: `(URL path, bytes)`.\npub static FILES: &[(&str, &[u8])] = &[\n");
    for (url, path) in &files {
        let abs = path.canonicalize().unwrap_or_else(|_| path.clone());
        let _ = writeln!(code, "    ({url:?}, include_bytes!({:?})),", abs.to_string_lossy());
    }
    code.push_str("];\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("assets.rs");
    fs::write(out, code).expect("write the asset table");
}
