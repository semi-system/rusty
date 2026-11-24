//! The contacts of the person for the composer (docs/design-caldav-carddav.md, step 5): `GET /api/v1/contacts?q=ada` answers `{"items":[{"name":"Ada Lovelace","email":"ada@example.org"}]}`, at most eight, from the
//! address books the person keeps through CardDAV. Read only: the webmail's database role may read the address books and change nothing in them. The query is two to sixty-four characters, never markup, and
//! only what the person's own cards say comes back.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use serde::Serialize;

use crate::api::{error, WebmailApp};

/// How many suggestions at most.
const MAX_ITEMS: usize = 8;
/// How many cards are read to find them.
const CANDIDATES: i64 = 100;
const MIN_QUERY: usize = 2;
const MAX_QUERY: usize = 64;
/// Addresses of one card that are looked at.
const MAX_EMAILS_PER_CARD: usize = 5;

#[derive(Serialize)]
struct Item {
    name: String,
    email: String,
}

#[derive(Serialize)]
struct Items {
    items: Vec<Item>,
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// A query-string value: `+` is a space, `%XX` a byte; text that is not UTF-8 or holds a control character is not understood.
fn decode(raw: &str) -> Option<String> {
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        match c {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                out.push(hex(*b.get(i + 1)?)? * 16 + hex(*b.get(i + 2)?)?);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    let s = String::from_utf8(out).ok()?;
    (!s.chars().any(char::is_control)).then_some(s)
}

/// The query of `?q=...` (and nothing else is accepted).
fn needle(req: &Request) -> Option<String> {
    let q = req.query.as_deref()?;
    let raw = q.strip_prefix("q=")?;
    if raw.contains('&') {
        return None;
    }
    let s = decode(raw)?;
    let s = s.trim().to_owned();
    (MIN_QUERY..=MAX_QUERY).contains(&s.chars().count()).then_some(s)
}

/// The suggestions for a query out of the text of the cards: a card is a suggestion for each of its addresses when its name or that address holds the query.
pub fn suggestions(cards: &[String], q: &str) -> Vec<(String, String)> {
    let q = q.to_lowercase();
    let mut out: Vec<(String, String)> = Vec::new();
    for body in cards {
        let Ok(card) = rusty_dav::vcard::read(body) else { continue };
        let name_hits = card.full_name.to_lowercase().contains(&q);
        for email in card.emails.iter().take(MAX_EMAILS_PER_CARD) {
            let email = email.trim();
            // An address that could not be one is not suggested (a card is data anyone with the password could write).
            if email.is_empty() || email.len() > 254 || !email.contains('@') || email.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',' | ';' | '"')) {
                continue;
            }
            if (name_hits || email.to_lowercase().contains(&q)) && !out.iter().any(|(_, e)| e.eq_ignore_ascii_case(email)) {
                out.push((card.full_name.clone(), email.to_owned()));
            }
        }
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.1.cmp(&b.1)));
    out.truncate(MAX_ITEMS);
    out
}

async fn list(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(q) = needle(&req) else { return error(400, "invalid") };
    match app.dav.search_cards(session.account_id(), &q, CANDIDATES).await {
        Ok(found) => {
            let bodies: Vec<String> = found.into_iter().map(|o| o.body).collect();
            let items = suggestions(&bodies, &q).into_iter().map(|(name, email)| Item { name, email }).collect();
            Response::json(200, &Items { items }).with_header("cache-control", "no-store")
        }
        Err(_) => error(503, "busy"),
    }
}

pub fn routes(app: Arc<WebmailApp>, router: Router) -> Router {
    router.route(Method::Get, "/api/v1/contacts", handler(move |req: Request| {
        let app = app.clone();
        async move { list(&app, req).await }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(name: &str, emails: &[&str]) -> String {
        let mails: String = emails.iter().map(|e| format!("EMAIL:{e}\r\n")).collect();
        format!("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{name}\r\n{mails}END:VCARD\r\n")
    }

    #[test]
    fn a_card_is_suggested_by_its_name_or_its_address_for_each_address_once() {
        let cards = vec![card("Ada Lovelace", &["ada@example.org", "ada@home.example"]), card("Alan Turing", &["alan@example.org"]), card("Grace Hopper", &["grace@navy.example"])];
        let names = |q: &str| suggestions(&cards, q).into_iter().map(|(n, e)| format!("{n} <{e}>")).collect::<Vec<_>>();
        assert_eq!(names("lovel"), ["Ada Lovelace <ada@example.org>", "Ada Lovelace <ada@home.example>"], "the name matches: every address of the card");
        assert_eq!(names("HOME"), ["Ada Lovelace <ada@home.example>"], "the address matches, case does not matter");
        assert_eq!(names("example.org"), ["Ada Lovelace <ada@example.org>", "Alan Turing <alan@example.org>"]);
        assert!(names("nobody").is_empty());
        // The same address in two cards is one suggestion.
        let twice = vec![card("Ada", &["ada@example.org"]), card("Ada L", &["ADA@example.org"])];
        assert_eq!(suggestions(&twice, "ada").len(), 1);
    }

    #[test]
    fn what_could_not_be_an_address_is_never_suggested_and_the_list_is_bounded() {
        let cards = vec![card("Evil", &["a b@example.org", "x<script>@example.org", "no-at-sign", "ok@example.org", "q\"@example.org", "a,b@example.org"])];
        assert_eq!(suggestions(&cards, "evil"), [("Evil".to_owned(), "ok@example.org".to_owned())]);
        let many: Vec<String> = (0..30).map(|i| card(&format!("Person {i:02}"), &[&format!("p{i}@example.org")])).collect();
        assert_eq!(suggestions(&many, "person").len(), MAX_ITEMS);
        assert!(suggestions(&["not a card".to_owned()], "x").is_empty());
    }

    #[test]
    fn the_query_is_decoded_and_bounded() {
        assert_eq!(decode("ada+l%C3%B6v").as_deref(), Some("ada löv"));
        for bad in ["%zz", "%4", "%00", "%FF", "a%0Ab"] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }
}
