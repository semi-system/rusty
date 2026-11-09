//! Folder and message commands against a real handler, on every backend.

mod common;

use common::*;
use rusty_imap::Limits;

async fn logged_in(b: &Backend, user: &str) -> (Conn, rusty_svc::Context) {
    let ctx = b.ctx().await;
    let mut c = Conn::open(handler(ctx.clone()), true);
    c.line().await;
    c.login(user, "pw").await;
    (c, ctx)
}

#[tokio::test]
async fn list_lsub_create_rename_delete_and_subscribe() {
    for_each_backend(|b| async move {
        b.account("alice@a.test", "pw").await;
        let (mut c, _) = logged_in(&b, "alice@a.test").await;
        let l = c.ok("LIST \"\" \"*\"").await;
        assert_eq!(l[0], "* LIST (\\HasNoChildren) \"/\" INBOX");
        assert!(l.contains(&"* LIST (\\HasNoChildren) \"/\" Sent".to_owned()), "{l:?}");
        assert_eq!(c.ok("LIST \"\" \"\"").await, ["* LIST (\\Noselect) \"/\" \"\""]);
        assert_eq!(c.ok("LIST \"\" \"inbox\"").await.len(), 1, "INBOX is case-insensitive");
        // CREATE builds intermediate folders; names are UTF-7 on the wire.
        c.ok("CREATE \"Projects/2026/Q1\"").await;
        c.ok("CREATE \"&AREA4w-\"").await;
        assert!(c.status("CREATE Projects").await.contains("NO [ALREADYEXISTS]"));
        assert!(c.status("CREATE inbox").await.contains("NO [ALREADYEXISTS]"));
        let l = c.ok("LIST \"\" \"Projects*\"").await;
        assert_eq!(l, ["* LIST (\\HasChildren) \"/\" Projects", "* LIST (\\HasChildren) \"/\" Projects/2026", "* LIST (\\HasNoChildren) \"/\" Projects/2026/Q1"]);
        assert_eq!(c.ok("LIST \"\" \"%\"").await.len(), 7, "five system folders, Projects and the UTF-7 one");
        // New folders are not subscribed; system folders are.
        let sub = c.ok("LSUB \"\" \"*\"").await;
        assert!(sub.iter().all(|x| !x.contains("Projects")) && sub.len() == 5, "{sub:?}");
        c.ok("SUBSCRIBE Projects/2026").await;
        assert!(c.ok("LSUB \"\" \"*\"").await.iter().any(|x| x.ends_with("Projects/2026")));
        c.ok("UNSUBSCRIBE Projects/2026").await;
        assert!(c.status("SUBSCRIBE Nope").await.contains("NO [NONEXISTENT]"));
        // RENAME moves and renames; system folders are protected.
        c.ok("RENAME Projects/2026/Q1 Archive2/Q1").await;
        assert!(c.ok("LIST \"\" \"Archive2/*\"").await.len() == 1);
        assert!(c.status("RENAME Sent Sent2").await.contains("NO [CANNOT]"));
        assert!(c.status("DELETE INBOX").await.contains("NO [CANNOT]"));
        assert!(c.status("DELETE Projects").await.contains("NO [HASCHILDREN]"));
        assert!(c.status("RENAME Projects Projects/2026").await.contains("NO"), "into its own subtree");
        c.ok("DELETE Archive2/Q1").await;
        c.ok("DELETE Archive2").await;
        assert!(c.status("DELETE Archive2").await.contains("NO [NONEXISTENT]"));
    })
    .await;
}

