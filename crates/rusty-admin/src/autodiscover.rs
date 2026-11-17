//! Auto-setup for the mail clients that do not read Mozilla's autoconfig (`autoconfig.rs`):
//!
//! * **Outlook** (Microsoft Autodiscover, the "POX" XML form that Outlook uses for IMAP accounts): `POST /autodiscover/autodiscover.xml` with the address in the request body
//!   (`GET` answers too, for a browser test), under both spellings of the path that clients send;
//! * **Apple Mail** (iPhone, iPad, Mac): `GET /mail/config.mobileconfig?emailaddress=user@domain` is a configuration profile that adds the account (the person types the password
//!   when they install it; no password and no secret is in the profile).
//!
//! Like the autoconfig answer these name the platform's mail host (IMAP over TLS on 993, submission over TLS on 465) with the address as the user name, whatever the domain: they say
//! nothing about which domains or accounts exist, nothing here needs a session or reads the database, and the address that is echoed has been checked to carry no markup (and is
//! escaped besides). How a client reaches this is the edge's and the DNS's business (`docs/first-mail.md`): Outlook asks `https://autodiscover.<domain>/autodiscover/autodiscover.xml`
//! and then the `_autodiscover._tcp.<domain>` SRV record, which names the mail host.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};
use sha2::{Digest, Sha256};

use crate::api::AdminApp;
use crate::autoconfig::domain_ok;

/// An address as far as these answers are concerned: a local part of letters, digits and `._+-`, `@`, a DNS name; lower-cased.
pub(crate) fn address_ok(s: &str) -> Option<String> {
    let s = s.trim();
    let (local, domain) = s.rsplit_once('@')?;
    let local_ok = !local.is_empty() && local.len() <= 64 && local.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'));
    let domain = domain.to_ascii_lowercase();
    (local_ok && domain_ok(&domain)).then(|| format!("{}@{domain}", local.to_ascii_lowercase()))
}

/// The text between `<tag>` and `</tag>` (a tag without attributes), the way Outlook writes `EMailAddress`.
fn between<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The Autodiscover answer for Outlook: IMAP over TLS on 993 and SMTP over TLS on 465, the address as the login name.
pub(crate) fn outlook_xml(host: &str, address: &str, pop3: bool) -> String {
    let (host, login) = (xml_escape(host), xml_escape(address));
    // POP3 over TLS on 995, listed after IMAP (Outlook takes the first protocol it can use) only where the installation runs it.
    let pop = if pop3 {
        format!(
            "      <Protocol>\n\
        <Type>POP3</Type>\n\
        <Server>{host}</Server>\n\
        <Port>995</Port>\n\
        <DomainRequired>off</DomainRequired>\n\
        <LoginName>{login}</LoginName>\n\
        <SPA>off</SPA>\n\
        <SSL>on</SSL>\n\
        <AuthRequired>on</AuthRequired>\n\
        <LeaveMessagesOnServer>on</LeaveMessagesOnServer>\n\
      </Protocol>\n"
        )
    } else {
        String::new()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<Autodiscover xmlns=\"http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006\">\n\
  <Response xmlns=\"http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a\">\n\
    <Account>\n\
      <AccountType>email</AccountType>\n\
      <Action>settings</Action>\n\
      <Protocol>\n\
        <Type>IMAP</Type>\n\
        <Server>{host}</Server>\n\
        <Port>993</Port>\n\
        <DomainRequired>off</DomainRequired>\n\
        <LoginName>{login}</LoginName>\n\
        <SPA>off</SPA>\n\
        <SSL>on</SSL>\n\
        <AuthRequired>on</AuthRequired>\n\
      </Protocol>\n\
      <Protocol>\n\
        <Type>SMTP</Type>\n\
        <Server>{host}</Server>\n\
        <Port>465</Port>\n\
        <DomainRequired>off</DomainRequired>\n\
        <LoginName>{login}</LoginName>\n\
        <SPA>off</SPA>\n\
        <Encryption>SSL</Encryption>\n\
        <AuthRequired>on</AuthRequired>\n\
        <UsePOPAuth>off</UsePOPAuth>\n\
        <SMTPLast>off</SMTPLast>\n\
      </Protocol>\n\
{pop}\
    </Account>\n\
  </Response>\n\
</Autodiscover>\n"
    )
}

