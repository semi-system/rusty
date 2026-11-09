//! The U7 extras: NAMESPACE, ID, SPECIAL-USE, LIST-EXTENDED, UIDPLUS (APPENDUID, COPYUID,
//! UID EXPUNGE), LITERAL+, ENABLE and the APPEND internal date. Wire-level, both backends.

mod common;

use common::*;

async fn logged_in(b: &Backend) -> (Conn, rusty_svc::Context, rusty_core::AccountId) {
    let acc = b.account("alice@a.test", "pw").await;
    let ctx = b.ctx().await;
    let mut c = Conn::open(handler(ctx.clone()), true);
    c.line().await;
    c.login("alice@a.test", "pw").await;
    (c, ctx, acc)
}

#[tokio::test]
async fn capabilities_advertise_the_extras() {
    for_each_backend(|b| async move {
        let mut c = Conn::open(handler(b.ctx().await), true);
        let g = c.line().await;
        for cap in ["UIDPLUS", "NAMESPACE", "ID", "ENABLE", "LITERAL+", "SPECIAL-USE", "LIST-EXTENDED", "IDLE", "MOVE", "UNSELECT", "SASL-IR"] {
            assert!(g.split([' ', ']']).any(|w| w == cap), "{cap} missing in {g}");
        }
        assert!(!g.contains("CONDSTORE") && !g.contains("QRESYNC"));
    })
    .await;
}

#[tokio::test]
async fn namespace_id_and_enable() {
    for_each_backend(|b| async move {
        let (mut c, _, _) = logged_in(&b).await;
        assert_eq!(c.ok("NAMESPACE").await, ["* NAMESPACE ((\"\" \"/\")) NIL NIL"]);
        assert_eq!(c.ok("ID (\"name\" \"Thunderbird\" \"version\" \"115.0\")").await, ["* ID (\"name\" \"Rusty\")"], "no version is disclosed");
        assert_eq!(c.ok("ID NIL").await.len(), 1);
        assert_eq!(c.ok("ENABLE UTF8=ACCEPT CONDSTORE").await, ["* ENABLED"], "nothing unsupported is enabled");
        // ID is allowed before login too.
        let mut d = Conn::open(handler(b.ctx().await), true);
        d.line().await;
        assert_eq!(d.ok("ID NIL").await.len(), 1);
        assert!(d.status("ID (\"k\")").await.contains("BAD"));
    })
    .await;
}

#[tokio::test]
async fn special_use_attributes_and_extended_list() {
    for_each_backend(|b| async move {
        let (mut c, _, _) = logged_in(&b).await;
        c.ok("CREATE Projects/2026").await;
        let l = c.ok("LIST \"\" \"*\"").await;
        for want in [
            "* LIST (\\HasNoChildren) \"/\" INBOX",
            "* LIST (\\HasNoChildren \\Sent) \"/\" Sent",
            "* LIST (\\HasNoChildren \\Drafts) \"/\" Drafts",
            "* LIST (\\HasNoChildren \\Trash) \"/\" Trash",
            "* LIST (\\HasNoChildren \\Junk) \"/\" Spam",
            "* LIST (\\HasChildren) \"/\" Projects",
        ] {
            assert!(l.iter().any(|x| x == want), "{want} in {l:?}");
        }
        // RETURN (SPECIAL-USE CHILDREN SUBSCRIBED) gives the same attributes plus \\Subscribed.
        let l = c.ok("LIST \"\" \"*\" RETURN (SUBSCRIBED CHILDREN SPECIAL-USE)").await;
        assert!(l.contains(&"* LIST (\\Subscribed \\HasNoChildren \\Sent) \"/\" Sent".to_owned()), "{l:?}");
        assert!(l.contains(&"* LIST (\\HasChildren) \"/\" Projects".to_owned()), "unsubscribed folders carry no \\Subscribed: {l:?}");
        // Selection by subscription, several patterns, no duplicates.
        c.ok("SUBSCRIBE Projects").await;
        let l = c.ok("LIST (SUBSCRIBED) \"\" (\"Projects*\" \"Sent\" \"Sent\")").await;
        assert_eq!(l, ["* LIST (\\Subscribed \\HasChildren) \"/\" Projects", "* LIST (\\Subscribed \\HasNoChildren \\Sent) \"/\" Sent"]);
        assert!(c.status("LIST (BOGUS) \"\" \"*\"").await.contains("BAD"));
        assert_eq!(c.ok("LIST \"\" \"\" RETURN (CHILDREN)").await, ["* LIST (\\Noselect) \"/\" \"\""]);
    })
    .await;
}

