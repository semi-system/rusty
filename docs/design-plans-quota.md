# Plans and mailbox quotas

Built 2026-11-17 (P1 item 3 of `design-admin-parity.md`), first slice.

## What exists

* **Limits live in the configuration layers** the directory already had: account, plan, domain, global, first value wins (`Directory::effective_config`). Two keys are read: `quota_bytes` (the mailbox may hold at most this many bytes) and `max_message_bytes` (one message; it can only lower the configured size limit). No key means no quota. A plan is a named bundle of those keys (`plans.config`); an account is on at most one (`accounts.plan_id`).
* **Usage is a column,** `mailboxes.used_bytes` (migration 0032): the sum of `messages.size` of messages with `deleted_at IS NULL`, kept by statement triggers on `messages` (insert, delete, expunge-as-update), one update per mailbox per statement. The triggers are `SECURITY DEFINER` (the services hold no write on `mailboxes` beyond what they had) and are on the allow list of such functions in `pg_roles.rs`.
* **The check is in the write transaction.** `MailboxHub::open_for` reads the two keys (one query, `effective_configs`) and hands the quota to the `Mailbox`; `insert_messages` reads `used_bytes` right after the statement that locks the mailbox row, and refuses with `Error::Quota("mailbox is full")` when `used + new > quota`. So SMTP delivery, IMAP APPEND, submission's copy and the batch paths are all covered, with no race between two deliveries. SMTP answers `552 5.2.2` (the existing mapping of `Quota` at DATA); IMAP answers `NO [LIMIT]`.
* **Admin web:** the Plans page (platform administrators: make or change a plan, sizes in MB), a Plan select and a Mailbox column ("12 MB of 1 GB", a badge from 90 %) in the list of accounts. API: `GET/PUT /api/v1/plans`, `PUT /api/v1/accounts/{a}/plan`; the account JSON carries `plan`, `used_bytes`, `quota_bytes` (computed in one read for a page).

## Behaviour to know

* A mailbox already over a new, lower quota keeps its mail and receives nothing until it is under (nothing is deleted).
* A message that is only expunged (waiting for the purge) does not count; restoring it would.
* COPY inside a mailbox is not checked yet (it does not go through `insert_messages`).
* A change of plan or limits reaches the next mailbox open: the next delivery, the next IMAP session.

## Not done (next slices)

* The 90 % warning mail and deletion of a plan.
* A domain default (the domain layer exists and is honoured; no screen sets it), a quota per domain total, messages per hour, protocols allowed and password policy per plan.
* Caching the layers per mailbox open: today one indexed query per delivery.

## IMAP QUOTA (RFC 9208)

Built 2026-11-22. The IMAP service advertises `QUOTA` and answers `GETQUOTA` and `GETQUOTAROOT` (parsed by `rusty-imap/src/ext.rs`, since the grammar library has no QUOTA). A mailbox has one quota root, the empty name, with the `STORAGE` resource in KiB (rounded up): `* QUOTAROOT INBOX ""` and `* QUOTA "" (STORAGE 4 100)`. The used figure is read from `mailboxes.used_bytes` at the time of the command; the limit is the one the mailbox was opened with at sign-in (so a plan changed during a long session shows at the next sign-in, while the check at delivery always uses the current one). With no limit there is no root: `GETQUOTAROOT` names none and `GETQUOTA` answers `NO [NONEXISTENT]`. A client cannot set a quota (`SETQUOTA` is not offered). An `APPEND` that does not fit answers `NO [OVERQUOTA] Mailbox is full`; any other limit stays `[LIMIT]`.