/// A UUID (8-4-4-4-12 hex) made of what the profile is about, so the same account always gets the same identifiers and installing it twice updates it instead of adding a second one.
fn uuid_of(what: &str) -> String {
    let h = Sha256::digest(what.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    b[6] = (b[6] & 0x0f) | 0x50; // version 5 layout
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let x = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..])
}

/// The Apple configuration profile (unsigned): an IMAP account over TLS on 993 and submission over TLS on 465, the address as the user name, no password.
pub(crate) fn mobileconfig(host: &str, address: &str, pop3: bool) -> String {
    let (h, a) = (xml_escape(host), xml_escape(address));
    // `EmailTypeIMAP` or `EmailTypePOP`; a POP3 profile has another incoming port and its own identifiers (one address may have both accounts).
    let (kind, in_port, which) = if pop3 { ("EmailTypePOP", 995, "pop") } else { ("EmailTypeIMAP", 993, "imap") };
    let account = uuid_of(&format!("account:{which}:{host}:{address}"));
    let profile = uuid_of(&format!("profile:{which}:{host}:{address}"));
    let reverse: String = host.split('.').rev().collect::<Vec<_>>().join(".");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>PayloadContent</key>\n\
  <array>\n\
    <dict>\n\
      <key>EmailAccountDescription</key><string>{a}</string>\n\
      <key>EmailAccountName</key><string>{a}</string>\n\
      <key>EmailAccountType</key><string>{kind}</string>\n\
      <key>EmailAddress</key><string>{a}</string>\n\
      <key>IncomingMailServerAuthentication</key><string>EmailAuthPassword</string>\n\
      <key>IncomingMailServerHostName</key><string>{h}</string>\n\
      <key>IncomingMailServerPortNumber</key><integer>{in_port}</integer>\n\
      <key>IncomingMailServerUseSSL</key><true/>\n\
      <key>IncomingMailServerUsername</key><string>{a}</string>\n\
      <key>OutgoingMailServerAuthentication</key><string>EmailAuthPassword</string>\n\
      <key>OutgoingMailServerHostName</key><string>{h}</string>\n\
      <key>OutgoingMailServerPortNumber</key><integer>465</integer>\n\
      <key>OutgoingMailServerUseSSL</key><true/>\n\
      <key>OutgoingMailServerUsername</key><string>{a}</string>\n\
      <key>OutgoingPasswordSameAsIncomingPassword</key><true/>\n\
      <key>PayloadDescription</key><string>Adds the mail account {a}</string>\n\
      <key>PayloadDisplayName</key><string>{a}</string>\n\
      <key>PayloadIdentifier</key><string>{reverse}.mail.{account}</string>\n\
      <key>PayloadType</key><string>com.apple.mail.managed</string>\n\
      <key>PayloadUUID</key><string>{account}</string>\n\
      <key>PayloadVersion</key><integer>1</integer>\n\
      <key>SMIMEEnabled</key><false/>\n\
    </dict>\n\
  </array>\n\
  <key>PayloadDescription</key><string>Mail account {a}</string>\n\
  <key>PayloadDisplayName</key><string>{h}: {a}</string>\n\
  <key>PayloadIdentifier</key><string>{reverse}.profile.{profile}</string>\n\
  <key>PayloadOrganization</key><string>{h}</string>\n\
  <key>PayloadRemovalDisallowed</key><false/>\n\
  <key>PayloadType</key><string>Configuration</string>\n\
  <key>PayloadUUID</key><string>{profile}</string>\n\
  <key>PayloadVersion</key><integer>1</integer>\n\
</dict>\n\
</plist>\n"
    )
}

/// The configured mail host name, when there is one and it is a DNS name.
fn mail_host(app: &AdminApp) -> Option<String> {
    let host = app.services.as_ref()?.mail.as_ref()?.host.to_ascii_lowercase();
    domain_ok(&host).then_some(host)
}

