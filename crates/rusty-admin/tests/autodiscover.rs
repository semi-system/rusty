//! Auto-setup for Outlook (Autodiscover) and Apple Mail (a configuration profile): public, no session, the platform's servers whatever the domain, the same answer for a domain that
//! exists and one that does not, and nothing from the request gets into the markup.
mod common;

use common::*;

const OUTLOOK_REQUEST: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><Autodiscover xmlns=\"http://schemas.microsoft.com/exchange/autodiscover/outlook/requestschema/2006\"><Request><EMailAddress>{ADDR}</EMailAddress><AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a</AcceptableResponseSchema></Request></Autodiscover>";

fn request_for(addr: &str) -> String {
    OUTLOOK_REQUEST.replace("{ADDR}", addr)
}

#[tokio::test]
async fn outlook_asks_with_post_under_either_spelling_and_gets_the_imap_and_smtp_settings() {
    let r = rig().await;
    for path in ["/autodiscover/autodiscover.xml", "/Autodiscover/Autodiscover.xml"] {
        let h = [("Content-Type", "text/xml")];
        let a = send(r.addr, "POST", path, &h, &request_for("Alice@Known.Test")).await;
        assert_eq!(a.status, 200, "{path}: {}", a.body);
        assert!(a.header("content-type").is_some_and(|t| t.starts_with("text/xml")), "{:?}", a.header("content-type"));
        assert!(a.header("set-cookie").is_none(), "no session is made");
        for want in ["<Type>IMAP</Type>", "<Server>mail.example.org</Server>", "<Port>993</Port>", "<SSL>on</SSL>", "<Type>SMTP</Type>", "<Port>465</Port>", "<Encryption>SSL</Encryption>", "<LoginName>alice@known.test</LoginName>"] {
            assert!(a.body.contains(want), "{want}: {}", a.body);
        }
    }
    // The same answer whether or not the domain exists here: it tells nothing about who is a customer.
    let known = send(r.addr, "POST", "/autodiscover/autodiscover.xml", &[], &request_for("bob@known.test")).await;
    let unknown = send(r.addr, "POST", "/autodiscover/autodiscover.xml", &[], &request_for("bob@never-heard-of.test")).await;
    assert_eq!(known.body.replace("known.test", "X"), unknown.body.replace("never-heard-of.test", "X"));
    // A browser test: GET with the address in the query.
    let get = send(r.addr, "GET", "/autodiscover/autodiscover.xml?emailaddress=bob@known.test", &[], "").await;
    assert_eq!(get.status, 200);
}

#[tokio::test]
async fn outlooks_request_without_a_usable_address_is_refused_and_nothing_of_it_is_echoed() {
    let r = rig().await;
    for body in ["", "<Autodiscover/>", &request_for("nobody"), &request_for("a<script>@x.test"), &request_for("a@b c.test"), &request_for("x@y.test\"><evil/>")] {
        let a = send(r.addr, "POST", "/autodiscover/autodiscover.xml", &[], body).await;
        assert_eq!(a.status, 400, "{body}: {}", a.body);
        assert!(!a.body.contains("script") && !a.body.contains("evil"), "{}", a.body);
    }
}

#[tokio::test]
async fn apple_mail_gets_a_profile_that_adds_the_account_and_holds_no_password() {
    let r = rig().await;
    let p = send(r.addr, "GET", "/mail/config.mobileconfig?emailaddress=alice%40Known.Test", &[], "").await;
    assert_eq!(p.status, 200, "{}", p.body);
    assert!(p.header("content-type").is_some_and(|t| t.starts_with("application/x-apple-aspen-config")), "{:?}", p.header("content-type"));
    assert!(p.header("content-disposition").is_some_and(|d| d.contains("attachment") && d.contains("mail.mobileconfig")), "{:?}", p.header("content-disposition"));
    assert!(p.header("set-cookie").is_none());
    for want in ["<string>EmailTypeIMAP</string>", "<string>alice@known.test</string>", "<string>mail.example.org</string>", "<integer>993</integer>", "<integer>465</integer>", "com.apple.mail.managed", "<key>PayloadUUID</key>"] {
        assert!(p.body.contains(want), "{want}: {}", p.body);
    }
    assert!(!p.body.to_ascii_lowercase().contains("<key>incomingmailserverpassword") && !p.body.to_ascii_lowercase().contains("<key>outgoingmailserverpassword"), "no password in a profile");
    // Installing it twice updates the account (the same identifiers), and another address gets other ones.
    let again = send(r.addr, "GET", "/mail/config.mobileconfig?email=alice@known.test", &[], "").await;
    assert_eq!(p.body, again.body);
    let other = send(r.addr, "GET", "/mail/config.mobileconfig?email=bob@known.test", &[], "").await;
    assert_ne!(p.body, other.body);
    // No address, or a hostile one: refused.
    for q in ["", "?emailaddress=", "?emailaddress=nobody", "?emailaddress=a@%3Cscript%3E.test"] {
        let bad = send(r.addr, "GET", &format!("/mail/config.mobileconfig{q}"), &[], "").await;
        assert_eq!(bad.status, 400, "{q}");
    }
    assert!(matches!(send(r.addr, "POST", "/mail/config.mobileconfig?email=a@b.test", &[], "").await.status, 404 | 405));
}

#[tokio::test]
async fn the_newer_outlook_asks_for_autodiscover_v2_and_is_sent_to_the_xml_form_on_the_mail_host() {
    let r = rig().await;
    for path in ["/autodiscover/autodiscover.json", "/Autodiscover/Autodiscover.json"] {
        let a = send(r.addr, "GET", &format!("{path}?Email=alice%40known.test&Protocol=AutodiscoverV1&RedirectCount=1"), &[], "").await;
        assert_eq!(a.status, 200, "{path}: {}", a.body);
        assert!(a.header("content-type").is_some_and(|t| t.starts_with("application/json")), "{:?}", a.header("content-type"));
        assert!(a.header("set-cookie").is_none());
        assert_eq!(a.json(), serde_json::json!({ "Protocol": "AutodiscoverV1", "Url": "https://mail.example.org/autodiscover/autodiscover.xml" }));
    }
    // Another protocol (ActiveSync, EWS) is not served, in the error shape Outlook reads; so is a request with none.
    for q in ["?Email=a@b.test&Protocol=ActiveSync", "?Email=a@b.test&Protocol=EWS", "?Email=a@b.test", ""] {
        let a = send(r.addr, "GET", &format!("/autodiscover/autodiscover.json{q}"), &[], "").await;
        assert_eq!(a.status, 400, "{q}");
        assert_eq!(a.json()["ErrorCode"], "InvalidProtocol");
    }
    assert!(matches!(send(r.addr, "POST", "/autodiscover/autodiscover.json?Protocol=AutodiscoverV1", &[], "").await.status, 404 | 405));
}

#[tokio::test]
async fn the_mozilla_answer_is_also_served_at_the_well_known_path() {
    let r = rig().await;
    let a = send(r.addr, "GET", "/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress=alice@known.test", &[], "").await;
    let b = send(r.addr, "GET", "/mail/config-v1.1.xml?emailaddress=alice@known.test", &[], "").await;
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(a.body, b.body, "the same answer at both paths");
    assert!(a.body.contains("<hostname>mail.example.org</hostname>") && a.body.contains("<incomingServer type=\"pop3\">"), "POP3 is on by default, so it is offered");
}
