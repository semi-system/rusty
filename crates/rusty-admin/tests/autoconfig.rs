//! Mail client auto-setup: the Mozilla autoconfig answer needs no session, names the platform's servers whatever the domain, echoes only a well-formed domain and says nothing about
//! which domains or accounts exist.
mod common;

use common::*;

#[tokio::test]
async fn the_answer_is_public_names_the_mail_servers_and_is_the_same_for_every_domain() {
    let r = rig().await;
    let none = send(r.addr, "GET", "/mail/config-v1.1.xml", &[], "").await;
    assert_eq!(none.status, 200, "{}", none.body);
    assert!(none.header("content-type").is_some_and(|t| t.starts_with("application/xml")), "{:?}", none.header("content-type"));
    assert!(none.header("set-cookie").is_none(), "no session is made");
    for want in ["<hostname>mail.example.org</hostname>", "<port>993</port>", "<socketType>SSL</socketType>", "<port>587</port>", "<socketType>STARTTLS</socketType>", "<username>%EMAILADDRESS%</username>"] {
        assert!(none.body.contains(want), "{want}: {}", none.body);
    }
    // The domain of the address is echoed, whether or not it exists here: the answer is the same for all, so it tells nothing.
    let known = send(r.addr, "GET", "/mail/config-v1.1.xml?emailaddress=alice%40Known.Test", &[], "").await;
    let unknown = send(r.addr, "GET", "/mail/config-v1.1.xml?emailaddress=bob@never-heard-of.test", &[], "").await;
    assert!(known.body.contains("<domain>known.test</domain>") && unknown.body.contains("<domain>never-heard-of.test</domain>"));
    assert_eq!(known.body.replace("known.test", "X"), unknown.body.replace("never-heard-of.test", "X"), "the same answer but for the name");
    // A domain that is not a DNS name is not echoed (no markup gets into the XML).
    let hostile = send(r.addr, "GET", "/mail/config-v1.1.xml?emailaddress=a@%3Cscript%3E.test", &[], "").await;
    assert_eq!(hostile.status, 200);
    assert!(!hostile.body.contains("script") && hostile.body.contains("<domain>mail.example.org</domain>"), "{}", hostile.body);
    // Only GET.
    assert!(matches!(send(r.addr, "POST", "/mail/config-v1.1.xml", &[], "").await.status, 404 | 405));
}
