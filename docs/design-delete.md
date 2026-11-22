# Deleting accounts and domains

Built 2026-11-17 (P1 item 4 of `design-admin-parity.md`).

* **Aliases** were already removable (`DELETE /api/v1/aliases/{a}`).
* **Closing and deleting an account.** Disabling an account is "closing" it: migration 0034 adds `accounts.closed_at`, set when the status becomes `disabled` and cleared when it becomes active or locked again. A closed account is removed **by itself** when `closed_at` is older than `closed_account_days` (top-level setting, default 30, 1 to 3650). An administrator can **delete it now** (`DELETE /api/v1/accounts/{a}`, the web asks first because it cannot be undone): that sets `closed_at = 0`, which is older than any retention, and the garbage collector removes it at its next pass. Only a closed account can be deleted (`409 not_closed`), and nobody can delete the account it is signed in with. A domain administrator may delete the closed accounts of its own domain.
* **What the removal does** (`GcRepo::purge_closed_account_batch`, called by the collector with up to 50 batches per pass): work on one due account at a time (`FOR UPDATE SKIP LOCKED`, so replicas do not collide); delete up to 1000 messages of its mailbox per transaction, so no statement runs long and a huge mailbox goes over several passes; when a batch finds fewer messages than the limit the mailbox is empty and the account is deleted, its mailbox, folders, aliases, capabilities and administrator grant going by cascade, and its address becoming free. The blobs the messages held lose their references (triggers) and are collected by the blob pass after the usual grace period. `rusty_gc` holds `DELETE` on `accounts` for this and nothing else new (a test of the privilege matrix pins it).
* **Deleting a domain** (`DELETE /api/v1/domains/{d}`, platform administrators): only a domain that holds nothing: no account, alias or list, no queued or running job, no blob store of its own (`409 not_empty` otherwise). Its finished jobs, DNS settings, DKIM keys and relay go with it. The row is locked first, so nothing is added between the check and the delete.

## Not done

* Restoring a closed account's mail inside the retention (reopening it is enough: the mail is untouched until the removal) and a screen listing what is due for removal and when.
* Deleting a domain that still has closed accounts waiting for their retention: delete the accounts first (now), wait for the pass, then the domain.
* Deleting the blob store of a tenant.
