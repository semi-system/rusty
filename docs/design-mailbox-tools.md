# Deleted mail kept for recovery, and mbox export and import

## Deleted messages are kept for a while

A message the person deletes (IMAP `EXPUNGE`, a POP3 `DELE` at quit) is **soft-deleted**: the row gets `deleted_at`, the person no longer sees it and it stops counting against the quota, but the body stays. Until now nothing ever removed those rows or freed their bodies. The garbage collector now does, after `deleted_retention_days` (top-level setting, default 14, 1 to 3650): `Gc::purge_deleted`, which keeps a message that still has a `deliveries` row (the idempotency window of a delivery). Bodies nothing refers to any more are then freed by the blob collection of the same pass.

Upgrade note: the first pass after this version removes every deleted message that is older than the retention, including those deleted long before (they were being kept forever by accident). Raise `deleted_retention_days` first if that is not wanted.

Inside the window an administrator can recover one: `Mailbox::list_deleted` and `Mailbox::recover` (a new message in the chosen folder with the original internal date; the original stays and goes with the retention). The webmail will offer the same to the person.

## `rustyctl mailbox`

The administrator web has no access to mail (by design); these run where the blob stores are, like `blobs verify`.

| Command | What it does |
|---|---|
| `mailbox deleted ADDR` | the deleted messages still kept: id, when, from, folder, size, subject |
| `mailbox recover ADDR ID [FOLDER]` | bring one back as a new message (default: the Inbox) |
| `mailbox export ADDR DIR` | one `.mbox` file per folder into the new directory DIR (it must not exist: nothing is overwritten) |
| `mailbox import ADDR FILE [FOLDER]` | add the messages of an mbox file to a folder (default: the Inbox; made when missing); a message that cannot be stored is counted and skipped |

The format is mboxrd: a `From MAILER-DAEMON <date>` line before each message, a body line that starts with `From ` (after any `>`) gets one more `>` on the way out and loses one on the way in, LF in the file and CRLF in the mailbox. An import is an administrator's act: the account's quota does not apply to it (the message size limit does), and an imported message gets the time of the import as its internal date. A file is read whole and may be up to 1 GiB.

Not done: Maildir; keeping the original internal date and the flags of an imported message; an export that streams without holding a folder in memory; the same functions behind a button in the admin web (it cannot read mail) or in the webmail (it will).
