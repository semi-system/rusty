# Moving a mailbox in from another IMAP server

A platform administrator can pull the mail of a mailbox on another IMAP server into an account here (Accounts, "Move in"): every folder, with the date and the flags of each message. Until now `imapsync` was the documented way.

* **Only reads over there.** The client (`rusty-svc/src/imap_pull.rs`) signs in with `LOGIN`, lists the folders (`LIST`), opens each with `EXAMINE` (read-only), lists its UIDs and reads each message with `UID FETCH ... BODY.PEEK[]`. Nothing is
  marked as read, deleted or moved on the other server. TLS from the first byte, or `STARTTLS` before the password is sent; the certificate is checked against the host's store. "No encryption" is an explicit choice that the page warns about.
* **The admin service never reads mail, and never reads the password back.** The administrator gives the server, the login and the password; the admin service creates the migration (`imap_migrations`, migration 0050, state `preparing`),
  seals the password as the sealed secret `migration/<id>/password` and then queues it. The IMAP service, where the mailboxes are (`MigrationService`, with the `[unsealing]` key), claims it, opens the secret and copies the mail.
  A migration is removed two days after it was asked for (the garbage collector), and the admin service then removes its sealed secret (no service that works on the mail may write a sealed secret); the audit log keeps who asked and the host and login, and hides the password like every secret.
* **Folders**: a folder with a special-use flag (`\Sent`, `\Trash`, `\Drafts`, `\Junk`, `\Archive`) or the name `INBOX` goes to the folder of that role here; any other folder gets a folder of its name (a `/` in it becomes a dot, a name here has no
  slash). Messages flagged `\Deleted` are not copied. Flags `\Seen`, `\Flagged`, `\Answered`, `\Draft` and the internal date are kept (a message without a readable date gets the day of the move).
* **Progress and control**: the worker writes the folder it is on, the folders done and the messages, skipped and bytes copied every 20 messages; the page refreshes every two seconds while a move goes on. Stopping a waiting move cancels it at
  once; stopping a running one is seen by the worker at its next report. One move at a time per account (a second would copy the same mail again). A move whose worker stops is marked failed after 15 minutes without a report.
* **Failures are fixed words** (`connect`, `tls`, `auth`, `protocol`, `timeout`, `quota`, `too_large`, `failed`), never text from the other server. A full mailbox ends the move with `quota`; what was copied stays. A message the other server
  refuses (removed meanwhile) is counted as skipped, a broken connection ends the move.
* **Limits**: a message of more than 64 MiB is skipped; every read has a 60 second limit. There is no resume: running a move again copies everything again (a duplicate check by `Message-ID` is a later improvement).
* Platform administrators only, like the export and import of a mailbox.