/// Whether the installation runs POP3 (`[admin] pop3`).
fn pop3_on(app: &AdminApp) -> bool {
    app.services.as_ref().and_then(|s| s.mail.as_ref()).is_some_and(|m| m.pop3)
}

/// `?protocol=pop3` asks for the POP3 profile; it is honoured only where POP3 runs (otherwise the answer is the IMAP profile, which is always there).
fn wants_pop3(app: &AdminApp, query: Option<&str>) -> bool {
    pop3_on(app) && query.is_some_and(|q| q.split('&').any(|p| p == "protocol=pop3"))
}

/// `?emailaddress=` or `?email=` (the spellings clients use), with `%40` read as `@`.
fn address_of_query(query: Option<&str>) -> Option<String> {
    let q = query?;
    let value = q.split('&').find_map(|p| p.strip_prefix("emailaddress=").or_else(|| p.strip_prefix("email=")))?;
    address_ok(&value.replace("%40", "@"))
}

async fn outlook(app: &AdminApp, req: Request) -> Response {
    let Some(host) = mail_host(app) else { return Response::error(404, "not_found") };
    // The address is in the body of Outlook's POST (`<EMailAddress>`), or in the query of a browser test.
    let body = String::from_utf8_lossy(&req.body);
    let address = between(&body, "EMailAddress").and_then(address_ok).or_else(|| address_of_query(req.query.as_deref()));
    let Some(address) = address else { return Response::error(400, "invalid") };
    Response::new(200)
        .with_header("content-type", "text/xml; charset=utf-8")
        .with_header("cache-control", "no-store")
        .with_header("x-content-type-options", "nosniff")
        .with_body(outlook_xml(&host, &address, pop3_on(app)).into_bytes())
}

/// Microsoft Autodiscover v2 (`GET /autodiscover/autodiscover.json?Email=...&Protocol=AutodiscoverV1`): a newer Outlook asks for the way to the settings first, and this server's way is the XML
/// form on the mail host. Only that protocol is served: another one (ActiveSync, EWS) is an `InvalidProtocol` error, in the shape Outlook understands.
async fn outlook_v2(app: &AdminApp, req: Request) -> Response {
    let Some(host) = mail_host(app) else { return Response::error(404, "not_found") };
    let q = req.query.as_deref().unwrap_or_default();
    let protocol = q.split('&').find_map(|p| p.strip_prefix("Protocol=").or_else(|| p.strip_prefix("protocol=")));
    let json = |status: u16, body: String| {
        Response::new(status).with_header("content-type", "application/json; charset=utf-8").with_header("cache-control", "no-store").with_header("x-content-type-options", "nosniff").with_body(body.into_bytes())
    };
    match protocol {
        Some(p) if p.eq_ignore_ascii_case("AutodiscoverV1") => json(200, format!("{{\"Protocol\":\"AutodiscoverV1\",\"Url\":\"https://{host}/autodiscover/autodiscover.xml\"}}")),
        _ => json(400, "{\"ErrorCode\":\"InvalidProtocol\",\"ErrorMessage\":\"The given protocol value is invalid. Supported values: AutodiscoverV1\"}".to_owned()),
    }
}

async fn apple(app: &AdminApp, req: Request) -> Response {
    let Some(host) = mail_host(app) else { return Response::error(404, "not_found") };
    let Some(address) = address_of_query(req.query.as_deref()) else { return Response::error(400, "invalid") };
    Response::new(200)
        .with_header("content-type", "application/x-apple-aspen-config; charset=utf-8")
        .with_header("content-disposition", "attachment; filename=\"mail.mobileconfig\"")
        .with_header("cache-control", "no-store")
        .with_header("x-content-type-options", "nosniff")
        .with_body(mobileconfig(&host, &address, wants_pop3(app, req.query.as_deref())).into_bytes())
}

