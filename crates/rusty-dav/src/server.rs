//! The methods and reports of WebDAV, CalDAV and CardDAV over a repository (docs/design-caldav-carddav.md). It knows nothing of sockets, TLS or passwords: the caller has signed the person in and hands over the
//! account, the method, the path and the few headers that matter; the answer is a status, headers and a body. Every path is checked, every body is bounded, and an account reaches its own tree only.

use std::sync::Arc;

use rusty_core::{AccountId, Error};
use rusty_store::{DavCollection, DavDeleteResult, DavObject, DavPrecondition, DavPutResult, DavRepo, NewDavObject, MAX_OBJECTS_PER_COLLECTION, MAX_OBJECT_BYTES};

use crate::content;
use crate::paths::{self, collection_href, home_href, object_href, principal_href, Kind, Resource};
use crate::time::parse_moment;
use crate::xml::{self, escape, Element};
use crate::{ical, vcard};

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub const CS: &str = "http://calendarserver.org/ns/";
pub const APPLE: &str = "http://apple.com/ns/ical/";

/// How many objects a report returns at most.
pub const REPORT_LIMIT: i64 = 1000;
const OBJECT_LIMIT: i64 = MAX_OBJECTS_PER_COLLECTION;
/// An answer stops growing here (the rest is reported as not returned): a collection of large objects must not make one answer of a gigabyte.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// The entry that says the list was cut at the limit (RFC 4791 section 7.1, RFC 6578 section 3.6).
fn truncated(collection_href: &str) -> String {
    format!("<D:response>{}<D:status>HTTP/1.1 507 Insufficient Storage</D:status><D:error><D:number-of-matches-within-limits/></D:error></D:response>", href(collection_href))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Options,
    Propfind,
    Proppatch,
    Report,
    Mkcol,
    Mkcalendar,
    Get,
    Put,
    Delete,
    Copy,
    Move,
}

/// The person who signed in.
#[derive(Debug, Clone)]
pub struct Principal {
    pub account: AccountId,
    pub address: String,
}

/// What the service reads of a request.
#[derive(Debug, Clone, Copy)]
pub struct Req<'a> {
    pub verb: Verb,
    /// The path as sent (escapes still in it).
    pub path: &'a str,
    pub depth: Option<&'a str>,
    pub if_match: Option<&'a str>,
    pub if_none_match: Option<&'a str>,
    pub destination: Option<&'a str>,
    pub overwrite: Option<&'a str>,
    pub body: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub content_type: &'static str,
    pub body: String,
}

impl Reply {
    fn new(status: u16) -> Reply {
        Reply { status, headers: Vec::new(), content_type: "application/xml; charset=utf-8", body: String::new() }
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Reply {
        self.headers.push((name, value.into()));
        self
    }

    fn text(status: u16, text: &str) -> Reply {
        Reply { status, headers: Vec::new(), content_type: "text/plain; charset=utf-8", body: format!("{text}\n") }
    }

    fn xml(status: u16, body: String) -> Reply {
        Reply { status, headers: Vec::new(), content_type: "application/xml; charset=utf-8", body }
    }

    /// `<D:error>` with one condition (RFC 4918 section 16): `(namespace, name, inner XML)`.
    fn error(status: u16, ns: &str, name: &str, inner: &str) -> Reply {
        let (open, close) = tags(ns, name);
        Reply::xml(status, format!("{XML_HEAD}<D:error {NAMESPACES}>{open}{inner}{close}</D:error>"))
    }

    fn storage(e: &Error) -> Reply {
        match e {
            Error::Busy(_) => Reply::text(503, "the store is busy: try again").header("retry-after", "5"),
            _ => Reply::text(500, "the store failed"),
        }
    }
}

const XML_HEAD: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";
const NAMESPACES: &str = "xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\" xmlns:CS=\"http://calendarserver.org/ns/\" xmlns:A=\"http://apple.com/ns/ical/\"";

/// The open and the close tag of an element of a namespace (a namespace without a prefix of ours is declared where it is used).
fn tags(ns: &str, name: &str) -> (String, String) {
    let prefix = match ns {
        DAV => Some("D"),
        CALDAV => Some("C"),
        CARDDAV => Some("CR"),
        CS => Some("CS"),
        APPLE => Some("A"),
        _ => None,
    };
    match prefix {
        Some(p) => (format!("<{p}:{name}>"), format!("</{p}:{name}>")),
        None => (format!("<{} xmlns=\"{}\">", safe_name(name), escape(ns)), format!("</{}>", safe_name(name))),
    }
}

/// A name taken from a request goes back into an answer only when it is a plain name.
fn safe_name(n: &str) -> String {
    n.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect()
}

fn empty_tag(ns: &str, name: &str) -> String {
    let (open, _) = tags(ns, name);
    match open.strip_suffix('>') {
        Some(o) => format!("{o}/>"),
        None => open,
    }
}

fn wrap(ns: &str, name: &str, inner: &str) -> String {
    let (o, c) = tags(ns, name);
    format!("{o}{inner}{c}")
}

fn href(h: &str) -> String {
    wrap(DAV, "href", &escape(h))
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `(year, month, day, hour, minute, second, weekday 0 = Sunday)` of seconds since the epoch.
fn civil(secs: i64) -> (i64, usize, i64, i64, i64, i64, usize) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let wd = usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0);
    (y, usize::try_from(m - 1).unwrap_or(0), d, rem / 3600, rem % 3600 / 60, rem % 60, wd)
}

pub fn http_date(secs: i64) -> String {
    let (y, m, d, h, mi, s, wd) = civil(secs);
    format!("{}, {d:02} {} {y} {h:02}:{mi:02}:{s:02} GMT", WEEKDAYS.get(wd).copied().unwrap_or("Mon"), MONTHS.get(m).copied().unwrap_or("Jan"))
}

fn iso_date(secs: i64) -> String {
    let (y, m, d, h, mi, s, _) = civil(secs);
    format!("{y:04}-{:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z", m + 1)
}

fn quoted_etag(etag: &str) -> String {
    format!("\"{etag}\"")
}

/// The tag of an `If-Match` or `If-None-Match` header value: one tag, quotes and a weak prefix taken off.
fn bare_etag(v: &str) -> String {
    v.trim().trim_start_matches("W/").trim_matches('"').to_owned()
}

