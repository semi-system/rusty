# Sender policy: allow and block lists

Anti-spam in Rusty is rspamd and ClamAV (docs/design-filters-and-resolver.md). This adds the administrator's own word before them:

* **Lists.** Configuration keys `sender_allow` and `sender_block`: addresses (`boss@far.example`) or domains (`far.example`, which also covers its subdomains). They live in the layers every account resolves (account, plan, domain, global), so a domain administrator keeps its domain's lists in the Settings page and a platform administrator can set them on a plan or globally with the same keys.
* **Where they act.** `SenderPolicy` is the first inbound filter. If every recipient allows the sender, the scanners are skipped (`FilterVerdict::Bypass`); if every recipient blocks it, the message is refused at the end of DATA with `550 5.7.1`; otherwise the scanners decide as before. Allowing wins over blocking, so "block the whole domain, allow one person" works.
* **Why "every recipient".** After DATA a message cannot be refused for some recipients only. A message with one blocking and one non-blocking recipient is delivered to both (and goes through the scanners for the second).
* **Quarantine and release.** Mail rspamd classes as spam is delivered to the recipient's Spam folder; taking it out of there is the release. A virus is refused (`554`), never delivered. There is no separate quarantine store.
* **A database that cannot answer** leaves the sender unknown: the scanners still run.

Not done: score thresholds per plan (they are rspamd's own actions today) and a quarantine page for the administrator.

## Spam score lines per plan, and the quarantine view

* **Lines.** The configuration keys `spam_junk_score` and `spam_reject_score` (numbers, 0 to 100, junk below reject) in any layer, set per plan on the Plans page. When the first recipient's layers give either, rspamd's score is judged by them: from `spam_junk_score` the message is filed in the Spam folder and marked, from `spam_reject_score` it is refused (`550 5.7.1`), below both it is delivered as clean. A temporary refusal of rspamd (greylisting, soft reject) stays one. Without lines, rspamd's own actions decide as before. After DATA one message has one fate for all its recipients, so the first recipient's account decides.
* **Quarantine view.** Mail filed in the Spam folder is delivered, and the delivery log now says so: its detail is `quarantined (spam score N)`. The Outgoing mail page's trace has a switch "Only mail filed as spam" (`GET .../trace?held=1`). The release is the person's own: moving the message from Spam to the Inbox in the mail client (the administrator cannot read mail by design).

## The action on a virus, and blocked attachment types

* **Virus action.** The key `virus_action` (`reject`, the default, or `quarantine`), set on the Platform settings page for every account (any layer may carry it). `reject` refuses the message at the end of DATA with `550 5.7.1`; `quarantine` files it in the recipient's Spam folder with `X-Virus-Scan: FOUND`. The first recipient's account decides.
* **Blocked attachment types.** The key `blocked_extensions` (extensions without a dot, at most 50): on the Platform settings page for everyone and in a domain's Settings page for that domain (a domain administrator may keep its own). `AttachmentPolicy` runs right after the sender policy and before the scanners; a message with a part whose `filename=` or `name=` ends in a blocked extension is refused with `554 5.7.1 Attachments of type .exe are not accepted here`. Only the header lines of the parts are read (a line of a body names nothing), and the last extension decides (`Invoice.pdf.exe` is `exe`). An attachment inside an archive is not looked into.
