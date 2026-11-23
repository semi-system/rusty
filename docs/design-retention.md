# Retention rules

A plan (or a domain, an account, or the global configuration: the same layers as every limit, the first that has the key wins) can say how long mail is kept. The garbage collector applies the rules on every pass.

* `spam_retention_days`: mail in the **Spam** folder older than this many days is expunged. Mail arrives in Spam when it is delivered, so its date is the day it was put there: this is the safe, common rule.
* `retention_days`: mail in **any folder** older than this many days is expunged: a policy rule (a company that keeps mail seven years and no longer), destructive on purpose, so it is empty unless set.
* Both are whole days from 1 to 36500; a value that is not a plain number in a stored configuration is no rule at all (the statement cannot fail on it). A `null` means "inherit": the layer below decides.
* The age is the message's **internal date** (when it arrived, or the date an import kept). It is not the day a message was moved to a folder, which the store does not record: that is why there is **no rule for the
  Trash folder** (an old message moved to Trash today would be gone at the next pass). Trash is emptied by hand or by the client.
* What happens to a message that is due: it is **expunged through its mailbox** (the change is logged, so an IMAP client sees an expunge and the counts follow), which is the same soft delete a client's EXPUNGE makes. The
  message is then kept for `deleted_retention_days` (default 30; the admin web's recovery and `rustyctl mailbox recover` bring it back) and only afterwards purged with its body.
* A pass works through up to 20 batches of 1000 messages and goes on at the next pass; a mailbox that is busy is left for the next one. A pass reads the messages of the accounts that have a rule through the four layers in one
  statement; a site with no rule anywhere pays for an empty result.
* **Where to set it**: the plan form of the admin web (Plans: "Empty the Spam folder after ..." and "Delete every message older than ..."), or `retention_days` and `spam_retention_days` in the configuration of a domain, an account,
  or the global one.
* Not here: archiving to a separate store and legal hold (D4 of `design-admin-parity.md`); a hold would have to be checked by this pass before it expunges.
