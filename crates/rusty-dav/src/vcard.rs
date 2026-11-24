//! A vCard of an address book collection (RFC 6352 section 6.3.2.1): one `VCARD` of version 3.0 or 4.0 with a formatted name. What the server reads from it is what the queries filter on: the UID, the name, the
//! e-mail addresses, the telephone numbers and the organisation.

use crate::content::{self, Component};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub uid: Option<String>,
    pub full_name: String,
    pub emails: Vec<String>,
    pub tels: Vec<String>,
    pub org: Option<String>,
}

const MAX_UID_CHARS: usize = 255;

fn texts(c: &Component, name: &str) -> Vec<String> {
    c.props_named(name).map(|p| p.value.clone()).filter(|v| !v.is_empty()).collect()
}

pub fn read(text: &str) -> Result<Card, &'static str> {
    let root = content::parse(text)?;
    if root.name != "VCARD" {
        return Err("the object is not a VCARD");
    }
    if !root.children.is_empty() {
        return Err("a card holds no component");
    }
    if !matches!(root.prop("VERSION").map(|p| p.value.as_str()), Some("3.0" | "4.0")) {
        return Err("the version is not 3.0 or 4.0");
    }
    let Some(full_name) = root.prop("FN").map(|p| p.value.trim().to_owned()).filter(|n| !n.is_empty()) else { return Err("the card has no FN") };
    let uid = match root.prop("UID").map(|p| p.value.as_str()) {
        None | Some("") => None,
        Some(u) if u.chars().count() > MAX_UID_CHARS || u.chars().any(char::is_control) => return Err("the UID is not valid"),
        Some(u) => Some(u.to_owned()),
    };
    Ok(Card { uid, full_name, emails: texts(&root, "EMAIL"), tels: texts(&root, "TEL"), org: root.prop("ORG").map(|p| p.value.clone()).filter(|v| !v.is_empty()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_gives_its_name_addresses_and_numbers() {
        let c = read("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:urn:uuid:1\r\nFN:Ada Lovelace\r\nEMAIL;TYPE=work:ada@example.org\r\nitem1.EMAIL:ada@home.example\r\nTEL;VALUE=uri:tel:+1-555-0100\r\nORG:Analytical Engines;Research\r\nEND:VCARD\r\n").unwrap();
        assert_eq!(c.uid.as_deref(), Some("urn:uuid:1"));
        assert_eq!((c.full_name.as_str(), c.emails.len(), c.tels.len(), c.org.as_deref()), ("Ada Lovelace", 2, 1, Some("Analytical Engines;Research")));
        let bare = read("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:X\r\nN:;X;;;\r\nEND:VCARD\r\n").unwrap();
        assert_eq!((bare.uid, bare.emails.is_empty()), (None, true));
    }

    #[test]
    fn what_is_not_a_card_is_refused() {
        for bad in [
            "BEGIN:VCARD\r\nVERSION:2.1\r\nFN:X\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nFN:X\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:  \r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:X\r\nBEGIN:X\r\nEND:X\r\nEND:VCARD\r\n",
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n",
        ] {
            assert!(read(bad).is_err(), "{bad:?}");
        }
        assert!(read(&format!("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:X\r\nUID:{}\r\nEND:VCARD\r\n", "u".repeat(256))).is_err());
    }
}