#[tokio::test]
async fn select_status_examine_and_a_failed_select_deselects() {
    for_each_backend(|b| async move {
        let acc = b.account("alice@a.test", "pw").await;
        let (mut c, ctx) = logged_in(&b, "alice@a.test").await;
        let mb = b.mailbox(&ctx, acc).await;
        deliver(&mb, 1, "one", "x").await;
        let m2 = deliver(&mb, 1, "two", "y").await;
        mb.set_flags(m2.id, rusty_core::Flags::SEEN).await.unwrap();
        let s = c.cmd("SELECT INBOX").await;
        assert_eq!(s[0], "* FLAGS (\\Seen \\Flagged \\Answered \\Deleted \\Draft)");
        assert!(s[1].starts_with("* OK [PERMANENTFLAGS (") && s[1].contains("\\*)"));
        assert_eq!(s[2], "* 2 EXISTS");
        assert_eq!(s[3], "* 0 RECENT");
        assert_eq!(s[4], "* OK [UNSEEN 1] First unseen message");
        assert!(s[5].starts_with("* OK [UIDVALIDITY ") && s[6] == "* OK [UIDNEXT 3] Predicted next UID", "{s:?}");
        assert!(s[7].starts_with("t2 OK [READ-WRITE]"), "{s:?}");
        let st = c.ok("STATUS INBOX (MESSAGES UNSEEN UIDNEXT RECENT)").await;
        assert_eq!(st, ["* STATUS INBOX (MESSAGES 2 UNSEEN 1 UIDNEXT 3 RECENT 0)"]);
        assert!(c.status("EXAMINE INBOX").await.contains("OK [READ-ONLY]"));
        assert!(c.status("STORE 1 +FLAGS (\\Seen)").await.contains("NO [READ-ONLY]"));
        assert!(c.status("SELECT Nope").await.contains("NO [NONEXISTENT]"));
        assert!(c.status("FETCH 1 FLAGS").await.contains("BAD"), "a failed SELECT leaves nothing selected");
    })
    .await;
}

#[tokio::test]
async fn fetch_items_sections_and_partials() {
    for_each_backend(|b| async move {
        let acc = b.account("alice@a.test", "pw").await;
        let (mut c, ctx) = logged_in(&b, "alice@a.test").await;
        let mb = b.mailbox(&ctx, acc).await;
        let r = deliver(&mb, 1, "hello", "line one").await;
        let raw = message("hello", "line one");
        c.ok("SELECT INBOX").await;
        let l = c.ok("FETCH 1 (UID FLAGS RFC822.SIZE)").await;
        assert_eq!(l, [format!("* 1 FETCH (UID {} FLAGS () RFC822.SIZE {})", r.uid, raw.len())]);
        let l = c.ok("FETCH 1 (INTERNALDATE ENVELOPE)").await;
        assert!(l[0].starts_with("* 1 FETCH (INTERNALDATE \"") && l[0].contains("ENVELOPE (\"Fri, 06 Nov 2026 10:00:00 +0000\" \"hello\""), "{l:?}");
        let l = c.ok("FETCH 1 BODYSTRUCTURE").await;
        assert!(l[0].contains("BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 10 1 NIL NIL NIL NIL)"), "{l:?}");
        // BODY.PEEK leaves the flags alone, BODY[] sets \Seen and reports it.
        let l = c.ok("FETCH 1 BODY.PEEK[TEXT]<0.4>").await;
        assert_eq!(l, ["* 1 FETCH (BODY[TEXT]<0> \"line\")"]);
        assert_eq!(c.ok("FETCH 1 FLAGS").await, ["* 1 FETCH (FLAGS ())"]);
        let l = c.ok("FETCH 1 BODY[HEADER.FIELDS (Subject)]").await;
        assert_eq!(l[0], "* 1 FETCH (BODY[HEADER.FIELDS (Subject)] {18}\r\nSubject: hello\r\n\r\n FLAGS (\\Seen))", "{l:?}");
        let l = c.ok("FETCH 1 RFC822").await;
        assert!(l[0].starts_with(&format!("* 1 FETCH (RFC822 {{{}}}\r\nFrom: Sender", raw.len())), "{l:?}");
        assert!(c.ok("FETCH 1 BODY[9]").await[0].contains("BODY[9] NIL"));
        // Unknown uids give no data and no error; the UID is always included.
        assert!(c.ok("UID FETCH 999999 FLAGS").await.is_empty());
        let l = c.ok(&format!("UID FETCH {} (FLAGS)", r.uid)).await;
        assert_eq!(l, [format!("* 1 FETCH (UID {} FLAGS (\\Seen))", r.uid)]);
        // Macros.
        assert!(c.ok("FETCH 1 FAST").await[0].contains("RFC822.SIZE"));
    })
    .await;
}

