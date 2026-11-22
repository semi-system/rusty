# Sender policy: allow and block lists

Anti-spam in Rusty is rspamd and ClamAV (docs/design-filters-and-resolver.md). This adds the administrator's own word before them:

* **Lists.** Configuration keys `sender_allow` and `sender_block`: addresses (`boss@far.example`) or domains (`far.example`, which also covers its subdomains). They live in the layers every account resolves (account, plan, domain, global), so a domain administrator keeps its domain's lists in the Settings page and a platform administrator can set them on a plan or globally with the same keys.
* **Where they act.** `SenderPolicy` is the first inbound filter. If every recipient allows the sender, the scanners are skipped (`FilterVerdict::Bypass`); if every recipient blocks it, the message is refused at the end of DATA with `550 5.7.1`; otherwise the scanners decide as before. Allowing wins over blocking, so "block the whole domain, allow one person" works.
* **Why "every recipient".** After DATA a message cannot be refused for some recipients only. A message with one blocking and one non-blocking recipient is delivered to both (and goes through the scanners for the second).
* **Quarantine and release.** Mail rspamd classes as spam is delivered to the recipient's Spam folder; taking it out of there is the release. A virus is refused (`554`), never delivered. There is no separate quarantine store.
* **A database that cannot answer** leaves the sender unknown: the scanners still run.

Not done: score thresholds per plan (they are rspamd's own actions today) and a quarantine page for the administrator.