/// The routes of this part of the service.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let mut router = router;
    // Outlook sends `/Autodiscover/Autodiscover.xml` and others `/autodiscover/autodiscover.xml`: the router is case-sensitive, so both are routes.
    for path in ["/autodiscover/autodiscover.xml", "/Autodiscover/Autodiscover.xml"] {
        for method in [Method::Get, Method::Post] {
            let a = app.clone();
            router = router.route(method, path, handler(move |r: Request| {
                let a = a.clone();
                async move { outlook(&a, r).await }
            }));
        }
    }
    for path in ["/autodiscover/autodiscover.json", "/Autodiscover/Autodiscover.json"] {
        let a = app.clone();
        router = router.route(Method::Get, path, handler(move |r: Request| {
            let a = a.clone();
            async move { outlook_v2(&a, r).await }
        }));
    }
    router.route(Method::Get, "/mail/config.mobileconfig", handler(move |r: Request| {
        let a = app.clone();
        async move { apple(&a, r).await }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_checked_before_it_is_echoed() {
        assert_eq!(address_ok(" Alice.B+tag@Example.ORG ").as_deref(), Some("alice.b+tag@example.org"));
        for bad in ["alice", "@example.org", "al ice@example.org", "a<b>@example.org", "alice@exa mple.org", "alice@-x.org", "a&b@example.org", "alice@example.org\"><x"] {
            assert_eq!(address_ok(bad), None, "{bad}");
        }
        assert_eq!(address_ok(&format!("{}@example.org", "a".repeat(65))), None, "a local part is at most 64");
    }

    #[test]
    fn the_address_is_read_from_outlooks_request_and_from_a_query() {
        let body = "<Autodiscover xmlns=\"x\"><Request><EMailAddress>Bob@example.org</EMailAddress><AcceptableResponseSchema>y</AcceptableResponseSchema></Request></Autodiscover>";
        assert_eq!(between(body, "EMailAddress").and_then(address_ok).as_deref(), Some("bob@example.org"));
        assert_eq!(between("<a></a>", "EMailAddress"), None);
        assert_eq!(address_of_query(Some("email=bob%40example.org")).as_deref(), Some("bob@example.org"));
        assert_eq!(address_of_query(Some("x=1&emailaddress=bob@example.org")).as_deref(), Some("bob@example.org"));
        assert_eq!(address_of_query(Some("emailaddress=bob")), None);
    }

    #[test]
    fn the_answers_name_the_mail_host_and_carry_no_markup_from_the_input() {
        let o = outlook_xml("mail.example.net", "bob@example.org", false);
        assert!(!o.contains("POP3"), "POP3 is listed only where it runs");
        let with = outlook_xml("mail.example.net", "bob@example.org", true);
        assert!(with.contains("<Type>POP3</Type>") && with.contains("<Port>995</Port>") && with.find("<Type>IMAP</Type>") < with.find("<Type>POP3</Type>"), "POP3 comes after IMAP");
        assert!(o.contains("<Server>mail.example.net</Server>") && o.contains("<LoginName>bob@example.org</LoginName>") && o.contains("<Port>993</Port>") && o.contains("<Port>465</Port>"));
        let m = mobileconfig("mail.example.net", "bob@example.org", false);
        let p = mobileconfig("mail.example.net", "bob@example.org", true);
        assert!(p.contains("EmailTypePOP") && p.contains("<integer>995</integer>") && !p.contains("EmailTypeIMAP") && p != m, "the POP3 profile is its own account");
        assert!(m.contains("<string>mail.example.net</string>") && m.contains("<integer>993</integer>") && m.contains("net.example.mail.mail."));
        // Even a value that got past the checks could not break out of the markup.
        assert!(!outlook_xml("h", "a<b>&c", true).contains("<b>") && !mobileconfig("h", "a<b>&c", true).contains("<b>"));
        // The same account always has the same identifiers, and they have the UUID shape.
        assert_eq!(uuid_of("x"), uuid_of("x"));
        assert_ne!(uuid_of("x"), uuid_of("y"));
        let u = uuid_of("x");
        assert_eq!(u.len(), 36);
        assert_eq!(u.matches('-').count(), 4);
    }
}