#[tokio::test]
async fn store_flags_keywords_and_silent() {
    for_each_backend(|b| async move {
        let acc = b.account("alice@a.test", "pw").await;
        let (mut c, ctx) = logged_in(&b, "alice@a.test").await;
        let mb = b.mailbox(&ctx, acc).await;
        for s in ["a", "b", "c"] {
            deliver(&mb, 1, s, s).await;
        }
        c.ok("SELECT INBOX").await;
        assert_eq!(c.ok("STORE 1:2 +FLAGS (\\Seen $label1)").await, ["* 1 FETCH (FLAGS (\\Seen $label1))", "* 2 FETCH (FLAGS (\\Seen $label1))"]);
        assert!(c.ok("STORE 1 +FLAGS.SILENT (\\Flagged)").await.is_empty());
        assert_eq!(c.ok("STORE 1 -FLAGS ($label1)").await, ["* 1 FETCH (FLAGS (\\Seen \\Flagged))"]);
        assert_eq!(c.ok("STORE 2 FLAGS (\\Draft work)").await, ["* 2 FETCH (FLAGS (\\Draft work))"]);
        assert_eq!(c.ok("UID STORE 3 +FLAGS (\\Answered)").await, ["* 3 FETCH (UID 3 FLAGS (\\Answered))"]);
        assert_eq!(c.ok("FETCH 1:3 FLAGS").await, ["* 1 FETCH (FLAGS (\\Seen \\Flagged))", "* 2 FETCH (FLAGS (\\Draft work))", "* 3 FETCH (FLAGS (\\Answered))"]);
        assert!(c.status("STORE 1 +FLAGS (bad\"kw)").await.contains("BAD"));
    })
    .await;
}

#[tokio::test]
async fn expunge_close_and_unselect() {
    for_each_backend(|b| async move {
        let acc = b.account("alice@a.test", "pw").await;
        let (mut c, ctx) = logged_in(&b, "alice@a.test").await;
        let mb = b.mailbox(&ctx, acc).await;
        for s in ["a", "b", "c", "d"] {
            deliver(&mb, 1, s, s).await;
        }
        c.ok("SELECT INBOX").await;
        c.ok("STORE 1,3 +FLAGS.SILENT (\\Deleted)").await;
        assert_eq!(c.ok("EXPUNGE").await, ["* 1 EXPUNGE", "* 2 EXPUNGE"], "numbers are valid at each point of the sequence");
        assert_eq!(c.ok("SEARCH ALL").await, ["* SEARCH 1 2"]);
        c.ok("STORE 1 +FLAGS.SILENT (\\Deleted)").await;
        c.ok("CLOSE").await;
        assert!(c.status("EXPUNGE").await.contains("BAD"));
        assert_eq!(c.ok("STATUS INBOX (MESSAGES)").await, ["* STATUS INBOX (MESSAGES 1)"]);
        c.ok("SELECT INBOX").await;
        c.ok("UNSELECT").await;
        assert!(c.status("NOOP").await.contains("OK"));
        assert!(c.status("FETCH 1 FLAGS").await.contains("BAD"));
    })
    .await;
}

