# Design: the admin console compared with Zimbra, and the plan to close the gap

Status: **proposed 2026-11-17** (the owner asked for a comparison with Zimbra and for its functions to be completed). Written before any code, as the project requires; the phases below are built as series of small pull
requests, each with its tests, its entries in the message catalogue (`web/admin/src/i18n`), its contract change (`contract/v1.json`) and, where it reads or writes the database, its query shape and budget.

## 1. How the comparison was made

The reference is the Zimbra Open Source Edition admin console (Zimbra 10: Home, Monitor, Manage, Configure, Tools) and what an administrator does with it. A function that exists only in Zimbra's paid edition is marked
*(NE)*. For each function: what Rusty has **today**, and what the **plan** is: a phase, a decision for the owner (section 4), or "no" with the reason. The admin web already covers its first-version scope
(`design-admin-web.md`, section 1); this document is everything beyond.

## 2. Function by function

### Monitor

| Zimbra | Rusty today | Plan |
|---|---|---|
| Server status (services up or down) | Monitor page: every service and replica from the heartbeats, versions, queue, storage checks, DNS problems | done |
| Mail queue: list, search, hold, requeue, delete, flush | the length of the outbound queue and the age of the oldest message | **P1** queue manager |
| Message count and statistics (per day, per domain) | `/metrics` counters (connections, scans, auth), no history in the web | **P2** statistics page over a small `message_stats` table |
| Message trace (what happened to a message) | none | **P1** delivery log (retention 30 days) and a trace search |
| Server statistics (CPU, disk, load) | `node_exporter` for the operator's own Prometheus; none in the web | no: it is the operator's monitoring (`design-metrics.md`) |
| Logs viewer | `rusty-swarm logs`, `docker logs` | no for service logs; **P2** audit log of admin actions in the web |

### Manage: accounts

