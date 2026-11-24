//! The contacts of the person for the composer (docs/design-caldav-carddav.md, step 5): `GET /api/v1/contacts?q=ada` answers `{"items":[{"name":"Ada Lovelace","email":"ada@example.org"}]}`, at most eight, from the
//! address books the person keeps through CardDAV. Read only: the webmail's database role may read the address books and change nothing in them. The query is two to sixty-four characters, never markup, and
//! only what the person's own cards say comes back.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

/// Most cards one export holds.
const EXPORT_MAX: i64 = 5000;

/// The text of an export: every card of the person, each ended by CRLF, the cards one after the other (a `.vcf` file is a concatenation of vCards).
pub fn export_text(cards: &[String]) -> String {
    let mut out = String::new();
    for body in cards {
        let body = body.trim_end_matches(['\r', '\n']);
        if body.is_empty() {
            continue;
        }
        out.push_str(&body.replace("\r\n", "\n").replace('\n', "\r\n"));
        out.push_str("\r\n");
    }
    out
}

/// `GET /api/v1/contacts/export`: the contacts of the person as one `contacts.vcf` file (read only, like the suggestions).
async fn export(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match app.dav.search_cards(session.account_id(), "BEGIN:VCARD", EXPORT_MAX).await {
        Ok(found) => {
            let bodies: Vec<String> = found.into_iter().map(|o| o.body).collect();
            Response::new(200)
                .with_header("content-type", "text/vcard; charset=utf-8")
                .with_header("content-disposition", "attachment; filename=\"contacts.vcf\"")
                .with_header("cache-control", "no-store")
                .with_header("x-content-type-options", "nosniff")
                .with_body(export_text(&bodies).into_bytes())
        }
        Err(_) => error(503, "busy"),
    }
}

/// Most cards one import request holds (the body limit of the service is far lower for any real card).
const IMPORT_MAX_CARDS: usize = 500;

#[derive(Deserialize)]
struct ImportBody {
    vcf: String,
}

#[derive(Serialize, Default, Debug, PartialEq, Eq)]
struct Imported {
    /// New contacts.
    imported: usize,
    /// Contacts the person already had (same UID, same name) that were replaced.
    updated: usize,
    /// Cards that are not valid vCards.
    invalid: usize,
    /// Cards whose UID another contact already has under another name.
    duplicates: usize,
}

/// The cards of a `.vcf` text, each one as its own text with CRLF line ends: what lies between a `BEGIN:VCARD` line and the next `END:VCARD` line (anything outside is ignored). At most `max`.
pub fn split_cards(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<Vec<&str>> = None;
    for line in text.lines() {
        let upper = line.trim_end().to_ascii_uppercase();
        if upper == "BEGIN:VCARD" {
            cur = Some(vec![line.trim_end()]);
        } else if let Some(lines) = cur.as_mut() {
            lines.push(line.trim_end_matches('\r'));
            if upper == "END:VCARD" {
                out.push(lines.join("\r\n") + "\r\n");
                cur = None;
                if out.len() >= max {
                    break;
                }
            }
        }
    }
    out
}

/// The object of a card: the name of the stored object comes from its UID (the same card imported again is the same object); a card with no UID is given one.
pub fn to_object(card_text: &str, random: [u8; 16]) -> Result<rusty_store::NewDavObject, &'static str> {
    let card = rusty_dav::vcard::read(card_text)?;
    let (uid, body) = match card.uid {
        Some(uid) => (uid, card_text.to_owned()),
        None => {
            let uid = format!("urn:uuid:{}", hex::encode(random));
            (uid.clone(), card_text.replacen("\r\n", &format!("\r\nUID:{uid}\r\n"), 1))
        }
    };
    let name = format!("{}.vcf", hex::encode(&Sha256::digest(uid.as_bytes())[..16]));
    Ok(rusty_store::NewDavObject { name, uid, body, component: "VCARD".into(), starts_at: None, ends_at: None, recurring: false })
}