#[tokio::test]
async fn another_session_is_seen_at_the_next_command() {
    for_each_backend(|b| async move {
        let acc = b.account("alice@a.test", "pw").await;
        let ctx = b.ctx().await;
        let mk = |ctx: &rusty_svc::Context| Conn::open(handler(ctx.clone()), true);
        let (mut one, mut two) = (mk(&ctx), mk(&ctx));
        for c in [&mut one, &mut two] {
            c.line().await;
            c.login("alice@a.test", "pw").await;
        }
        let mb = b.mailbox(&ctx, acc).await;
        for s in ["a", "b", "c"] {
            deliver(&mb, 1, s, s).await;
        }
        one.ok("SELECT INBOX").await;
        two.ok("SELECT INBOX").await;
        // A delivery shows up as EXISTS.
        deliver(&mb, 1, "d", "d").await;
        assert_eq!(one.ok("NOOP").await, ["* 4 EXISTS"]);
        // Session two flags and expunges; session one hears about flags, then the expunge.
        two.ok("STORE 2 +FLAGS.SILENT (\\Seen)").await;
        assert_eq!(one.ok("NOOP").await, ["* 2 FETCH (UID 2 FLAGS (\\Seen))"]);
        two.ok("STORE 1 +FLAGS.SILENT (\\Deleted)").await;
        two.ok("EXPUNGE").await;
        // Not during a plain FETCH: the sequence numbers must stay valid for it ...
        assert_eq!(one.ok("FETCH 1 UID").await, ["* 1 FETCH (UID 1)"].map(String::from), "the expunge is held back");
        // ... but at NOOP it is reported, and numbers shift.
        assert_eq!(one.ok("NOOP").await, ["* 1 EXPUNGE"]);
        assert_eq!(one.ok("FETCH 1 UID").await, ["* 1 FETCH (UID 2)"]);
        // A UID command may carry the expunge notice.
        deliver(&mb, 1, "e", "e").await;
        two.ok("STORE 1 +FLAGS.SILENT (\\Deleted)").await;
        two.ok("EXPUNGE").await;
        let l = one.ok("UID FETCH 3 UID").await;
        assert_eq!(l, ["* 2 FETCH (UID 3)", "* 1 EXPUNGE", "* 3 EXISTS"]);
    })
    .await;
}

#[tokio::test]
async fn search_decides_by_flags_dates_headers_and_body() {
    for_each_backend(|b| async move {
        let acc = b.account("alice@a.test", "pw").await;
        let (mut c, ctx) = logged_in(&b, "alice@a.test").await;
        let mb = b.mailbox(&ctx, acc).await;
        deliver(&mb, 1, "invoice", "please pay the invoice").await;
        deliver(&mb, 1, "lunch", "noon at the cafe").await;
        c.ok("SELECT INBOX").await;
        c.ok("STORE 2 +FLAGS.SILENT (\\Seen $x)").await;
        assert_eq!(c.ok("SEARCH UNSEEN").await, ["* SEARCH 1"]);
        assert_eq!(c.ok("SEARCH SUBJECT LUNCH").await, ["* SEARCH 2"]);
        assert_eq!(c.ok("SEARCH BODY cafe").await, ["* SEARCH 2"]);
        assert_eq!(c.ok("UID SEARCH OR FROM nobody BODY invoice").await, ["* SEARCH 1"]);
        assert_eq!(c.ok("SEARCH KEYWORD $x").await, ["* SEARCH 2"]);
        assert_eq!(c.ok("SEARCH NOT SEEN HEADER Message-ID invoice").await, ["* SEARCH 1"]);
        assert_eq!(c.ok("SEARCH SENTSINCE 6-Nov-2026 SENTBEFORE 7-Nov-2026").await, ["* SEARCH 1 2"]);
        assert_eq!(c.ok("SEARCH LARGER 100000").await, ["* SEARCH"]);
        assert!(c.status("SEARCH CHARSET KOI8-R ALL").await.contains("NO [BADCHARSET"));
    })
    .await;
}

#[tokio::test]
async fn an_expensive_search_fails_closed() {
    for_each_backend(|b| async move {
        let acc = b.account("alice@a.test", "pw").await;
        let ctx = b.ctx().await;
        let limits = Limits { search_scan_messages: 1, ..Limits::default() };
        let mut c = Conn::open(handler_with(ctx.clone(), limits), true);
        c.line().await;
        c.login("alice@a.test", "pw").await;
        let mb = b.mailbox(&ctx, acc).await;
        deliver(&mb, 1, "a", "a").await;
        deliver(&mb, 1, "b", "b").await;
        c.ok("SELECT INBOX").await;
        let lines = c.cmd("SEARCH BODY zzz").await;
        assert_eq!(lines.len(), 1, "no partial answer: {lines:?}");
        assert!(lines[0].contains("NO [LIMIT]"), "{lines:?}");
        assert_eq!(c.ok("SEARCH UNSEEN").await, ["* SEARCH 1 2"], "cheap searches are unaffected");
    })
    .await;
}

