//! The `mailbox` commands (docs/design-mailbox-tools.md): deleted messages are listed and recovered, a mailbox is exported as mbox files and an mbox file is imported.

mod common;

use common::*;
use rusty_ctl::{Command, PasswordSource};
use rusty_store::Mailbox;

const ONE: &[u8] = b"From: a@far.example\r\nTo: alice@example.org\r\nSubject: first\r\n\r\nFrom the start of a line\r\n";
const TWO: &[u8] = b"From: b@far.example\r\nTo: alice@example.org\r\nSubject: second\r\n\r\nbody two\r\n";

async fn account(b: &Backend, addr: &str) -> rusty_core::AccountId {
    b.run(Command::AccountAdd { addr: addr.into(), password: PasswordSource::Stdin }, &["a-long-password-1"]).await.unwrap();
    b.dir.find_account(addr).await.unwrap().unwrap().id
}

#[tokio::test]
async fn deleted_mail_is_listed_and_recovered_and_a_mailbox_goes_out_and_comes_in_as_mbox() {
    for_each_backend(|b| async move {
        b.run(Command::DomainAdd { name: "example.org".into() }, &[]).await.unwrap();
        let alice = account(&b, "alice@example.org").await;
        let bob = account(&b, "bob@example.org").await;
        let mail = b.mail.clone().unwrap();
        let mb = Mailbox::open_with(mail.repo.clone(), mail.stores.clone(), alice).await.unwrap();
        let inbox = mb.list_folders().await.unwrap().into_iter().find(|f| f.role == "inbox").unwrap().id;
        let first = mb.append_dated(inbox, bytes::Bytes::from_static(ONE), rusty_core::Flags::default(), 1_700_000_000).await.unwrap();
        mb.append(inbox, bytes::Bytes::from_static(TWO), rusty_core::Flags::default()).await.unwrap();
        mb.expunge(first.id).await.unwrap();

        // The deleted message is listed, with what the owner knew of it.
        let listed = b.run(Command::MailboxDeleted { addr: "alice@example.org".into() }, &[]).await.unwrap();
        assert!(listed.text.contains(&format!("{}  deleted", first.id.0)) && listed.text.contains("first") && listed.text.contains("a@far.example"), "{}", listed.text);
        assert_eq!(listed.json["messages"].as_array().unwrap().len(), 1);
        // Recovering brings it back as a new message with its date, and a live message cannot be recovered.
        let got = b.run(Command::MailboxRecover { addr: "alice@example.org".into(), id: first.id.0, folder: None }, &[]).await.unwrap();
        assert!(got.text.starts_with("recovered message"), "{}", got.text);
        let live = mb.list_folder(inbox, 0, 10).await.unwrap();
        assert_eq!(live.len(), 2);
        assert!(live.iter().any(|m| m.subject == "first" && m.internal_date == 1_700_000_000));
        let again = b.run(Command::MailboxRecover { addr: "alice@example.org".into(), id: live.iter().find(|m| m.subject == "second").unwrap().id.0, folder: None }, &[]).await;
        assert!(again.is_err(), "a live message is not a deleted one");
        assert!(b.run(Command::MailboxRecover { addr: "alice@example.org".into(), id: first.id.0, folder: Some("nowhere".into()) }, &[]).await.is_err());

        // Export: one mbox per folder, into a directory that must not exist; the quoting of a line that starts with `From ` is there.
        let dir = b.tmp.path().join("export");
        let out = b.run(Command::MailboxExport { addr: "alice@example.org".into(), dir: dir.clone() }, &[]).await.unwrap();
        assert_eq!(out.json["messages"], 2, "{}", out.text);
        let inbox_file = std::fs::read_to_string(dir.join("Inbox.mbox")).unwrap();
        assert_eq!(inbox_file.matches("\nFrom MAILER-DAEMON ").count() + usize::from(inbox_file.starts_with("From MAILER-DAEMON ")), 2, "{inbox_file}");
        assert!(inbox_file.contains("\n>From the start of a line\n"), "{inbox_file}");
        assert!(b.run(Command::MailboxExport { addr: "alice@example.org".into(), dir: dir.clone() }, &[]).await.is_err(), "nothing is overwritten");

        // Import into another account's folder (made when missing): the same two messages, byte for byte but for the line ends.
        let done = b.run(Command::MailboxImport { addr: "bob@example.org".into(), file: dir.join("Inbox.mbox"), folder: Some("Archive".into()) }, &[]).await.unwrap();
        assert_eq!(done.json["imported"], 2, "{}", done.text);
        let bmb = Mailbox::open_with(mail.repo.clone(), mail.stores.clone(), bob).await.unwrap();
        let archive = bmb.list_folders().await.unwrap().into_iter().find(|f| f.name == "Archive").expect("the folder was made").id;
        let moved = bmb.list_folder(archive, 0, 10).await.unwrap();
        assert_eq!(moved.len(), 2);
        let raw = bmb.get_raw(moved.iter().find(|m| m.subject == "first").unwrap().id).await.unwrap();
        assert_eq!(&raw[..], ONE, "the message is what it was, with its From line unquoted");
        // Something that is not an mbox imports nothing.
        let junk = b.tmp.path().join("junk.txt");
        std::fs::write(&junk, "hello\n").unwrap();
        let none = b.run(Command::MailboxImport { addr: "bob@example.org".into(), file: junk, folder: None }, &[]).await.unwrap();
        assert_eq!(none.json["imported"], 0);
    })
    .await;
}