/// `POST /api/v1/contacts/import` with `{"vcf": "<text of a .vcf file>"}`: the cards are added to the default address book of the person (or the first one). The writes are the DAV service's own
/// (`DavRepo::put`), so the contacts are at once what a CardDAV client sees and syncs. A person who has no address book gets a 409: the book is made by the DAV service, never here.
async fn import(app: &WebmailApp, req: Request) -> Response {
    let session = match app.require(&req, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<ImportBody>(&req.body) else { return error(400, "invalid") };
    let cards = split_cards(&b.vcf, IMPORT_MAX_CARDS);
    if cards.is_empty() {
        return error(400, "invalid");
    }
    let account = session.account_id();
    let Ok(books) = app.dav.collections(account, "addressbook").await else { return error(503, "busy") };
    let Some(book) = books.iter().find(|c| c.name == "default").or(books.first()) else { return error(409, "no_address_book") };
    let mut result = Imported::default();
    for text in cards {
        let mut random = [0u8; 16];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut random);
        let Ok(object) = to_object(&text, random) else {
            result.invalid += 1;
            continue;
        };
        match app.dav.put(book.id, &object, &rusty_store::DavPrecondition::None).await {
            Ok(rusty_store::DavPutResult::Created { .. }) => result.imported += 1,
            Ok(rusty_store::DavPutResult::Updated { .. }) => result.updated += 1,
            Ok(rusty_store::DavPutResult::UidConflict(_)) => result.duplicates += 1,
            Ok(rusty_store::DavPutResult::Full(_)) => return error(409, "full"),
            Ok(rusty_store::DavPutResult::PreconditionFailed) => result.invalid += 1,
            Err(_) => return error(503, "busy"),
        }
    }
    Response::json(200, &result).with_header("cache-control", "no-store")
}

pub fn routes(app: Arc<WebmailApp>, router: Router) -> Router {
    let exporter = app.clone();
    let importer = app.clone();
    router
        .route(Method::Get, "/api/v1/contacts/export", handler(move |req: Request| {
            let app = exporter.clone();
            async move { export(&app, req).await }
        }))
        .route(Method::Post, "/api/v1/contacts/import", handler(move |req: Request| {
            let app = importer.clone();
            async move { import(&app, req).await }
        }))
        .route(Method::Get, "/api/v1/contacts", handler(move |req: Request| {
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
    fn an_export_is_the_cards_one_after_the_other_each_ended_by_crlf() {
        let out = export_text(&["BEGIN:VCARD\nFN:A\nEND:VCARD\n".to_owned(), "".to_owned(), "BEGIN:VCARD\r\nFN:B\r\nEND:VCARD".to_owned()]);
        assert_eq!(out, "BEGIN:VCARD\r\nFN:A\r\nEND:VCARD\r\nBEGIN:VCARD\r\nFN:B\r\nEND:VCARD\r\n");
        assert_eq!(export_text(&[]), "");
    }


    #[test]
    fn a_vcf_file_is_split_into_cards_and_a_card_without_a_uid_is_given_one() {
        let vcf = "junk\nBEGIN:VCARD\nVERSION:3.0\nFN:Ada\nEND:VCARD\r\n\r\nbegin:vcard\r\nVERSION:3.0\r\nUID:x-1\r\nFN:Alan\r\nend:vcard\r\nBEGIN:VCARD\r\nFN:cut off";
        let cards = split_cards(vcf, 10);
        assert_eq!(cards.len(), 2, "{cards:?}");
        assert!(cards[0].starts_with("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ada\r\n") && cards[0].ends_with("END:VCARD\r\n"));
        assert_eq!(split_cards(vcf, 1).len(), 1, "the bound holds");
        let a = to_object(&cards[0], [7; 16]).unwrap();
        assert!(a.uid.starts_with("urn:uuid:07070707") && a.body.contains(&format!("\r\nUID:{}\r\n", a.uid)) && a.name.ends_with(".vcf"), "{a:?}");
        // The name comes from the UID: the same card again is the same object; another UID is another.
        let b = to_object(&cards[1], [1; 16]).unwrap();
        assert_eq!(b.uid, "x-1");
        assert_eq!(b.name, to_object(&cards[1], [2; 16]).unwrap().name);
        assert_ne!(a.name, b.name);
        assert!(rusty_dav::paths::name_ok(&a.name));
        assert!(to_object("BEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n", [0; 16]).is_err(), "a card with no name is refused");
    }
    #[test]
    fn the_query_is_decoded_and_bounded() {
        assert_eq!(decode("ada+l%C3%B6v").as_deref(), Some("ada löv"));
        for bad in ["%zz", "%4", "%00", "%FF", "a%0Ab"] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }
}
