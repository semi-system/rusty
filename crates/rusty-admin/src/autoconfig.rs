//! Mail client auto-setup (Mozilla autoconfig, the format Thunderbird reads): `GET /mail/config-v1.1.xml[?emailaddress=user@domain]`.
//!
//! A client that is given `user@example.org` asks `http://autoconfig.example.org/mail/config-v1.1.xml?emailaddress=user@example.org`; the edge sends that one request (host
//! `autoconfig.*`, exactly this path, port 80) to this service. The answer names the server of the platform (the configured mail host name) with IMAP over TLS on 993 and
//! submission with STARTTLS on 587 and the address as the user name, whatever the domain: it says nothing about which domains or accounts exist, so it cannot be used to find
//! them out. Nothing here needs a session and nothing here reads the database.
//!
//! The domain echoed in the answer is the one of the address in the query (checked as a DNS name, so it can carry no markup), else the mail host's.

use std::sync::Arc;

use rusty_http::{handler, Method, Request, Response, Router};

use crate::api::AdminApp;

/// A DNS name, as far as this answer is concerned: dot-separated labels of letters, digits and `-`.
pub(crate) fn domain_ok(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 253
        && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// The domain of `?emailaddress=local@domain` (percent-decoding is limited to `%40`, the `@` a client escapes), lower-cased.
fn domain_of(query: Option<&str>) -> Option<String> {
    let q = query?;
    let value = q.split('&').find_map(|p| p.strip_prefix("emailaddress="))?;
    let value = value.replace("%40", "@");
    let (_, domain) = value.rsplit_once('@')?;
    let domain = domain.trim().to_ascii_lowercase();
    domain_ok(&domain).then_some(domain)
}

fn xml(host: &str, domain: &str, pop3: bool) -> String {
    // POP3 is offered as a second incoming server only where the installation runs it (TLS on 995, the address as the user name).
    let pop = if pop3 {
        format!(
            "    <incomingServer type=\"pop3\">\n\
      <hostname>{host}</hostname>\n\
      <port>995</port>\n\
      <socketType>SSL</socketType>\n\
      <authentication>password-cleartext</authentication>\n\
      <username>%EMAILADDRESS%</username>\n\
    </incomingServer>\n"
        )
    } else {
        String::new()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<clientConfig version=\"1.1\">\n\
  <emailProvider id=\"{domain}\">\n\
    <domain>{domain}</domain>\n\
    <displayName>{domain}</displayName>\n\
    <displayShortName>{domain}</displayShortName>\n\
    <incomingServer type=\"imap\">\n\
      <hostname>{host}</hostname>\n\
      <port>993</port>\n\
      <socketType>SSL</socketType>\n\
      <authentication>password-cleartext</authentication>\n\
      <username>%EMAILADDRESS%</username>\n\
    </incomingServer>\n\
{pop}\
    <outgoingServer type=\"smtp\">\n\
      <hostname>{host}</hostname>\n\
      <port>587</port>\n\
      <socketType>STARTTLS</socketType>\n\
      <authentication>password-cleartext</authentication>\n\
      <username>%EMAILADDRESS%</username>\n\
    </outgoingServer>\n\
  </emailProvider>\n\
</clientConfig>\n"
    )
}

async fn config(app: &AdminApp, req: Request) -> Response {
    let Some(mail) = app.services.as_ref().and_then(|s| s.mail.as_ref()) else {
        return Response::error(404, "not_found");
    };
    let host = mail.host.to_ascii_lowercase();
    if !domain_ok(&host) {
        return Response::error(404, "not_found");
    }
    let domain = domain_of(req.query.as_deref()).unwrap_or_else(|| host.clone());
    Response::new(200)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_header("cache-control", "public, max-age=300")
        .with_header("x-content-type-options", "nosniff")
        .with_body(xml(&host, &domain, crate::protocols::is_on(app, "pop3").await).into_bytes())
}

/// The route of this part of the service.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    // The path Thunderbird asks for, and the same answer where the domain's own web server would give it (`/.well-known/autoconfig/mail/config-v1.1.xml`): served for the clients that are given the mail host.
    let mut router = router;
    for path in ["/mail/config-v1.1.xml", "/.well-known/autoconfig/mail/config-v1.1.xml"] {
        let a = app.clone();
        router = router.route(Method::Get, path, handler(move |r: Request| {
            let a = a.clone();
            async move { config(&a, r).await }
        }));
    }
    router
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_domain_comes_from_the_address_and_nothing_else_gets_into_the_answer() {
        assert_eq!(domain_of(Some("emailaddress=alice%40Example.ORG")).as_deref(), Some("example.org"));
        assert_eq!(domain_of(Some("x=1&emailaddress=alice@example.org")).as_deref(), Some("example.org"));
        for bad in ["emailaddress=alice@exa mple.org", "emailaddress=alice@a<b>.org", "emailaddress=alice@-x.org", "emailaddress=alice@", "emailaddress=alice", "emailaddress=alice@a..b", "other=1"] {
            assert_eq!(domain_of(Some(bad)), None, "{bad}");
        }
        assert_eq!(domain_of(None), None);
        assert!(!xml("mail.example.org", "example.org", false).contains("<script"));
        // POP3 is listed only when the installation runs it.
        assert!(!xml("mail.example.org", "example.org", false).contains("pop3"));
        let with = xml("mail.example.org", "example.org", true);
        assert!(with.contains("<incomingServer type=\"pop3\">") && with.contains("<port>995</port>") && with.contains("<incomingServer type=\"imap\">"));
    }
}
