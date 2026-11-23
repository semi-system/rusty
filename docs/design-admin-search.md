# Search of the mail for platform administrators

A platform administrator can look in the mail of every mailbox (or of one domain or one account) for a message: who received it, in which folder, when, from whom, the subject and the size. **Never the text of a message.**
It is a privacy risk, so:

* **Off by default.** The platform setting `admin_search` (Platform settings, "Search of the mail") turns it on; with it off, `POST /api/v1/search` answers `409 wrong_state` and the page says to turn it on.
* **Platform administrators only** (a domain administrator and a helpdesk administrator get 403); at least one of: the sender contains, a recipient contains, the subject contains, from a date, to a date (texts of at most 200
  characters, case does not matter, `%` and `_` mean themselves). A domain or an address narrows it.
* **Every search is audited**: the request goes through the same wrapper as every change of the admin web, so the audit log has who asked, when, from which address and, since the audit keeps the fields of a body, **what for**
  (the texts and dates; the Audit page lists them).
* **The admin service never reads mail.** A search is a request in the database (`message_searches`, migration 0049) that the IMAP service, where the mailboxes are, runs (`SearchService`) and answers in the same row: at most 100
  results (newest first) as a JSON array, whether more matched, a state (`queued`, `running`, `done`, `failed`) and, for a failure, a fixed word (`failed`, `no_criteria`; never text from a mailbox). The query has a time limit of
  ten seconds. The admin web polls `GET /api/v1/search/{id}`; everything is removed a day after it was asked for. The admin role may insert and read the table and nothing of the messages; the IMAP role updates it.
* Expunged (deleted) messages are not found; the body is not searched (a full-text index is a later project of its own).
