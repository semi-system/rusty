//! Workspace-wide unsafe ban. Lives in rusty-core because it is the root of the dependency
//! graph; the test locates the workspace via CARGO_MANIFEST_DIR and scans every crate.

use std::fs;
use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Source with comments, string/char literals blanked out (simple state machine; handles
/// nested block comments, raw strings and escapes well enough for a token scan).
fn strip(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut o = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let n = b.get(i + 1).copied().unwrap_or(char::from(0));
        if c == char::from(47) && n == char::from(47) {
            while i < b.len() && b[i] != char::from(10) {
                i += 1;
            }
        } else if c == char::from(47) && n == char::from(42) {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == char::from(47) && b.get(i + 1) == Some(&char::from(42)) {
                    depth += 1;
                    i += 2;
                } else if b[i] == char::from(42) && b.get(i + 1) == Some(&char::from(47)) {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if c == char::from(114) && (n == char::from(34) || n == char::from(35)) && {
            // raw string r"..." / r#"..."#
            let mut j = i + 1;
            while b.get(j) == Some(&char::from(35)) {
                j += 1;
            }
            b.get(j) == Some(&char::from(34))
        } {
            let mut j = i + 1;
            let mut hashes = 0;
            while b[j] == char::from(35) {
                hashes += 1;
                j += 1;
            }
            j += 1;
            loop {
                if j >= b.len() {
                    break;
                }
                if b[j] == char::from(34) && (0..hashes).all(|k| b.get(j + 1 + k) == Some(&char::from(35))) {
                    j += 1 + hashes;
                    break;
                }
                j += 1;
            }
            i = j;
        } else if c == char::from(34) {
            i += 1;
            while i < b.len() && b[i] != char::from(34) {
                if b[i] == char::from(92) {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
        } else if c == char::from(39) && b.get(i + 2) == Some(&char::from(39)) {
            i += 3; // simple char literal like x
        } else if c == char::from(39) && n == char::from(92) {
            i += 2;
            while i < b.len() && b[i] != char::from(39) {
                i += 1;
            }
            i += 1;
        } else {
            o.push(c);
            i += 1;
        }
    }
    o
}

fn has_unsafe_token(code: &str) -> bool {
    let mut cur = String::new();
    for c in code.chars().chain(std::iter::once(char::from(32))) {
        if c.is_alphanumeric() || c == char::from(95) {
            cur.push(c);
        } else {
            if cur == "unsafe" {
                return true;
            }
            cur.clear();
        }
    }
    false
}

/// The attribute must appear in real code: it is looked up on comment-stripped text, so an
/// occurrence inside a `//` or `/* */` comment does not count.
fn has_attr(text: &str, attr: &str) -> bool {
    strip_comments(text).lines().any(|l| l.trim() == attr)
}

fn strip_comments(src: &str) -> String {
    // Same state machine as `strip`, but string literals are kept (attribute has no string).
    let b: Vec<char> = src.chars().collect();
    let mut o = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if b[i] == '/' && b.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else {
            o.push(b[i]);
            i += 1;
        }
    }
    o
}

fn crate_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(crates_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    v.sort();
    v
}

#[test]
fn workspace_forbids_unsafe() {
    let crates = crate_dirs();
    assert!(crates.len() >= 4, "found only {crates:?}");
    for c in &crates {
        let lib = c.join("src/lib.rs");
        let text = fs::read_to_string(&lib).unwrap_or_else(|e| panic!("{}: {e}", lib.display()));
        let attr = format!("#![forbid({}_code)]", "unsafe");
        assert!(
            has_attr(&text, &attr),
            "{} lacks {attr} (outside comments)",
            lib.display()
        );
        let mut files = Vec::new();
        rs_files(&c.join("src"), &mut files);
        for f in files {
            let code = strip(&fs::read_to_string(&f).unwrap());
            // The forbid attribute itself contains the token; remove it before scanning.
            let code = code.replace(&attr, "");
            assert!(!has_unsafe_token(&code), "unsafe token in {}", f.display());
        }
    }
}

#[test]
fn scanner_detects_unsafe_and_ignores_comments_and_strings() {
    assert!(has_unsafe_token(&strip("fn f() { unsafe { } }")));
    assert!(has_unsafe_token(&strip("unsafe fn f() {}")));
    assert!(!has_unsafe_token(&strip("// unsafe\nfn f() {}")));
    assert!(!has_unsafe_token(&strip("/* unsafe /* nested */ unsafe */ fn f() {}")));
    assert!(!has_unsafe_token(&strip("let s = \"unsafe \\\" unsafe\";")));
    assert!(!has_unsafe_token(&strip("let s = r#\"unsafe\"#;")));
    assert!(!has_unsafe_token(&strip("let unsafe_thing = 1;")));
}

#[test]
fn forbid_attribute_inside_a_comment_does_not_count() {
    let attr = format!("#![forbid({}_code)]", "unsafe");
    assert!(has_attr(&format!("{attr}\nfn f() {{}}"), &attr));
    assert!(!has_attr(&format!("/*\n{attr}\n*/\nfn f() {{}}"), &attr));
    assert!(!has_attr(&format!("// {attr}\nfn f() {{}}"), &attr));
}