/// What a request sets on a collection, and the properties it names that cannot be changed.
struct Updates {
    display: Option<String>,
    description: Option<String>,
    color: Option<String>,
    refused: Vec<(String, String)>,
}

pub struct Dav {
    repo: Arc<dyn DavRepo>,
}

/// What a PROPFIND or a report asks for.
enum Wanted {
    All,
    Names,
    Props(Vec<(String, String)>),
}

enum Target<'a> {
    Root,
    Principal,
    Home(Kind),
    Collection(&'a DavCollection),
    Object(Kind, &'a DavObject),
}

impl Dav {
    pub fn new(repo: Arc<dyn DavRepo>) -> Dav {
        Dav { repo }
    }

    pub async fn handle(&self, who: &Principal, req: &Req<'_>) -> Reply {
        let Some(res) = paths::parse(req.path) else { return Reply::text(404, "there is nothing here") };
        // A person reaches their own tree only.
        let owner = match &res {
            Resource::Principal(a) | Resource::Home(_, a) | Resource::Collection(_, a, _) | Resource::Object(_, a, _, _) => Some(a.as_str()),
            Resource::Root => None,
        };
        if owner.is_some_and(|a| !a.eq_ignore_ascii_case(&who.address)) {
            return Reply::error(403, DAV, "need-privileges", "").header("x-reason", "not-your-tree");
        }
        if req.verb == Verb::Options {
            return Self::options(&res);
        }
        if let Err(e) = self.repo.ensure_defaults(who.account).await {
            return Reply::storage(&e);
        }
        match (req.verb, &res) {
            (Verb::Propfind, _) => self.propfind(who, req, &res).await,
            (Verb::Proppatch, Resource::Collection(kind, _, name)) => self.proppatch(who, req, *kind, name).await,
            (Verb::Report, Resource::Collection(kind, _, name)) => self.report(who, req, *kind, name).await,
            (Verb::Mkcalendar, Resource::Collection(kind, _, name)) if *kind == Kind::Calendar => self.make_collection(who, req, *kind, name, true).await,
            (Verb::Mkcol, Resource::Collection(kind, _, name)) => self.make_collection(who, req, *kind, name, false).await,
            (Verb::Get, Resource::Object(kind, _, c, o)) => self.get(who, *kind, c, o).await,
            (Verb::Put, Resource::Object(kind, _, c, o)) => self.put(who, req, *kind, c, o).await,
            (Verb::Delete, Resource::Object(kind, _, c, o)) => self.delete_object(who, req, *kind, c, o).await,
            (Verb::Delete, Resource::Collection(kind, _, name)) => match self.repo.delete_collection(who.account, kind.as_str(), name).await {
                Ok(true) => Reply::new(204),
                Ok(false) => Reply::text(404, "there is no such collection"),
                Err(e) => Reply::storage(&e),
            },
            (Verb::Copy | Verb::Move, Resource::Object(kind, _, c, o)) => self.copy_move(who, req, *kind, c, o).await,
            (Verb::Delete | Verb::Copy | Verb::Move | Verb::Mkcol | Verb::Mkcalendar, _) => Reply::error(403, DAV, "forbidden", "").header("x-reason", "protected"),
            _ => Reply::text(405, "the method is not allowed here").header("allow", allow_of(&res)),
        }
    }

    /// The answer to `OPTIONS` for a path: it does not depend on who asks (clients ask what is there before they send a password).
    pub fn options_of(path: &str) -> Reply {
        match paths::parse(path) {
            Some(res) => Self::options(&res),
            None => Reply::text(404, "there is nothing here"),
        }
    }

    fn options(res: &Resource) -> Reply {
        let mut r = Reply::new(200).header("dav", "1, 3, calendar-access, addressbook").header("allow", allow_of(res));
        r.content_type = "text/plain; charset=utf-8";
        r
    }

    async fn collection(&self, who: &Principal, kind: Kind, name: &str) -> Result<Option<DavCollection>, Reply> {
        self.repo.collection(who.account, kind.as_str(), name).await.map_err(|e| Reply::storage(&e))
    }

    // ---------------------------------------------------------------------------------------- properties

    /// The value (inner XML) of one property of a target, `None` when the target has no such property.
    fn prop(&self, who: &Principal, t: &Target<'_>, ns: &str, name: &str, with_data: bool) -> Option<String> {
        let a = &who.address;
        match (ns, name, t) {
            (DAV, "resourcetype", Target::Root | Target::Home(_)) => Some(empty_tag(DAV, "collection")),
            (DAV, "resourcetype", Target::Principal) => Some(format!("{}{}", empty_tag(DAV, "collection"), empty_tag(DAV, "principal"))),
            (DAV, "resourcetype", Target::Collection(c)) => Some(format!("{}{}", empty_tag(DAV, "collection"), if c.kind == "calendar" { empty_tag(CALDAV, "calendar") } else { empty_tag(CARDDAV, "addressbook") })),
            (DAV, "resourcetype", Target::Object(..)) => Some(String::new()),
            (DAV, "displayname", Target::Root) => Some("Rusty".to_owned()),
            (DAV, "displayname", Target::Principal) => Some(escape(a)),
            (DAV, "displayname", Target::Home(k)) => Some(if *k == Kind::Calendar { "Calendars" } else { "Contacts" }.to_owned()),
            (DAV, "displayname", Target::Collection(c)) => Some(escape(if c.display_name.is_empty() { &c.name } else { &c.display_name })),
            (DAV, "displayname", Target::Object(_, o)) => Some(escape(&o.name)),
            (DAV, "current-user-principal" | "principal-URL" | "owner", Target::Root | Target::Principal | Target::Home(_) | Target::Collection(_)) => Some(href(&principal_href(a))),
            (DAV, "principal-collection-set", Target::Root) => Some(href("/dav/principals/")),
            (CALDAV, "calendar-home-set", Target::Principal) => Some(href(&home_href(Kind::Calendar, a))),
            (CARDDAV, "addressbook-home-set", Target::Principal) => Some(href(&home_href(Kind::AddressBook, a))),
            (CALDAV, "calendar-user-address-set", Target::Principal) => Some(href(&format!("mailto:{a}"))),
            (DAV, "current-user-privilege-set", Target::Home(_) | Target::Collection(_) | Target::Object(..)) => Some(
                ["read", "write", "write-properties", "write-content", "bind", "unbind", "read-current-user-privilege-set"].iter().map(|p| wrap(DAV, "privilege", &empty_tag(DAV, p))).collect::<String>(),
            ),
            (DAV, "supported-report-set", Target::Collection(c)) => {
                let reports: &[(&str, &str)] = if c.kind == "calendar" { &[(CALDAV, "calendar-query"), (CALDAV, "calendar-multiget")] } else { &[(CARDDAV, "addressbook-query"), (CARDDAV, "addressbook-multiget")] };
                let mut out: String = reports.iter().map(|(n, r)| wrap(DAV, "supported-report", &wrap(DAV, "report", &empty_tag(n, r)))).collect();
                out.push_str(&wrap(DAV, "supported-report", &wrap(DAV, "report", &empty_tag(DAV, "sync-collection"))));
                Some(out)
            }
            (CS, "getctag", Target::Collection(c)) => Some(format!("{}", c.revision)),
            (DAV, "sync-token", Target::Collection(c)) => Some(format!("urn:rusty:sync:{}:{}", c.id, c.revision)),
            (DAV, "creationdate", Target::Collection(c)) => Some(iso_date(c.created_at)),
            (DAV, "getlastmodified", Target::Collection(c)) => Some(http_date(c.updated_at)),
            (CALDAV, "supported-calendar-component-set", Target::Collection(c)) if c.kind == "calendar" => Some(["VEVENT", "VTODO", "VJOURNAL"].iter().map(|n| format!("<C:comp name=\"{n}\"/>")).collect()),
            (CALDAV, "supported-calendar-data", Target::Collection(c)) if c.kind == "calendar" => Some("<C:calendar-data content-type=\"text/calendar\" version=\"2.0\"/>".to_owned()),
            (CARDDAV, "supported-address-data", Target::Collection(c)) if c.kind == "addressbook" => Some("<CR:address-data-type content-type=\"text/vcard\" version=\"3.0\"/><CR:address-data-type content-type=\"text/vcard\" version=\"4.0\"/>".to_owned()),
            (CALDAV, "max-resource-size", Target::Collection(c)) if c.kind == "calendar" => Some(MAX_OBJECT_BYTES.to_string()),
            (CARDDAV, "max-resource-size", Target::Collection(c)) if c.kind == "addressbook" => Some(MAX_OBJECT_BYTES.to_string()),
            (CALDAV, "calendar-description", Target::Collection(c)) if c.kind == "calendar" => Some(escape(&c.description)),
            (CARDDAV, "addressbook-description", Target::Collection(c)) if c.kind == "addressbook" => Some(escape(&c.description)),
            (APPLE, "calendar-color", Target::Collection(c)) if c.kind == "calendar" && !c.color.is_empty() => Some(escape(&c.color)),
            (DAV, "getetag", Target::Object(_, o)) => Some(escape(&quoted_etag(&o.etag))),
            (DAV, "getcontenttype", Target::Object(k, o)) => Some(match k {
                Kind::Calendar => format!("text/calendar; charset=utf-8; component={}", o.component),
                Kind::AddressBook => "text/vcard; charset=utf-8".to_owned(),
            }),
            (DAV, "getcontentlength", Target::Object(_, o)) => Some(o.size.to_string()),
            (DAV, "getlastmodified", Target::Object(_, o)) => Some(http_date(o.updated_at)),
            (CALDAV, "calendar-data", Target::Object(Kind::Calendar, o)) if with_data => Some(escape(&o.body)),
            (CARDDAV, "address-data", Target::Object(Kind::AddressBook, o)) if with_data => Some(escape(&o.body)),
            _ => None,
        }
    }

    /// The properties `allprop` and `propname` list for a target.
    fn all_names(t: &Target<'_>) -> Vec<(&'static str, &'static str)> {
        match t {
            Target::Root => vec![(DAV, "resourcetype"), (DAV, "displayname"), (DAV, "current-user-principal"), (DAV, "principal-collection-set")],
            Target::Principal => vec![(DAV, "resourcetype"), (DAV, "displayname"), (DAV, "principal-URL"), (DAV, "current-user-principal"), (CALDAV, "calendar-home-set"), (CARDDAV, "addressbook-home-set"), (CALDAV, "calendar-user-address-set"), (DAV, "owner")],
            Target::Home(_) => vec![(DAV, "resourcetype"), (DAV, "displayname"), (DAV, "owner"), (DAV, "current-user-privilege-set")],
            Target::Collection(c) => {
                let mut v = vec![(DAV, "resourcetype"), (DAV, "displayname"), (CS, "getctag"), (DAV, "sync-token"), (DAV, "supported-report-set"), (DAV, "owner"), (DAV, "current-user-privilege-set"), (DAV, "creationdate"), (DAV, "getlastmodified")];
                if c.kind == "calendar" {
                    v.extend([(CALDAV, "supported-calendar-component-set"), (CALDAV, "supported-calendar-data"), (CALDAV, "max-resource-size"), (CALDAV, "calendar-description"), (APPLE, "calendar-color")]);
                } else {
                    v.extend([(CARDDAV, "supported-address-data"), (CARDDAV, "max-resource-size"), (CARDDAV, "addressbook-description")]);
                }
                v
            }
            Target::Object(..) => vec![(DAV, "resourcetype"), (DAV, "getetag"), (DAV, "getcontenttype"), (DAV, "getcontentlength"), (DAV, "getlastmodified"), (DAV, "current-user-privilege-set")],
        }
    }

    /// One `<D:response>` for a target with the properties asked for.
    fn response(&self, who: &Principal, h: &str, t: &Target<'_>, wanted: &Wanted, with_data: bool) -> String {
        let mut found = String::new();
        let mut missing = String::new();
        match wanted {
            Wanted::All => {
                for (ns, n) in Self::all_names(t) {
                    if let Some(v) = self.prop(who, t, ns, n, false) {
                        found.push_str(&wrap(ns, n, &v));
                    }
                }
            }
            Wanted::Names => {
                for (ns, n) in Self::all_names(t) {
                    found.push_str(&empty_tag(ns, n));
                }
            }
            Wanted::Props(list) => {
                for (ns, n) in list {
                    match self.prop(who, t, ns, n, with_data) {
                        Some(v) if v.is_empty() => found.push_str(&empty_tag(ns, n)),
                        Some(v) => found.push_str(&wrap(ns, n, &v)),
                        None => missing.push_str(&empty_tag(ns, n)),
                    }
                }
            }
        }
        let mut out = format!("<D:response>{}", href(h));
        if !found.is_empty() || missing.is_empty() {
            out.push_str(&format!("<D:propstat><D:prop>{found}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"));
        }
        if !missing.is_empty() {
            out.push_str(&format!("<D:propstat><D:prop>{missing}</D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"));
        }
        out.push_str("</D:response>");
        out
    }

    fn multistatus(inner: &str) -> Reply {
        Reply::xml(207, format!("{XML_HEAD}<D:multistatus {NAMESPACES}>{inner}</D:multistatus>"))
    }

    fn wanted(prop_parent: Option<&Element>) -> Wanted {
        match prop_parent {
            None => Wanted::All,
            Some(p) => Wanted::Props(p.elements().map(|e| (e.ns.clone(), e.name.clone())).collect()),
        }
    }

    // ---------------------------------------------------------------------------------------- PROPFIND

    async fn propfind(&self, who: &Principal, req: &Req<'_>, res: &Resource) -> Reply {
        let depth = match req.depth.map(str::trim) {
            Some("0") => 0,
            Some("1") => 1,
            _ => return Reply::error(403, DAV, "propfind-finite-depth", ""),
        };
        let wanted = if req.body.iter().all(u8::is_ascii_whitespace) {
            Wanted::All
        } else {
            let doc = match xml::parse(req.body) {
                Ok(d) => d,
                Err(e) => return Reply::text(400, e),
            };
            if !doc.is(DAV, "propfind") {
                return Reply::text(400, "a propfind is expected");
            }
            if doc.child(DAV, "propname").is_some() {
                Wanted::Names
            } else if let Some(p) = doc.child(DAV, "prop") {
                Self::wanted(Some(p))
            } else {
                Wanted::All
            }
        };
        let mut out = String::new();
        let a = &who.address;
        match res {
            Resource::Root => {
                out.push_str(&self.response(who, "/dav/", &Target::Root, &wanted, true));
                if depth == 1 {
                    out.push_str(&self.response(who, &principal_href(a), &Target::Principal, &wanted, true));
                    for k in [Kind::Calendar, Kind::AddressBook] {
                        out.push_str(&self.response(who, &home_href(k, a), &Target::Home(k), &wanted, true));
                    }
                }
            }
            Resource::Principal(_) => out.push_str(&self.response(who, &principal_href(a), &Target::Principal, &wanted, true)),
            Resource::Home(kind, _) => {
                out.push_str(&self.response(who, &home_href(*kind, a), &Target::Home(*kind), &wanted, true));
                if depth == 1 {
                    match self.repo.collections(who.account, kind.as_str()).await {
                        Ok(list) => {
                            for c in &list {
                                out.push_str(&self.response(who, &collection_href(*kind, a, &c.name), &Target::Collection(c), &wanted, true));
                            }
                        }
                        Err(e) => return Reply::storage(&e),
                    }
                }
            }
            Resource::Collection(kind, _, name) => {
                let c = match self.collection(who, *kind, name).await {
                    Ok(Some(c)) => c,
                    Ok(None) => return Reply::text(404, "there is no such collection"),
                    Err(r) => return r,
                };
                out.push_str(&self.response(who, &collection_href(*kind, a, name), &Target::Collection(&c), &wanted, true));
                if depth == 1 {
                    match self.repo.objects(c.id, OBJECT_LIMIT).await {
                        Ok(list) => {
                            for o in &list {
                                if out.len() > MAX_RESPONSE_BYTES {
                                    out.push_str(&truncated(&collection_href(*kind, a, name)));
                                    break;
                                }
                                out.push_str(&self.response(who, &object_href(*kind, a, name, &o.name), &Target::Object(*kind, o), &wanted, true));
                            }
                        }
                        Err(e) => return Reply::storage(&e),
                    }
                }
            }
            Resource::Object(kind, _, cname, oname) => {
                let c = match self.collection(who, *kind, cname).await {
                    Ok(Some(c)) => c,
                    Ok(None) => return Reply::text(404, "there is no such collection"),
                    Err(r) => return r,
                };
                match self.repo.object(c.id, oname).await {
                    Ok(Some(o)) => out.push_str(&self.response(who, &object_href(*kind, a, cname, oname), &Target::Object(*kind, &o), &wanted, true)),
                    Ok(None) => return Reply::text(404, "there is no such object"),
                    Err(e) => return Reply::storage(&e),
                }
            }
        }
        Self::multistatus(&out)
    }

    // ---------------------------------------------------------------------------------------- PROPPATCH, MKCOL, MKCALENDAR

    /// The properties a request sets or removes on a collection: `(display, description, color, the ones that cannot be changed)`.
    fn updates(doc: &Element) -> Updates {
        let (mut display, mut description, mut color, mut refused) = (None, None, None, Vec::new());
        for action in doc.elements() {
            let removing = action.is(DAV, "remove");
            if !(action.is(DAV, "set") || removing) {
                continue;
            }
            for prop in action.child(DAV, "prop").into_iter().flat_map(Element::elements) {
                let value = if removing { String::new() } else { prop.text() };
                match (prop.ns.as_str(), prop.name.as_str()) {
                    (DAV, "displayname") => display = Some(value),
                    (CALDAV, "calendar-description") | (CARDDAV, "addressbook-description") => description = Some(value),
                    (APPLE, "calendar-color") => color = Some(value.trim().to_owned()),
                    (ns, n) => refused.push((ns.to_owned(), n.to_owned())),
                }
            }
        }
        Updates { display, description, color, refused }
    }

    fn valid_fields(display: Option<&str>, description: Option<&str>, color: Option<&str>) -> bool {
        display.is_none_or(|d| d.chars().count() <= 256 && !d.chars().any(char::is_control))
            && description.is_none_or(|d| d.chars().count() <= 2048)
            && color.is_none_or(|c| {
                c.is_empty() || (c.starts_with('#') && matches!(c.len(), 7 | 9) && c.bytes().skip(1).all(|b| b.is_ascii_hexdigit()))
            })
    }

    async fn proppatch(&self, who: &Principal, req: &Req<'_>, kind: Kind, name: &str) -> Reply {
        let doc = match xml::parse(req.body) {
            Ok(d) if d.is(DAV, "propertyupdate") => d,
            Ok(_) => return Reply::text(400, "a propertyupdate is expected"),
            Err(e) => return Reply::text(400, e),
        };
        let Updates { display, description, color, refused } = Self::updates(&doc);
        let h = collection_href(kind, &who.address, name);
        let mut set = String::new();
        let mut denied = String::new();
        for (ns, n) in &refused {
            denied.push_str(&empty_tag(ns, n));
        }
        let ok = Self::valid_fields(display.as_deref(), description.as_deref(), color.as_deref());
        if !ok {
            return Reply::text(400, "a value is not valid");
        }
        match self.repo.update_collection(who.account, kind.as_str(), name, display.as_deref(), description.as_deref(), color.as_deref()).await {
            Ok(true) => {}
            Ok(false) => return Reply::text(404, "there is no such collection"),
            Err(Error::Invalid(_)) => return Reply::text(400, "a value is not valid"),
            Err(e) => return Reply::storage(&e),
        }
        if display.is_some() {
            set.push_str(&empty_tag(DAV, "displayname"));
        }
        if description.is_some() {
            set.push_str(&empty_tag(if kind == Kind::Calendar { CALDAV } else { CARDDAV }, if kind == Kind::Calendar { "calendar-description" } else { "addressbook-description" }));
        }
        if color.is_some() {
            set.push_str(&empty_tag(APPLE, "calendar-color"));
        }
        let mut out = format!("<D:response>{}", href(&h));
        if !set.is_empty() {
            out.push_str(&format!("<D:propstat><D:prop>{set}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"));
        }
        if !denied.is_empty() {
            out.push_str(&format!("<D:propstat><D:prop>{denied}</D:prop><D:status>HTTP/1.1 403 Forbidden</D:status></D:propstat>"));
        }
        out.push_str("</D:response>");
        Self::multistatus(&out)
    }

    async fn make_collection(&self, who: &Principal, req: &Req<'_>, kind: Kind, name: &str, calendar_method: bool) -> Reply {
        let (mut display, mut description, mut color) = (None, None, None);
        if !req.body.iter().all(u8::is_ascii_whitespace) {
            let doc = match xml::parse(req.body) {
                Ok(d) => d,
                Err(e) => return Reply::text(400, e),
            };
            if !(doc.is(CALDAV, "mkcalendar") || doc.is(DAV, "mkcol")) {
                return Reply::text(400, "a mkcalendar or a mkcol is expected");
            }
            // MKCOL says what it makes in its resource type; only the kind of the path is made here.
            if let Some(rt) = doc.child(DAV, "set").and_then(|s| s.child(DAV, "prop")).and_then(|p| p.child(DAV, "resourcetype")) {
                let wants_book = rt.child(CARDDAV, "addressbook").is_some();
                let wants_cal = rt.child(CALDAV, "calendar").is_some();
                if (wants_book && kind != Kind::AddressBook) || (wants_cal && kind != Kind::Calendar) {
                    return Reply::error(403, DAV, "valid-resourcetype", "");
                }
            }
            let Updates { display: d, description: de, color: c, refused } = Self::updates(&doc);
            if !refused.is_empty() && doc.is(CALDAV, "mkcalendar") {
                return Reply::error(403, DAV, "forbidden", "");
            }
            (display, description, color) = (d, de, c);
        }
        if !calendar_method && kind == Kind::Calendar {
            // A plain MKCOL under /calendars/ must say it is a calendar.
            if req.body.iter().all(u8::is_ascii_whitespace) {
                return Reply::error(403, DAV, "valid-resourcetype", "");
            }
        }
        if !Self::valid_fields(display.as_deref(), description.as_deref(), color.as_deref()) {
            return Reply::text(400, "a value is not valid");
        }
        match self.repo.create_collection(who.account, kind.as_str(), name, display.as_deref().unwrap_or(""), description.as_deref().unwrap_or("")).await {
            Ok(_) => {}
            Err(Error::Conflict(_)) => return Reply::text(405, "a resource exists there"),
            Err(Error::Quota(_)) => return Reply::text(507, "too many collections"),
            Err(Error::Invalid(_)) => return Reply::text(403, "the name or a value is not valid"),
            Err(e) => return Reply::storage(&e),
        }
        if color.is_some() {
            if let Err(e) = self.repo.update_collection(who.account, kind.as_str(), name, None, None, color.as_deref()).await {
                return Reply::storage(&e);
            }
        }
        Reply::new(201).header("location", collection_href(kind, &who.address, name))
    }

    // ---------------------------------------------------------------------------------------- GET, PUT, DELETE, COPY, MOVE

    async fn get(&self, who: &Principal, kind: Kind, cname: &str, oname: &str) -> Reply {
        let c = match self.collection(who, kind, cname).await {
            Ok(Some(c)) => c,
            Ok(None) => return Reply::text(404, "there is no such collection"),
            Err(r) => return r,
        };
        match self.repo.object(c.id, oname).await {
            Ok(Some(o)) => {
                let mut r = Reply::new(200).header("etag", quoted_etag(&o.etag)).header("last-modified", http_date(o.updated_at));
                r.content_type = if kind == Kind::Calendar { "text/calendar; charset=utf-8" } else { "text/vcard; charset=utf-8" };
                r.body = o.body;
                r
            }
            Ok(None) => Reply::text(404, "there is no such object"),
            Err(e) => Reply::storage(&e),
        }
    }

    /// What the table keeps beside the text of an object, from the object itself; `Err` is the reply for one that is not valid.
    fn read_object(kind: Kind, name: &str, body: &[u8]) -> Result<NewDavObject, Reply> {
        if body.len() > MAX_OBJECT_BYTES {
            return Err(Reply::text(413, "the object is too large"));
        }
        let Ok(text) = std::str::from_utf8(body) else { return Err(Reply::text(415, "the object is not UTF-8 text")) };
        if !name.ends_with(kind.extension()) {
            return Err(Reply::text(403, &format!("the name of an object ends with {}", kind.extension())));
        }
        match kind {
            Kind::Calendar => match ical::read(text) {
                Ok(o) => Ok(NewDavObject { name: name.to_owned(), uid: o.uid, body: text.to_owned(), component: o.component.to_owned(), starts_at: o.starts_at, ends_at: o.ends_at, recurring: o.recurring }),
                Err(e) => Err(Reply::error(403, CALDAV, "valid-calendar-object-resource", &escape(e))),
            },
            Kind::AddressBook => match vcard::read(text) {
                Ok(c) => {
                    let uid = c.uid.unwrap_or_else(|| name.trim_end_matches(kind.extension()).to_owned());
                    Ok(NewDavObject { name: name.to_owned(), uid, body: text.to_owned(), component: "VCARD".to_owned(), starts_at: None, ends_at: None, recurring: false })
                }
                Err(e) => Err(Reply::error(403, CARDDAV, "valid-address-data", &escape(e))),
            },
        }
    }

    fn conflict(kind: Kind, who: &Principal, cname: &str, other: &str) -> Reply {
        let ns = if kind == Kind::Calendar { CALDAV } else { CARDDAV };
        Reply::error(409, ns, "no-uid-conflict", &href(&object_href(kind, &who.address, cname, other)))
    }

    async fn store(&self, who: &Principal, kind: Kind, c: &DavCollection, new: &NewDavObject, pre: &DavPrecondition) -> Reply {
        match self.repo.put(c.id, new, pre).await {
            Ok(DavPutResult::Created { etag, .. }) => Reply::new(201).header("etag", quoted_etag(&etag)),
            Ok(DavPutResult::Updated { etag, .. }) => Reply::new(204).header("etag", quoted_etag(&etag)),
            Ok(DavPutResult::PreconditionFailed) => Reply::text(412, "the precondition failed"),
            Ok(DavPutResult::UidConflict(other)) => Self::conflict(kind, who, &c.name, &other),
            Ok(DavPutResult::Full(what)) => Reply::text(507, &format!("there is no room for more {what}")),
            Err(Error::Invalid(_)) => Reply::text(400, "the object is not valid"),
            Err(Error::NotFound(_)) => Reply::text(409, "the collection is gone"),
            Err(e) => Reply::storage(&e),
        }
    }

    async fn put(&self, who: &Principal, req: &Req<'_>, kind: Kind, cname: &str, oname: &str) -> Reply {
        let c = match self.collection(who, kind, cname).await {
            Ok(Some(c)) => c,
            Ok(None) => return Reply::text(409, "there is no such collection"),
            Err(r) => return r,
        };
        let new = match Self::read_object(kind, oname, req.body) {
            Ok(n) => n,
            Err(r) => return r,
        };
        let pre = match self.precondition(&c, oname, req.if_match, req.if_none_match).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        self.store(who, kind, &c, &new, &pre).await
    }

    /// `If-None-Match: *` and `If-Match` as the store's precondition (`If-Match: *` asks only that the object is there).
    async fn precondition(&self, c: &DavCollection, oname: &str, if_match: Option<&str>, if_none_match: Option<&str>) -> Result<DavPrecondition, Reply> {
        if if_none_match.map(str::trim) == Some("*") {
            return Ok(DavPrecondition::Absent);
        }
        match if_match.map(str::trim) {
            None => Ok(DavPrecondition::None),
            Some("*") => match self.repo.object(c.id, oname).await {
                Ok(Some(_)) => Ok(DavPrecondition::None),
                Ok(None) => Err(Reply::text(412, "the precondition failed")),
                Err(e) => Err(Reply::storage(&e)),
            },
            Some(tag) if tag.contains(',') => Err(Reply::text(412, "one tag in If-Match is understood")),
            Some(tag) => Ok(DavPrecondition::Etag(bare_etag(tag))),
        }
    }

    async fn delete_object(&self, who: &Principal, req: &Req<'_>, kind: Kind, cname: &str, oname: &str) -> Reply {
        let c = match self.collection(who, kind, cname).await {
            Ok(Some(c)) => c,
            Ok(None) => return Reply::text(404, "there is no such collection"),
            Err(r) => return r,
        };
        let pre = match self.precondition(&c, oname, req.if_match, None).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        match self.repo.delete(c.id, oname, &pre).await {
            Ok(DavDeleteResult::Deleted { .. }) => Reply::new(204),
            Ok(DavDeleteResult::NotFound) => Reply::text(404, "there is no such object"),
            Ok(DavDeleteResult::PreconditionFailed) => Reply::text(412, "the precondition failed"),
            Err(e) => Reply::storage(&e),
        }
    }

    async fn copy_move(&self, who: &Principal, req: &Req<'_>, kind: Kind, cname: &str, oname: &str) -> Reply {
        let Some(dest) = req.destination else { return Reply::text(400, "a Destination is expected") };
        // The header is a URL or a path; only the path counts, and it must name an object of this account.
        let path = match dest.find("://") {
            Some(i) => dest.get(i + 3..).and_then(|r| r.find('/').and_then(|j| r.get(j..))).unwrap_or(""),
            None => dest,
        };
        let Some(Resource::Object(dkind, da, dc, dobj)) = paths::parse(path) else { return Reply::text(403, "the destination is not an object of yours") };
        if dkind != kind || !da.eq_ignore_ascii_case(&who.address) {
            return Reply::text(403, "the destination is not an object of yours");
        }
        let src = match self.collection(who, kind, cname).await {
            Ok(Some(c)) => c,
            Ok(None) => return Reply::text(404, "there is no such collection"),
            Err(r) => return r,
        };
        let dst = match self.collection(who, dkind, &dc).await {
            Ok(Some(c)) => c,
            Ok(None) => return Reply::text(409, "the destination collection is not there"),
            Err(r) => return r,
        };
        let obj = match self.repo.object(src.id, oname).await {
            Ok(Some(o)) => o,
            Ok(None) => return Reply::text(404, "there is no such object"),
            Err(e) => return Reply::storage(&e),
        };
        if src.id == dst.id && oname == dobj {
            return Reply::text(403, "the source and the destination are the same");
        }
        let new = match Self::read_object(kind, &dobj, obj.body.as_bytes()) {
            Ok(mut n) => {
                // A copy keeps the UID the object has (the contact's own, or the name it had when it had none).
                n.uid = obj.uid.clone();
                n
            }
            Err(r) => return r,
        };
        let pre = if req.overwrite.map(str::trim) == Some("F") { DavPrecondition::Absent } else { DavPrecondition::None };
        let reply = self.store(who, kind, &dst, &new, &pre).await;
        if req.verb == Verb::Move && matches!(reply.status, 201 | 204) {
            if let Err(e) = self.repo.delete(src.id, oname, &DavPrecondition::None).await {
                return Reply::storage(&e);
            }
        }
        reply
    }

    // ---------------------------------------------------------------------------------------- REPORT

    async fn report(&self, who: &Principal, req: &Req<'_>, kind: Kind, name: &str) -> Reply {
        let doc = match xml::parse(req.body) {
            Ok(d) => d,
            Err(e) => return Reply::text(400, e),
        };
        let c = match self.collection(who, kind, name).await {
            Ok(Some(c)) => c,
            Ok(None) => return Reply::text(404, "there is no such collection"),
            Err(r) => return r,
        };
        let wanted = match doc.child(DAV, "prop") {
            Some(p) => Self::wanted(Some(p)),
            None => Wanted::Props(vec![(DAV.to_owned(), "getetag".to_owned())]),
        };
        let (ns_ok, objects): (bool, Result<Vec<DavObject>, Error>) = match (kind, doc.ns.as_str(), doc.name.as_str()) {
            (Kind::Calendar, CALDAV, "calendar-query") => (true, self.calendar_query(&c, &doc).await),
            (Kind::AddressBook, CARDDAV, "addressbook-query") => (true, self.addressbook_query(&c, &doc).await),
            (Kind::Calendar, CALDAV, "calendar-multiget") | (Kind::AddressBook, CARDDAV, "addressbook-multiget") => {
                let names: Vec<String> = doc
                    .elements()
                    .filter(|e| e.is(DAV, "href"))
                    .take(usize::try_from(REPORT_LIMIT).unwrap_or(1000))
                    .filter_map(|h| match paths::parse(h.text().trim()) {
                        Some(Resource::Object(k, a, cn, on)) if k == kind && cn == c.name && a.eq_ignore_ascii_case(&who.address) => Some(on),
                        _ => None,
                    })
                    .collect();
                (true, self.repo.objects_named(c.id, &names).await)
            }
            (_, DAV, "sync-collection") => return self.sync_collection(who, &doc, kind, &c, &wanted).await,
            _ => (false, Ok(Vec::new())),
        };
        if !ns_ok {
            return Reply::error(403, DAV, "supported-report", "");
        }
        let list = match objects {
            Ok(l) => l,
            Err(e) => return Reply::storage(&e),
        };
        let mut out = String::new();
        let is_multiget = doc.name.ends_with("multiget");
        let mut seen: Vec<&str> = Vec::new();
        for o in &list {
            if out.len() > MAX_RESPONSE_BYTES {
                out.push_str(&truncated(&collection_href(kind, &who.address, &c.name)));
                break;
            }
            seen.push(&o.name);
            out.push_str(&self.response(who, &object_href(kind, &who.address, &c.name, &o.name), &Target::Object(kind, o), &wanted, true));
        }
        if is_multiget {
            for h in doc.elements().filter(|e| e.is(DAV, "href")).take(usize::try_from(REPORT_LIMIT).unwrap_or(1000)) {
                let text = h.text();
                let known = matches!(paths::parse(text.trim()), Some(Resource::Object(_, _, _, ref on)) if seen.contains(&on.as_str()));
                if !known {
                    out.push_str(&format!("<D:response>{}<D:status>HTTP/1.1 404 Not Found</D:status></D:response>", href(text.trim())));
                }
            }
        }
        Self::multistatus(&out)
    }

    async fn sync_collection(&self, who: &Principal, doc: &Element, kind: Kind, c: &DavCollection, wanted: &Wanted) -> Reply {
        let token = doc.child(DAV, "sync-token").map(|t| t.text()).unwrap_or_default();
        let token = token.trim();
        let since = if token.is_empty() {
            0
        } else {
            let parsed = token.strip_prefix("urn:rusty:sync:").and_then(|r| r.split_once(':')).and_then(|(id, rev)| Some((id.parse::<i64>().ok()?, rev.parse::<i64>().ok()?)));
            match parsed {
                Some((id, rev)) if id == c.id && rev >= 0 => rev,
                _ => return Reply::error(403, DAV, "valid-sync-token", ""),
            }
        };
        let changes = match self.repo.changes_since(c.id, since, REPORT_LIMIT).await {
            Ok(Some(ch)) => ch,
            Ok(None) => return Reply::error(403, DAV, "valid-sync-token", ""),
            Err(e) => return Reply::storage(&e),
        };
        let names: Vec<String> = changes.changed.iter().map(|(n, _)| n.clone()).collect();
        let objects = match self.repo.objects_named(c.id, &names).await {
            Ok(o) => o,
            Err(e) => return Reply::storage(&e),
        };
        let mut out = String::new();
        for o in &objects {
            if out.len() > MAX_RESPONSE_BYTES {
                out.push_str(&truncated(&collection_href(kind, &who.address, &c.name)));
                break;
            }
            out.push_str(&self.response(who, &object_href(kind, &who.address, &c.name, &o.name), &Target::Object(kind, o), wanted, true));
        }
        for d in &changes.deleted {
            out.push_str(&format!("<D:response>{}<D:status>HTTP/1.1 404 Not Found</D:status></D:response>", href(&object_href(kind, &who.address, &c.name, d))));
        }
        if changes.truncated {
            out.push_str(&format!("<D:response>{}<D:status>HTTP/1.1 507 Insufficient Storage</D:status><D:error><D:number-of-matches-within-limits/></D:error></D:response>", href(&collection_href(kind, &who.address, &c.name))));
        }
        out.push_str(&format!("<D:sync-token>urn:rusty:sync:{}:{}</D:sync-token>", c.id, changes.revision));
        Self::multistatus(&out)
    }

    async fn calendar_query(&self, c: &DavCollection, doc: &Element) -> Result<Vec<DavObject>, Error> {
        let f = CalFilter::from(doc.child(CALDAV, "filter"));
        let found = self.repo.objects_in_range(c.id, f.component.as_deref(), f.from, f.to, OBJECT_LIMIT).await?;
        let mut out = Vec::new();
        for o in found {
            if f.props.is_empty() || f.matches(&o.body) {
                out.push(o);
                if i64::try_from(out.len()).unwrap_or(i64::MAX) >= REPORT_LIMIT {
                    break;
                }
            }
        }
        Ok(out)
    }

    async fn addressbook_query(&self, c: &DavCollection, doc: &Element) -> Result<Vec<DavObject>, Error> {
        let filter = doc.child(CARDDAV, "filter");
        let all_of = filter.and_then(|f| f.attr("test")) == Some("allof");
        let props: Vec<PropFilter> = filter.into_iter().flat_map(Element::elements).filter(|e| e.is(CARDDAV, "prop-filter")).map(PropFilter::from).collect();
        let limit = doc.child(CARDDAV, "limit").and_then(|l| l.child(CARDDAV, "nresults")).and_then(|n| n.text().trim().parse::<i64>().ok()).filter(|n| *n > 0).map_or(REPORT_LIMIT, |n| n.min(REPORT_LIMIT));
        let all = self.repo.objects(c.id, OBJECT_LIMIT).await?;
        let mut out = Vec::new();
        for o in all {
            let ok = props.is_empty() || {
                match content::parse(&o.body) {
                    Ok(tree) => {
                        let mut results = props.iter().map(|p| p.holds(&tree));
                        if all_of { results.all(|r| r) } else { results.any(|r| r) }
                    }
                    Err(_) => false,
                }
            };
            if ok {
                out.push(o);
                if i64::try_from(out.len()).unwrap_or(i64::MAX) >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}

fn allow_of(res: &Resource) -> &'static str {
    match res {
        Resource::Root | Resource::Principal(_) | Resource::Home(..) => "OPTIONS, PROPFIND",
        Resource::Collection(..) => "OPTIONS, PROPFIND, PROPPATCH, REPORT, MKCOL, MKCALENDAR, DELETE",
        Resource::Object(..) => "OPTIONS, GET, HEAD, PUT, DELETE, COPY, MOVE, PROPFIND",
    }
}

// -------------------------------------------------------------------------------------------- filters

/// `prop-filter` of a query: a property of the object that must be (or must not be) there, with a text that must (or must not) be in it.
struct PropFilter {
    name: String,
    not_defined: bool,
    text: Option<TextMatch>,
}

struct TextMatch {
    value: String,
    negate: bool,
    mode: MatchMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    Contains,
    Equals,
    StartsWith,
    EndsWith,
}

impl PropFilter {
    fn from(e: &Element) -> PropFilter {
        let text = e.elements().find(|t| t.name == "text-match").map(|t| TextMatch {
            value: t.text().to_lowercase(),
            negate: t.attr("negate-condition") == Some("yes"),
            mode: match t.attr("match-type") {
                Some("equals") => MatchMode::Equals,
                Some("starts-with") => MatchMode::StartsWith,
                Some("ends-with") => MatchMode::EndsWith,
                _ => MatchMode::Contains,
            },
        });
        PropFilter { name: e.attr("name").unwrap_or("").to_ascii_uppercase(), not_defined: e.elements().any(|x| x.name == "is-not-defined"), text }
    }

    /// Does the component satisfy the filter? Comparison ignores case (the default collation of both protocols).
    fn holds(&self, c: &content::Component) -> bool {
        let found: Vec<&content::Prop> = c.props_named(&self.name).collect();
        if self.not_defined {
            return found.is_empty();
        }
        if found.is_empty() {
            return false;
        }
        let Some(t) = &self.text else { return true };
        let hit = found.iter().any(|p| {
            let v = p.value.to_lowercase();
            match t.mode {
                MatchMode::Contains => v.contains(&t.value),
                MatchMode::Equals => v == t.value,
                MatchMode::StartsWith => v.starts_with(&t.value),
                MatchMode::EndsWith => v.ends_with(&t.value),
            }
        });
        hit != t.negate
    }
}

/// The part of a `calendar-query` the store and the readers answer: the component, the time range and the property filters.
struct CalFilter {
    component: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    props: Vec<PropFilter>,
    all_of: bool,
}

impl CalFilter {
    fn from(filter: Option<&Element>) -> CalFilter {
        let mut f = CalFilter { component: None, from: None, to: None, props: Vec::new(), all_of: true };
        let Some(outer) = filter.and_then(|f| f.child(CALDAV, "comp-filter")) else { return f };
        if let Some(inner) = outer.child(CALDAV, "comp-filter") {
            f.component = inner.attr("name").map(str::to_ascii_uppercase).filter(|n| matches!(n.as_str(), "VEVENT" | "VTODO" | "VJOURNAL"));
            if let Some(tr) = inner.child(CALDAV, "time-range") {
                f.from = tr.attr("start").and_then(parse_moment).map(|m| m.secs);
                f.to = tr.attr("end").and_then(parse_moment).map(|m| m.secs);
            }
            f.props = inner.elements().filter(|e| e.is(CALDAV, "prop-filter")).map(PropFilter::from).collect();
            f.all_of = inner.attr("test") != Some("anyof");
        }
        f
    }

    /// The property filters on the object's own components of the kind asked for (the master and its overrides).
    fn matches(&self, body: &str) -> bool {
        let Ok(tree) = content::parse(body) else { return false };
        let comps: Vec<&content::Component> = tree.children.iter().filter(|c| self.component.as_deref().is_none_or(|k| c.name == k)).collect();
        comps.iter().any(|c| {
            let mut results = self.props.iter().map(|p| p.holds(c));
            if self.all_of { results.all(|r| r) } else { results.any(|r| r) }
        })
    }
}
