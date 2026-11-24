//! The DAV methods and reports over a real database: a client discovers its principal and its collections, puts, reads, changes and removes events and contacts, asks for ranges and for what changed, and reaches
//! nobody else's tree.


use rusty_dav::server::{Dav, Principal, Req, Reply, Verb};
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{Store, StoreConfig};

struct World {
    dav: Dav,
    alice: Principal,
    bob: Principal,
    _db: pg::PgTestDb,
}

async fn world() -> World {
    let url = pg::pg_admin_url().expect("a Postgres test server (RUSTY_TEST_PG_URL)");
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let seed = Store::open(StoreConfig::postgres(db.seed_url())).await.unwrap();
    let dir = seed.repos().directory.clone();
    let dom = dir.create_domain("one.test").await.unwrap();
    let mut people = Vec::new();
    for local in ["alice", "bob"] {
        let acc = dir.create_account(&rusty_store::repo::NewAccount { local: local.to_owned(), domain: dom, plan: None, password_hash: None }).await.unwrap();
        people.push(Principal { account: acc, address: format!("{local}@one.test") });
    }
    let bob = people.pop().unwrap();
    let alice = people.pop().unwrap();
    World { dav: Dav::new(store.repos().dav.clone()), alice, bob, _db: db }
}

#[derive(Default, Clone)]
struct R<'a> {
    depth: Option<&'a str>,
    if_match: Option<&'a str>,
    if_none_match: Option<&'a str>,
    destination: Option<&'a str>,
    overwrite: Option<&'a str>,
}

impl World {
    async fn call(&self, who: &Principal, verb: Verb, path: &str, body: &str, h: R<'_>) -> Reply {
        let req = Req { verb, path, depth: h.depth, if_match: h.if_match, if_none_match: h.if_none_match, destination: h.destination, overwrite: h.overwrite, body: body.as_bytes() };
        self.dav.handle(who, &req).await
    }

    async fn a(&self, verb: Verb, path: &str, body: &str) -> Reply {
        self.call(&self.alice, verb, path, body, R { depth: Some("0"), ..R::default() }).await
    }
}

fn header<'a>(r: &'a Reply, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
}

const CAL: &str = "/dav/calendars/alice%40one.test/default/";
const BOOK: &str = "/dav/addressbooks/alice%40one.test/default/";

fn event(uid: &str, summary: &str, start: &str, end: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\nDTSTART:{start}\r\nDTEND:{end}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
}

fn card(uid: &str, name: &str, email: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{name}\r\nEMAIL:{email}\r\nEND:VCARD\r\n")
}

fn propfind(props: &str) -> String {
    format!("<?xml version=\"1.0\"?><D:propfind xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\" xmlns:CS=\"http://calendarserver.org/ns/\"><D:prop>{props}</D:prop></D:propfind>")
}

