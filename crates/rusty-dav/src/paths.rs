//! The paths of the service (`/dav/...`) and the way a person's address and the names of collections and objects go into and out of them.

/// `calendar` or `addressbook`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Calendar,
    AddressBook,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Calendar => "calendar",
            Kind::AddressBook => "addressbook",
        }
    }

    /// The first segment of the paths of this kind.
    pub fn root(self) -> &'static str {
        match self {
            Kind::Calendar => "calendars",
            Kind::AddressBook => "addressbooks",
        }
    }

    /// The extension of the names of its objects.
    pub fn extension(self) -> &'static str {
        match self {
            Kind::Calendar => ".ics",
            Kind::AddressBook => ".vcf",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// `/dav/`
    Root,
    /// `/dav/principals/{account}/`
    Principal(String),
    /// `/dav/calendars/{account}/` and `/dav/addressbooks/{account}/`
    Home(Kind, String),
    /// `/dav/calendars/{account}/{name}/`
    Collection(Kind, String, String),
    /// `/dav/calendars/{account}/{name}/{object}`
    Object(Kind, String, String, String),
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// One path segment as text: escapes undone once, UTF-8, no control character, no slash, no dot segment.
pub fn decode_segment(raw: &str) -> Option<String> {
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c == b'%' {
            let h = hex(*b.get(i + 1)?)?;
            let l = hex(*b.get(i + 2)?)?;
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    let s = String::from_utf8(out).ok()?;
    if s.is_empty() || s == "." || s == ".." || s.chars().any(|c| c.is_control() || c == '/' || c == '\\') {
        return None;
    }
    Some(s)
}

/// A segment for a path: everything but letters, digits and `-._~` escaped (so an address with `@` is `%40`).
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The names of collections and objects: what the table accepts.
pub fn name_ok(s: &str) -> bool {
    (1..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Which resource a path names (`None` for a path of the service that is not one, or not valid).
pub fn parse(path: &str) -> Option<Resource> {
    let rest = path.strip_prefix('/')?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let segs: Vec<String> = rest.split('/').map(decode_segment).collect::<Option<_>>()?;
    let mut it = segs.iter().map(String::as_str);
    if it.next()? != "dav" {
        return None;
    }
    let parts: Vec<&str> = it.collect();
    match parts.as_slice() {
        [] => Some(Resource::Root),
        ["principals", a] => Some(Resource::Principal((*a).to_owned())),
        [root @ ("calendars" | "addressbooks"), a] => Some(Resource::Home(kind_of(root)?, (*a).to_owned())),
        [root @ ("calendars" | "addressbooks"), a, c] if name_ok(c) => Some(Resource::Collection(kind_of(root)?, (*a).to_owned(), (*c).to_owned())),
        [root @ ("calendars" | "addressbooks"), a, c, o] if name_ok(c) && name_ok(o) => Some(Resource::Object(kind_of(root)?, (*a).to_owned(), (*c).to_owned(), (*o).to_owned())),
        _ => None,
    }
}

fn kind_of(root: &str) -> Option<Kind> {
    match root {
        "calendars" => Some(Kind::Calendar),
        "addressbooks" => Some(Kind::AddressBook),
        _ => None,
    }
}

pub fn principal_href(address: &str) -> String {
    format!("/dav/principals/{}/", encode_segment(address))
}

pub fn home_href(kind: Kind, address: &str) -> String {
    format!("/dav/{}/{}/", kind.root(), encode_segment(address))
}

pub fn collection_href(kind: Kind, address: &str, name: &str) -> String {
    format!("{}{}/", home_href(kind, address), encode_segment(name))
}

pub fn object_href(kind: Kind, address: &str, collection: &str, name: &str) -> String {
    format!("{}{}", collection_href(kind, address, collection), encode_segment(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_name_resources() {
        assert_eq!(parse("/dav/"), Some(Resource::Root));
        assert_eq!(parse("/dav"), Some(Resource::Root));
        assert_eq!(parse("/dav/principals/ada%40one.test/"), Some(Resource::Principal("ada@one.test".into())));
        assert_eq!(parse("/dav/calendars/ada@one.test"), Some(Resource::Home(Kind::Calendar, "ada@one.test".into())));
        assert_eq!(parse("/dav/addressbooks/a/work/"), Some(Resource::Collection(Kind::AddressBook, "a".into(), "work".into())));
        assert_eq!(parse("/dav/calendars/a/main/x-1.ics"), Some(Resource::Object(Kind::Calendar, "a".into(), "main".into(), "x-1.ics".into())));
        for bad in ["/", "/other/", "/dav/principals/", "/dav/calendars/a/b/c/d", "/dav/calendars/a/b c/", "/dav/calendars/a/%2e%2e/", "/dav/calendars/a/x%2Fy/", "/dav/calendars/a/./", "/dav/calendars//", "/dav/calendars/a/%zz/", "/dav/calendars/a/%00/", "/dav/nothing", "dav/", "/dav/calendars/a/main/ob ject"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn hrefs_round_trip() {
        let h = object_href(Kind::Calendar, "ada@one.test", "main", "e-1.ics");
        assert_eq!(h, "/dav/calendars/ada%40one.test/main/e-1.ics");
        assert_eq!(parse(&h), Some(Resource::Object(Kind::Calendar, "ada@one.test".into(), "main".into(), "e-1.ics".into())));
        assert_eq!(encode_segment("a b/ü"), "a%20b%2F%C3%BC");
        assert_eq!(decode_segment("a%20b%C3%BC").as_deref(), Some("a bü"));
        assert!(name_ok("a.b_c-1") && !name_ok("") && !name_ok("a b") && !name_ok(&"x".repeat(129)));
    }
}