| Zimbra | Rusty today | Plan |
|---|---|---|
| Account list with search, filter, paging, sort | a plain list per domain | **P1** search by address, status and plan; paging; sort |
| Bulk create from CSV, bulk status change | none | **P1** CSV import (dry run, then apply; a report of what was refused) |
| Account status: active, locked out, closed, maintenance | active and disabled | **P1** `active`, `locked` (login refused, mail still delivered), `closed` (nothing in or out, kept until purged), the same three in the web, the CLI and the directory |
| Delete an account | none (only disable) | **P1** purge of a closed account after a delay (default 30 days), its mailbox and blobs removed by the garbage collector |
| Set password, force change at next login | set password | **P1** `must_change_password` (the web portal and IMAP both honour it) |
| Password policy: length, complexity, history, expiry; lockout after N failures | a length rule and a login throttle | **P1** per-plan policy (length, complexity, history, expiry) and the lockout of the throttle shown and clearable |
| Aliases | one alias to one account | **P1** an alias may name several accounts (a list); remove, rename |
| Distribution lists (members, nested lists) | the tables `lists` and `list_members` exist; no CLI, no web, no routing test in the UI | **P1** lists in the web and the CLI: create, members (accounts or outside addresses), delivery to all, "send as the list" rule for members only |
| Catch-all address | none | **P1** per domain, to one account or list |
| Resources (rooms, equipment) | no | tied to a calendar: decision D1 |
| Class of Service | `plans` table (name and JSON), nothing reads it | **P1** plans: mailbox quota, maximum message size, messages per hour, protocols allowed (IMAP, submission), password policy; a plan per domain as the default and one per account |
| Quota and usage per mailbox | none | **P1** usage counters per mailbox (a column kept by the store transaction), soft limit (warn at 90 %), hard limit (a message that does not fit is refused with `552 5.2.2` at the SMTP transaction, and an IMAP `QUOTA` extension answer) |
| Per-account features: forwarding, vacation, signatures, filters | none | **P3** (users' own features, below) |
| Delegated administrators | two levels: platform and one domain; granting is in the CLI only | **P1** the web page; **P2** a third level `helpdesk` (a domain's accounts: reset password, unlock, view; nothing else) |
| Mailbox export, import, move; recover deleted items (dumpster) | `backup.sh` for the whole database | **P2** export and import of one mailbox (mbox/Maildir), and a "deleted items" folder with a retention time |
| Account activity: last login, last delivery | none | **P1** `last_login_at` and `last_delivery_at` on the account page |

### Manage: domains

| Zimbra | Rusty today | Plan |
|---|---|---|
| Create, disable | yes | done |
| Delete | none | **P1** delete of a closed, empty domain (no accounts, lists or aliases) |
| Domain aliases (`alias.example` delivers as `example`) | none | **P1** |
| Default plan, maximum accounts, default quota | none | **P1** with the plans |
| Mail footer / disclaimer, signature policy | none | **P2** appended to outbound mail of the domain |
| DKIM | generate | **P2** rotate (a new selector, the old kept until the TTL has passed), several selectors shown |
| Relay per domain | yes | done |
| Storage per domain (S3, pools) | yes | done (beyond Zimbra) |
| Authentication per domain (LDAP, AD, SSO) | none | deferred by the owner (SSO), D3 |
| Global address list | none | **P3** with the webmail (the directory search a client needs) |

### Configure

| Zimbra | Rusty today | Plan |
|---|---|---|
| Global settings: maximum message size, attachment blocking, banner, rate limits, RBLs, greylisting | configuration files and the swarm inventory | **P2** a platform-settings table with hot reload and an admin page (the same keys as the files; the file wins at start, the page changes the running value) |
| Anti-spam: thresholds, allow and block lists, quarantine, release | none (rspamd defaults) | **P2** thresholds per plan, allow/block per domain, a quarantine folder and a release action |
| Anti-virus settings | ClamAV on or off | **P2** the action on a virus (reject or quarantine) |
| Certificates: view, install, CSR | the edge's ACME; `certs sync` | **P2** the certificates page: hosts, expiry, issuer, the next renewal; manual upload for a deployment without ACME |
| Servers and services | deployment (`rusty-swarm`) | no: the tool and the Monitor page |
| Retention and archiving, legal hold *(NE)* | none | **P3** a retention rule per plan; archiving and legal hold are D4 |
| Backup and restore *(NE)* | `backup`, `restore` in the deploy tools | **P3** a status page (the last backup, its size, its age) from the tool's report; the action stays in the tool |
| Software updates | `rusty-swarm upgrade` | no in the web; the Monitor shows the versions that run |
| Zimlets, admin extensions | no | no: Zimlets are a Zimbra web-client technology |

### Tools

| Zimbra | Rusty today | Plan |
|---|---|---|
| Search mail across mailboxes (admin) | none | **P3**, platform administrators only, every search written to the audit log, off by default (a privacy risk: D5) |
| Migration wizard | none | **P3** an IMAP pull job (host, login, folder map) per account, with progress; until then `imapsync` is documented |
| Audit of admin actions | none | **P2** who, what, when, from which address; kept 365 days; domain administrators see their domain's |
| Download client tools | no | no |

### Protocols and groupware

| Zimbra | Rusty today | Plan |
|---|---|---|
| IMAP, SMTP submission, SMTP in | yes | done |
| POP3 | no | D2 (recommended: a small service in P3) |
| ManageSieve and server-side filters | no | **P3** |
| Calendar (CalDAV), contacts (CardDAV), tasks, briefcase, ActiveSync, chat, video | no | D1 (recommended: not part of this server now; a separate service later if wanted) |
| Webmail | designed (`design-webmail.md`, SolidJS) | **P3** |
| 2FA, app passwords, SSO | deferred by the owner | D3 |

## 3. Phases

**P1: daily administration.** The items marked P1 above, in this order, each a series of small PRs (database migration, directory facade, CLI, API and contract, page, tests):
1. administrators: list, grant, revoke (the table `admin_grants` exists); 2. account status (`active`, `locked`, `closed`) and `must_change_password`, last login and last delivery;
3. plans (class of service) and quotas with usage counters, the refusal at delivery and the IMAP `QUOTA` answer; 4. delete: aliases, closed accounts after the delay, empty domains;
5. search, paging, sort and CSV import of accounts; 6. distribution lists, multi-target aliases, catch-all, domain aliases; 7. the outbound queue manager (list, requeue, delete, hold) and the delivery log with message trace.

**P2: operations and policy.** Audit log; platform settings with hot reload; anti-spam and anti-virus policy and the quarantine; certificates page; statistics; DKIM rotation; mail footer; the `helpdesk` role;
mailbox export and import, deleted items.

**P3: users' own features.** The users' portal and the webmail (self-service password, forwarding, vacation, signatures, filters with ManageSieve), POP3 if D2 says yes, the global address list, retention rules,
backup status, cross-mailbox search for platform administrators, the IMAP migration job.

**P4: deferred by the owner.** 2FA, app passwords, SSO.

## 4. Decisions for the owner (each has a recommendation; nothing below is built before it is answered)

* **D1. Groupware** (calendar, contacts, tasks, briefcase, ActiveSync, resources): Zimbra is a collaboration suite, this project is a mail server. *Recommended: out of scope now.* If wanted later, CalDAV and CardDAV are
  separate services with their own design, not part of the admin work.
* **D2. POP3.** *Recommended: yes, in P3, as a small service on the shared mailbox code* (some old clients and devices still need it).
* **D3. Per-domain external authentication** (LDAP or AD of the customer, SSO). *Recommended: stays deferred as decided.*
* **D4. Archiving and legal hold.** *Recommended: no;* a retention rule per plan (P3) covers the usual need.
* **D5. Cross-mailbox search by administrators.** *Recommended: build it only with the audit log, for platform administrators, off by default,* because it lets an administrator read other people's mail.

## 5. Rules every item follows

* **Tenant isolation first.** A domain administrator sees and changes its own domain only; a request about another tenant's object answers as if the object did not exist (as the existing pages do). Every new route has the
  isolation test of the existing ones (`manage.rs` and `outbound.rs` tests are the model).
* **Through the directory facade.** The web calls the same `Directory` and repository methods the CLI uses, so a rule is enforced once. A new rule is a method there, tested there, and used by both.
* **Quotas and status are enforced where mail enters and where users log in** (SMTP delivery, IMAP and submission login), not only displayed; the admin page shows what the services enforce.
* **Nothing destructive without a step back.** Closing precedes deleting; deleting a domain needs it empty; a CSV import is a dry run first; every destructive action asks for the exact name.
* **Text in the catalogue**, errors with stable codes, every table with paging and a size cap, every list a query with a budget (`query_shape_pg.rs`).
* **One migration per item,** forward only, with its privilege lines in `grants_pg.sql`; the per-service roles get only what the service needs (the delivery service reads quotas; it does not write plans).