#[tokio::test]
async fn a_client_discovers_its_principal_and_its_default_collections() {
    let w = world().await;
    let o = w.a(Verb::Options, "/dav/", "").await;
    assert_eq!(o.status, 200);
    assert!(header(&o, "dav").unwrap().contains("calendar-access") && header(&o, "dav").unwrap().contains("addressbook"));
    // The root names the principal; the principal names the homes.
    let r = w.a(Verb::Propfind, "/dav/", &propfind("<D:current-user-principal/><D:resourcetype/>")).await;
    assert_eq!(r.status, 207);
    assert!(r.body.contains("<D:href>/dav/principals/alice%40one.test/</D:href></D:current-user-principal>"), "{}", r.body);
    let r = w.a(Verb::Propfind, "/dav/principals/alice@one.test/", &propfind("<C:calendar-home-set/><CR:addressbook-home-set/><D:displayname/><D:nothing-here/>")).await;
    assert!(r.body.contains("<C:calendar-home-set><D:href>/dav/calendars/alice%40one.test/</D:href></C:calendar-home-set>"), "{}", r.body);
    assert!(r.body.contains("<CR:addressbook-home-set><D:href>/dav/addressbooks/alice%40one.test/</D:href>"), "{}", r.body);
    assert!(r.body.contains("<D:prop><D:nothing-here/></D:prop><D:status>HTTP/1.1 404 Not Found"), "an unknown property is a 404 propstat of its own: {}", r.body);
    // The calendar home lists the calendar a new account has.
    let r = w.call(&w.alice, Verb::Propfind, "/dav/calendars/alice%40one.test/", &propfind("<D:resourcetype/><D:displayname/><CS:getctag/><D:sync-token/><C:supported-calendar-component-set/>"), R { depth: Some("1"), ..R::default() }).await;
    assert_eq!(r.status, 207);
    assert!(r.body.contains("<D:href>/dav/calendars/alice%40one.test/default/</D:href>") && r.body.contains("<D:collection/><C:calendar/>"), "{}", r.body);
    assert!(r.body.contains("<CS:getctag>0</CS:getctag>") && r.body.contains("<C:comp name=\"VEVENT\"/>"), "{}", r.body);
    let books = w.call(&w.alice, Verb::Propfind, "/dav/addressbooks/alice%40one.test/", "", R { depth: Some("1"), ..R::default() }).await;
    assert!(books.body.contains("<CR:addressbook/>"), "{}", books.body);
    // Depth infinity and a missing Depth are refused; a body that is not a propfind is a 400.
    let r = w.call(&w.alice, Verb::Propfind, "/dav/", "", R { depth: Some("infinity"), ..R::default() }).await;
    assert_eq!(r.status, 403);
    assert!(r.body.contains("propfind-finite-depth"));
    assert_eq!(w.call(&w.alice, Verb::Propfind, "/dav/", "", R::default()).await.status, 403);
    assert_eq!(w.a(Verb::Propfind, "/dav/", "<a/>").await.status, 400);
    assert_eq!(w.a(Verb::Propfind, "/dav/", "<!DOCTYPE a><a/>").await.status, 400);
    // Nobody else's tree, and nothing that is not a path of the service.
    assert_eq!(w.a(Verb::Propfind, "/dav/calendars/bob%40one.test/default/", "").await.status, 403);
    assert_eq!(w.a(Verb::Get, "/dav/calendars/bob%40one.test/default/x.ics", "").await.status, 403);
    assert_eq!(w.a(Verb::Propfind, "/elsewhere/", "").await.status, 404);
    assert_eq!(w.a(Verb::Propfind, "/dav/calendars/alice%40one.test/a%20b/", "").await.status, 404);
}