#[tokio::test]
async fn append_copy_and_move() {
    for_each_backend(|b| async move {
        let acc = b.account("alice@a.test", "pw").await;
        let ctx = b.ctx_with(|c| c.max_message_bytes = 4096).await;
        let mut c = Conn::open(handler(ctx.clone()), true);
        c.line().await;
        c.login("alice@a.test", "pw").await;
        let msg = message("drafted", "text");
        // Synchronising literal.
        c.send(&format!("a1 APPEND Drafts (\\Seen \\Draft proj) {{{}}}\r\n", msg.len())).await;
        assert_eq!(c.line().await, "+ Ready for literal data");
        c.send(&format!("{msg}\r\n")).await;
        assert!(c.line().await.starts_with("a1 OK"));
        let st = c.ok("STATUS Drafts (MESSAGES UNSEEN)").await;
        assert_eq!(st, ["* STATUS Drafts (MESSAGES 1 UNSEEN 0)"]);
        assert!(c.status("APPEND Nope {1+}\r\nx").await.contains("NO [TRYCREATE]"));
        // Over the size limit: refused before the data is sent.
        c.send("a2 APPEND Drafts {5000}\r\n").await;
        let l = c.line().await;
        assert!(l.starts_with("a2 NO [LIMIT]"), "{l}");
        // COPY keeps the original; MOVE removes it and reports the expunge.
        c.ok("SELECT Drafts").await;
        c.ok("COPY 1 INBOX").await;
        assert!(c.status("COPY 1 Missing").await.contains("NO [TRYCREATE]"));
        assert_eq!(c.ok("SEARCH ALL").await, ["* SEARCH 1"]);
        assert_eq!(c.ok("MOVE 1 Trash").await, ["* 1 EXPUNGE"]);
        assert_eq!(c.ok("SEARCH ALL").await, ["* SEARCH"]);
        let mb = b.mailbox(&ctx, acc).await;
        assert_eq!(mb.folder_counts(rusty_core::FolderId(1)).await.unwrap().messages, 1);
        assert_eq!(mb.folder_counts(rusty_core::FolderId(4)).await.unwrap().messages, 1);
        c.ok("SELECT INBOX").await;
        assert!(c.ok("FETCH 1 FLAGS").await[0].contains("\\Seen \\Draft proj"), "flags and keywords travel with a copy");
    })
    .await;
}

#[tokio::test]
async fn one_account_cannot_reach_anothers_mailbox() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("a.test").await.unwrap();
        let alice = b.account_in(dom, "alice", "pw").await;
        b.account_in(dom, "bob", "pw").await;
        let ctx = b.ctx().await;
        let mb = b.mailbox(&ctx, alice).await;
        let f = mb.create_folder(None, "Secret").await.unwrap();
        mb.append(f, bytes::Bytes::from(message("private", "x")), rusty_core::Flags::empty()).await.unwrap();
        let mut c = Conn::open(handler(ctx), true);
        c.line().await;
        c.login("bob@a.test", "pw").await;
        assert!(c.ok("LIST \"\" \"*\"").await.iter().all(|l| !l.contains("Secret")));
        assert!(c.status("SELECT Secret").await.contains("NO [NONEXISTENT]"));
        assert!(c.status("STATUS Secret (MESSAGES)").await.contains("NO [NONEXISTENT]"));
        c.ok("SELECT INBOX").await;
        assert!(c.ok("UID FETCH 1:* FLAGS").await.is_empty());
        assert!(c.status("COPY 1 Secret").await.contains("NO [TRYCREATE]"));
        assert!(c.status("APPEND Secret {1+}\r\nx").await.contains("TRYCREATE"));
    })
    .await;
}