#[tokio::test]
async fn uidplus_append_copy_move_and_uid_expunge() {
    for_each_backend(|b| async move {
        let (mut c, ctx, acc) = logged_in(&b).await;
        let msg = message("one", "x");
        let uv = {
            let st = c.ok("STATUS Drafts (UIDVALIDITY)").await;
            st[0].split(['(', ')']).nth(1).unwrap().trim_start_matches("UIDVALIDITY ").to_owned()
        };
        // LITERAL+: no continuation is sent.
        c.send(&format!("a1 APPEND Drafts (\\Seen) {{{}+}}\r\n{msg}\r\n", msg.len())).await;
        let l = c.line().await;
        assert_eq!(l, format!("a1 OK [APPENDUID {uv} 1] APPEND completed"), "no `+` line first");
        let mb = b.mailbox(&ctx, acc).await;
        for s in ["a", "b", "c"] {
            deliver(&mb, 1, s, s).await;
        }
        c.ok("SELECT INBOX").await;
        let st = c.status("COPY 1:3 Drafts").await;
        assert!(st.contains(&format!("[COPYUID {uv} 1,2,3 2,3,4]")), "{st}");
        let inbox_uv = {
            let sel = c.ok("STATUS Trash (UIDVALIDITY)").await;
            sel[0].split(['(', ')']).nth(1).unwrap().trim_start_matches("UIDVALIDITY ").to_owned()
        };
        let lines = c.cmd("UID MOVE 2:3 Trash").await;
        assert_eq!(lines[0], "* 2 EXPUNGE", "{lines:?}");
        assert!(lines.last().unwrap().contains(&format!("[COPYUID {inbox_uv} 2,3 1,2]")), "{lines:?}");
        deliver(&mb, 1, "d", "d").await;
        assert_eq!(c.ok("NOOP").await, ["* 2 EXISTS"]);
        // UID EXPUNGE removes only the named \\Deleted messages.
        c.ok("STORE 1:* +FLAGS.SILENT (\\Deleted)").await;
        assert_eq!(c.ok("UID EXPUNGE 1").await, ["* 1 EXPUNGE"]);
        assert_eq!(c.ok("SEARCH DELETED").await, ["* SEARCH 1"], "the other \\Deleted message stays");
        assert!(c.status("UID EXPUNGE 1:*").await.contains("OK"));
    })
    .await;
}

#[tokio::test]
async fn append_honours_the_clients_internal_date() {
    for_each_backend(|b| async move {
        let (mut c, _, _) = logged_in(&b).await;
        let msg = message("dated", "x");
        c.send(&format!("a1 APPEND INBOX \"07-Mar-2019 08:30:15 +0200\" {{{}+}}\r\n{msg}\r\n", msg.len())).await;
        assert!(c.line().await.starts_with("a1 OK [APPENDUID"));
        c.ok("SELECT INBOX").await;
        assert_eq!(c.ok("FETCH 1 INTERNALDATE").await, ["* 1 FETCH (INTERNALDATE \"07-Mar-2019 06:30:15 +0000\")"]);
        assert_eq!(c.ok("SEARCH BEFORE 8-Mar-2019 SINCE 7-Mar-2019").await, ["* SEARCH 1"]);
        // Far in the past or future is bounded by the store: refused, nothing stored.
        let msg2 = message("ancient", "x");
        c.send(&format!("a2 APPEND INBOX \"01-Jan-2200 00:00:00 +0000\" {{{}+}}\r\n{msg2}\r\n", msg2.len())).await;
        assert!(c.line().await.starts_with("a2 NO"));
        assert_eq!(c.ok("SEARCH ALL").await, ["* SEARCH 1"]);
    })
    .await;
}