#[tokio::test]
async fn an_event_is_put_read_changed_and_removed_with_its_entity_tag() {
    let w = world().await;
    let path = format!("{CAL}e1.ics");
    let ics = event("e1", "Lunch <&> \"quotes\"", "20240101T100000Z", "20240101T110000Z");
    let put = w.call(&w.alice, Verb::Put, &path, &ics, R { if_none_match: Some("*"), ..R::default() }).await;
    assert_eq!(put.status, 201);
    let etag = header(&put, "etag").unwrap().to_owned();
    assert!(etag.starts_with('"') && etag.ends_with('"'));
    assert_eq!(w.call(&w.alice, Verb::Put, &path, &ics, R { if_none_match: Some("*"), ..R::default() }).await.status, 412, "If-None-Match: * on a name that is taken");
    // Read back byte for byte, with the tag.
    let got = w.a(Verb::Get, &path, "").await;
    assert_eq!((got.status, got.body.as_str(), header(&got, "etag")), (200, ics.as_str(), Some(etag.as_str())));
    assert!(got.content_type.starts_with("text/calendar"));
    // A change needs the tag it replaces; it gives a new one.
    let changed = ics.replace("Lunch", "Dinner");
    assert_eq!(w.call(&w.alice, Verb::Put, &path, &changed, R { if_match: Some("\"stale\""), ..R::default() }).await.status, 412);
    let upd = w.call(&w.alice, Verb::Put, &path, &changed, R { if_match: Some(&etag), ..R::default() }).await;
    assert_eq!(upd.status, 204);
    assert_ne!(header(&upd, "etag").unwrap(), etag);
    // The properties of the object, and the text of it escaped inside the answer.
    let r = w.call(&w.alice, Verb::Propfind, &path, &propfind("<D:getetag/><D:getcontenttype/><D:getcontentlength/><C:calendar-data/>"), R { depth: Some("0"), ..R::default() }).await;
    assert!(r.body.contains("component=VEVENT") && r.body.contains("Dinner &lt;&amp;&gt; &quot;quotes&quot;"), "{}", r.body);
    // What is not an event is refused with the condition of the protocol; so is a name with the wrong ending and a second object with the UID.
    let bad = w.a(Verb::Put, &format!("{CAL}bad.ics"), "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n").await;
    assert_eq!(bad.status, 403);
    assert!(bad.body.contains("valid-calendar-object-resource"), "{}", bad.body);
    assert_eq!(w.a(Verb::Put, &format!("{CAL}e2.txt"), &ics).await.status, 403);
    let dup = w.a(Verb::Put, &format!("{CAL}other.ics"), &ics).await;
    assert_eq!(dup.status, 409);
    assert!(dup.body.contains("no-uid-conflict") && dup.body.contains("/dav/calendars/alice%40one.test/default/e1.ics"), "{}", dup.body);
    assert_eq!(w.a(Verb::Put, &format!("{CAL}big.ics"), &"x".repeat(1024 * 1024 + 1)).await.status, 413);
    assert_eq!(w.a(Verb::Put, "/dav/calendars/alice%40one.test/nothing/x.ics", &ics).await.status, 409, "no such collection");
    // A delete needs its tag too.
    assert_eq!(w.call(&w.alice, Verb::Delete, &path, "", R { if_match: Some("\"stale\""), ..R::default() }).await.status, 412);
    assert_eq!(w.a(Verb::Delete, &path, "").await.status, 204);
    assert_eq!(w.a(Verb::Delete, &path, "").await.status, 404);
    assert_eq!(w.a(Verb::Get, &path, "").await.status, 404);
}

fn query(start: &str, end: &str, extra: &str) -> String {
    format!(
        "<C:calendar-query xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name=\"VCALENDAR\"><C:comp-filter name=\"VEVENT\"><C:time-range start=\"{start}\" end=\"{end}\"/>{extra}</C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"
    )
}

#[tokio::test]
async fn reports_find_events_by_range_by_text_and_by_name() {
    let w = world().await;
    for (n, sum, s, e) in [("jan", "January party", "20240110T100000Z", "20240110T110000Z"), ("feb", "February review", "20240210T100000Z", "20240210T110000Z"), ("mar", "March party", "20240310T100000Z", "20240310T110000Z")] {
        assert_eq!(w.a(Verb::Put, &format!("{CAL}{n}.ics"), &event(n, sum, s, e)).await.status, 201);
    }
    let hrefs = |r: &Reply| -> Vec<String> { r.body.split("<D:href>").skip(1).map(|p| p.split("</D:href>").next().unwrap().rsplit('/').next().unwrap().to_owned()).collect() };
    let r = w.a(Verb::Report, CAL, &query("20240201T000000Z", "20240301T000000Z", "")).await;
    assert_eq!((r.status, hrefs(&r)), (207, vec!["feb.ics".to_owned()]), "{}", r.body);
    assert!(r.body.contains("SUMMARY:February review"), "the data comes with it");
    let r = w.a(Verb::Report, CAL, &query("20240101T000000Z", "20240401T000000Z", "")).await;
    assert_eq!(hrefs(&r), ["feb.ics", "jan.ics", "mar.ics"]);
    let party = "<C:prop-filter name=\"SUMMARY\"><C:text-match collation=\"i;ascii-casemap\">PARTY</C:text-match></C:prop-filter>";
    assert_eq!(hrefs(&w.a(Verb::Report, CAL, &query("20240101T000000Z", "20240401T000000Z", party)).await), ["jan.ics", "mar.ics"]);
    let not_party = "<C:prop-filter name=\"SUMMARY\"><C:text-match negate-condition=\"yes\">party</C:text-match></C:prop-filter>";
    assert_eq!(hrefs(&w.a(Verb::Report, CAL, &query("20240101T000000Z", "20240401T000000Z", not_party)).await), ["feb.ics"]);
    let no_loc = "<C:prop-filter name=\"LOCATION\"><C:is-not-defined/></C:prop-filter>";
    assert_eq!(hrefs(&w.a(Verb::Report, CAL, &query("20240101T000000Z", "20240201T000000Z", no_loc)).await).len(), 1);
    // Multiget by href: what is there, and a 404 for what is not (and for the paths of another collection).
    let multi = format!(
        "<C:calendar-multiget xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/></D:prop><D:href>{CAL}jan.ics</D:href><D:href>{CAL}none.ics</D:href><D:href>/dav/calendars/alice%40one.test/other/mar.ics</D:href></C:calendar-multiget>"
    );
    let r = w.a(Verb::Report, CAL, &multi).await;
    assert_eq!(r.body.matches("HTTP/1.1 404 Not Found").count(), 2, "{}", r.body);
    assert!(r.body.contains("jan.ics</D:href><D:propstat>"), "{}", r.body);
    // A report this collection does not answer, and a body that is not XML.
    assert_eq!(w.a(Verb::Report, CAL, "<CR:addressbook-query xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"/>").await.status, 403);
    assert_eq!(w.a(Verb::Report, CAL, "no").await.status, 400);
    assert_eq!(w.a(Verb::Report, "/dav/calendars/alice%40one.test/nothing/", &query("20240101T000000Z", "20240401T000000Z", "")).await.status, 404);
}

#[tokio::test]
async fn contacts_are_put_and_found_by_name_and_address() {
    let w = world().await;
    assert_eq!(w.a(Verb::Put, &format!("{BOOK}ada.vcf"), &card("ada", "Ada Lovelace", "ada@example.org")).await.status, 201);
    assert_eq!(w.a(Verb::Put, &format!("{BOOK}alan.vcf"), &card("alan", "Alan Turing", "alan@example.org")).await.status, 201);
    // A card with no UID takes its name for one.
    assert_eq!(w.a(Verb::Put, &format!("{BOOK}grace.vcf"), "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Grace Hopper\r\nEND:VCARD\r\n").await.status, 201);
    let bad = w.a(Verb::Put, &format!("{BOOK}nameless.vcf"), "BEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n").await;
    assert_eq!(bad.status, 403);
    assert!(bad.body.contains("valid-address-data"));
    assert_eq!(w.a(Verb::Put, &format!("{BOOK}cal.ics"), &event("e", "x", "20240101T100000Z", "20240101T110000Z")).await.status, 403, "an event is not a contact");
    let q = |filter: &str, extra: &str| {
        format!("<CR:addressbook-query xmlns:D=\"DAV:\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><D:prop><D:getetag/><CR:address-data/></D:prop><CR:filter{extra}>{filter}</CR:filter></CR:addressbook-query>")
    };
    let by_mail = q("<CR:prop-filter name=\"EMAIL\"><CR:text-match match-type=\"ends-with\">@example.org</CR:text-match></CR:prop-filter>", "");
    let r = w.a(Verb::Report, BOOK, &by_mail).await;
    assert!(r.body.contains("ada.vcf") && r.body.contains("alan.vcf") && !r.body.contains("grace.vcf"), "{}", r.body);
    assert!(r.body.contains("FN:Ada Lovelace"));
    let by_name = q("<CR:prop-filter name=\"FN\"><CR:text-match>TURING</CR:text-match></CR:prop-filter>", "");
    let r = w.a(Verb::Report, BOOK, &by_name).await;
    assert!(r.body.contains("alan.vcf") && !r.body.contains("ada.vcf"));
    let both = q("<CR:prop-filter name=\"FN\"><CR:text-match>ada</CR:text-match></CR:prop-filter><CR:prop-filter name=\"EMAIL\"><CR:text-match>turing</CR:text-match></CR:prop-filter>", " test=\"allof\"");
    assert!(!w.a(Verb::Report, BOOK, &both).await.body.contains(".vcf"));
    let either = q("<CR:prop-filter name=\"FN\"><CR:text-match>ada</CR:text-match></CR:prop-filter><CR:prop-filter name=\"FN\"><CR:text-match>grace</CR:text-match></CR:prop-filter>", "");
    let r = w.a(Verb::Report, BOOK, &either).await;
    assert!(r.body.contains("ada.vcf") && r.body.contains("grace.vcf") && !r.body.contains("alan.vcf"));
    let limited = "<CR:addressbook-query xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><CR:filter/><CR:limit><CR:nresults>2</CR:nresults></CR:limit></CR:addressbook-query>";
    assert_eq!(w.a(Verb::Report, BOOK, &limited).await.body.matches("<D:response>").count(), 2);
}

#[tokio::test]
async fn the_sync_report_gives_what_changed_since_a_token_and_refuses_a_token_that_is_not_the_collections() {
    let w = world().await;
    let sync = |token: &str| {
        format!("<D:sync-collection xmlns:D=\"DAV:\"><D:sync-token>{token}</D:sync-token><D:sync-level>1</D:sync-level><D:prop><D:getetag/></D:prop></D:sync-collection>")
    };
    let token_of = |r: &Reply| r.body.split("<D:sync-token>").nth(1).unwrap().split("</D:sync-token>").next().unwrap().to_owned();
    for n in ["a", "b"] {
        w.a(Verb::Put, &format!("{CAL}{n}.ics"), &event(n, n, "20240101T100000Z", "20240101T110000Z")).await;
    }
    let first = w.a(Verb::Report, CAL, &sync("")).await;
    assert_eq!(first.status, 207);
    assert!(first.body.contains("a.ics") && first.body.contains("b.ics"));
    let t1 = token_of(&first);
    assert!(t1.starts_with("urn:rusty:sync:") && t1.ends_with(":2"), "{t1}");
    // Nothing changed: no response, the same token.
    let none = w.a(Verb::Report, CAL, &sync(&t1)).await;
    assert!(!none.body.contains("<D:response>") && token_of(&none) == t1, "{}", none.body);
    // A change and a removal.
    w.a(Verb::Put, &format!("{CAL}a.ics"), &event("a", "changed", "20240101T100000Z", "20240101T110000Z")).await;
    w.a(Verb::Delete, &format!("{CAL}b.ics"), "").await;
    let r = w.a(Verb::Report, CAL, &sync(&t1)).await;
    assert!(r.body.contains("a.ics</D:href><D:propstat>"), "{}", r.body);
    assert!(r.body.contains("b.ics</D:href><D:status>HTTP/1.1 404 Not Found"), "{}", r.body);
    assert!(token_of(&r).ends_with(":4"));
    // The ctag moves with the collection.
    let p = w.call(&w.alice, Verb::Propfind, CAL, &propfind("<CS:getctag/>"), R { depth: Some("0"), ..R::default() }).await;
    assert!(p.body.contains("<CS:getctag>4</CS:getctag>"), "{}", p.body);
    // A token of another collection, or one that is not a token.
    let other = w.a(Verb::Report, BOOK, &sync(&t1)).await;
    assert_eq!(other.status, 403);
    assert!(other.body.contains("valid-sync-token"));
    assert_eq!(w.a(Verb::Report, CAL, &sync("http://example.org/token")).await.status, 403);
    assert_eq!(w.a(Verb::Report, CAL, &sync("urn:rusty:sync:1:-5")).await.status, 403);
}

#[tokio::test]
async fn collections_are_made_changed_and_removed() {
    let w = world().await;
    let work = "/dav/calendars/alice%40one.test/work/";
    let mk = "<C:mkcalendar xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:A=\"http://apple.com/ns/ical/\"><D:set><D:prop><D:displayname>Work &amp; more</D:displayname><C:calendar-description>Office</C:calendar-description><A:calendar-color>#336699</A:calendar-color></D:prop></D:set></C:mkcalendar>";
    let r = w.a(Verb::Mkcalendar, work, mk).await;
    assert_eq!(r.status, 201);
    assert_eq!(header(&r, "location"), Some(work));
    assert_eq!(w.a(Verb::Mkcalendar, work, mk).await.status, 405, "it is there");
    let p = w.a(Verb::Propfind, work, &propfind("<D:displayname/><C:calendar-description/><C:supported-calendar-component-set/>")).await;
    assert!(p.body.contains("<D:displayname>Work &amp; more</D:displayname>") && p.body.contains("<C:calendar-description>Office</C:calendar-description>"), "{}", p.body);
    let color = w.call(&w.alice, Verb::Propfind, work, "<D:propfind xmlns:D=\"DAV:\" xmlns:A=\"http://apple.com/ns/ical/\"><D:prop><A:calendar-color/></D:prop></D:propfind>", R { depth: Some("0"), ..R::default() }).await;
    assert!(color.body.contains("<A:calendar-color>#336699</A:calendar-color>"), "{}", color.body);
    // PROPPATCH: what can change does; what cannot is a 403 of its own; a colour that is not one is a 400.
    let patch = |inner: &str| format!("<D:propertyupdate xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:X=\"urn:x\" xmlns:A=\"http://apple.com/ns/ical/\">{inner}</D:propertyupdate>");
    let r = w.a(Verb::Proppatch, work, &patch("<D:set><D:prop><D:displayname>Job</D:displayname><X:flavour>mint</X:flavour></D:prop></D:set>")).await;
    assert_eq!(r.status, 207);
    assert!(r.body.contains("200 OK") && r.body.contains("403 Forbidden") && r.body.contains("flavour"), "{}", r.body);
    assert_eq!(w.a(Verb::Proppatch, work, &patch("<D:set><D:prop><A:calendar-color>red</A:calendar-color></D:prop></D:set>")).await.status, 400);
    assert!(w.a(Verb::Propfind, work, "").await.body.contains("Job"));
    // MKCOL makes an address book when it says so, and only where the path is one.
    let book = "/dav/addressbooks/alice%40one.test/friends/";
    let mkcol = "<D:mkcol xmlns:D=\"DAV:\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><D:set><D:prop><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype><D:displayname>Friends</D:displayname></D:prop></D:set></D:mkcol>";
    assert_eq!(w.a(Verb::Mkcol, book, mkcol).await.status, 201);
    assert_eq!(w.a(Verb::Mkcol, "/dav/calendars/alice%40one.test/x/", mkcol).await.status, 403, "an address book under /calendars/");
    assert_eq!(w.a(Verb::Mkcol, "/dav/calendars/alice%40one.test/y/", "").await.status, 403, "a plain collection is not offered");
    assert_eq!(w.a(Verb::Mkcalendar, "/dav/calendars/alice%40one.test/bad%20name/", "").await.status, 404);
    assert_eq!(w.a(Verb::Mkcalendar, "/dav/calendars/bob%40one.test/work/", "").await.status, 403);
    // Removal takes the objects with it; the home, the root and the principal are not removable.
    w.a(Verb::Put, &format!("{work}x.ics"), &event("x", "x", "20240101T100000Z", "20240101T110000Z")).await;
    assert_eq!(w.a(Verb::Delete, work, "").await.status, 204);
    assert_eq!(w.a(Verb::Get, &format!("{work}x.ics"), "").await.status, 404);
    assert_eq!(w.a(Verb::Delete, work, "").await.status, 404);
    for p in ["/dav/", "/dav/principals/alice%40one.test/", "/dav/calendars/alice%40one.test/"] {
        assert_eq!(w.a(Verb::Delete, p, "").await.status, 403, "{p}");
    }
    // A method a resource does not have says what it has.
    let r = w.a(Verb::Get, CAL, "").await;
    assert_eq!(r.status, 405);
    assert!(header(&r, "allow").unwrap().contains("REPORT"));
    assert_eq!(w.a(Verb::Put, CAL, "x").await.status, 405);
}

#[tokio::test]
async fn an_object_is_copied_and_moved_within_the_account_and_never_into_another() {
    let w = world().await;
    w.a(Verb::Mkcalendar, "/dav/calendars/alice%40one.test/second/", "").await;
    let src = format!("{CAL}e.ics");
    w.a(Verb::Put, &src, &event("e", "to move", "20240101T100000Z", "20240101T110000Z")).await;
    let dest = "/dav/calendars/alice%40one.test/second/e.ics";
    let copy = w.call(&w.alice, Verb::Copy, &src, "", R { destination: Some(&format!("https://mail.one.test{dest}")), ..R::default() }).await;
    assert_eq!(copy.status, 201);
    assert_eq!(w.a(Verb::Get, &src, "").await.status, 200, "a copy leaves the source");
    assert_eq!(w.call(&w.alice, Verb::Copy, &src, "", R { destination: Some(dest), overwrite: Some("F"), ..R::default() }).await.status, 412, "Overwrite: F on a name that is taken");
    let moved = w.call(&w.alice, Verb::Move, &src, "", R { destination: Some(dest), ..R::default() }).await;
    assert_eq!(moved.status, 204);
    assert_eq!(w.a(Verb::Get, &src, "").await.status, 404, "a move takes the source");
    assert!(w.a(Verb::Get, dest, "").await.body.contains("to move"));
    // The same collection under the same UID is a conflict; another account's tree and a path that is no object are refused.
    w.a(Verb::Put, &src, &event("e", "again", "20240101T100000Z", "20240101T110000Z")).await;
    let twice = w.call(&w.alice, Verb::Copy, &src, "", R { destination: Some(&format!("{CAL}e2.ics")), ..R::default() }).await;
    assert_eq!(twice.status, 409);
    assert_eq!(w.call(&w.alice, Verb::Copy, &src, "", R { destination: Some("/dav/calendars/bob%40one.test/default/e.ics"), ..R::default() }).await.status, 403);
    assert_eq!(w.call(&w.alice, Verb::Copy, &src, "", R { destination: Some("/dav/calendars/alice%40one.test/default/"), ..R::default() }).await.status, 403);
    assert_eq!(w.call(&w.alice, Verb::Copy, &src, "", R { destination: Some(&src), ..R::default() }).await.status, 403, "onto itself");
    assert_eq!(w.call(&w.alice, Verb::Copy, &src, "", R::default()).await.status, 400, "no Destination");
    // Bob has a tree of his own with the same names and none of this.
    assert_eq!(w.call(&w.bob, Verb::Get, "/dav/calendars/bob%40one.test/default/e.ics", "", R::default()).await.status, 404);
}

#[tokio::test]
async fn the_date_headers_are_http_dates() {
    use rusty_dav::server::http_date;
    assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    assert_eq!(http_date(1_795_499_452), "Tue, 24 Nov 2026 05:50:52 GMT");
    assert_eq!(http_date(951_825_600), "Tue, 29 Feb 2000 12:00:00 GMT");
}
