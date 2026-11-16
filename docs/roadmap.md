# Roadmap

The goal is Zimbra parity at the level below. Scope levels:

* **A, mail core:** receive and send, IMAP, web mail, folders, search, filters, spam and virus protection, multi-tenant, administration.
* **B, groupware:** A plus calendar, contacts, tasks, briefcase, sharing, GAL, CalDAV/CardDAV, resources.
* **C, commercial-grade:** B plus integrated backup and restore, storage tiering and the other features usually found only in paid editions. Mobile sync (ActiveSync) is out of scope: standard protocols are enough.

Status words: **built** (code and tests in the repository), **in progress**, **designed** (a design page exists, no code), **planned**, **replaced** (decided against, see the note).
Library policy: use a maintained library first, rewrite after the product works; every dependency passes the licence and supply-chain gates in CI (`ci.md`).

## Milestones

| Milestone | Meaning | State |
|---|---|---|
| M1 | Thunderbird reads and sends real mail against a Compose stack | Reached: the five-service stack runs on the LAN test server, send and read verified on 587, 465, 25 and IMAP 993 |
| M2 | Small production, many tenants | In progress: see the phase 2 table |
| M3 | Level A complete (webmail, search, filters, basic sharing) | Planned |
| M4 | Level B complete (groupware) | Planned |
| M5 | Ready to replace a real Zimbra (real import, verified backup and restore, monitoring, HA) | Planned |

## Phase 1: first mail

| Project | State |
|---|---|
| Container frame and edge | Built: HAProxy edge, PROXY v2, standalone stack (`docker.md`, `first-mail.md`) |
| IMAP server | Built (core, IDLE on the change feed) |
| Inbound SMTP and submission | Built |
| A separate `store` service | Replaced: services use the store crate and PostgreSQL directly, each in its own image (`design-per-service-images.md`) |
| `tlsd`, a Rust TLS layer | Replaced: HAProxy does TLS for 465, 993 and 443; the STARTTLS ports pass through |

## Phase 2: full MTA and minimal production

| Project | State |
|---|---|
| Delivery worker (queue, retries, relays, DKIM signing) | Built (`dkim.md`, `relay.md`) |
| Message authentication on inbound (SPF, DKIM, DMARC) | In progress: needs a resolver section for `rusty-smtp-in` |
| Spam and virus filtering (rspamd, ClamAV) | Built (`docker.md`, the `filter` profile) |
| PostgreSQL and Valkey | Built: PostgreSQL only (SQLite is gone); Valkey for shared limits |
| Tenants | Built: domain = tenant, blob store per tenant (`blob-stores.md`) |
| Certificates per domain | Replaced `certd`: ACME HTTP-01 per domain at the edge (`acme.md`) |
| Operator CLI and admin API, admin web | Built (`design-admin-web.md`); monitoring page in progress |

## Phase 3: level A user experience

| Project | State |
|---|---|
| Full-text search (tantivy per mailbox, filtered by ACL) | Planned |
| JMAP (Core, Mail, Submission, push, sync from the change feed) | Planned |
| Webmail (SolidJS) | Designed (`design-webmail.md`) |
| User mail rules (Sieve, vacation, forwarding) | Planned |
| POP3 | Planned |
| Folder sharing and ACLs | Designed (`design-folder-sharing.md`) |
| Send-as and delegation | Planned (after sharing) |
| External accounts (pull from another IMAP/POP server) | Planned |
| Two-factor, app passwords, SSO | Deferred (owner, 2026-11-16: not needed for now) |
| Authoritative DNS for tenant domains | Designed (`design-authoritative-dns.md`) |

## Phase 4: groupware (level B)

Contacts (vCard, CardDAV), calendar (iCalendar, CalDAV, scheduling and resources), tasks and notes, briefcase (WebDAV), GAL, and the groupware web client. All planned, all reusing the sharing table.

## Phase 5: operations, migration, reliability

| Project | State |
|---|---|
| Backup and restore | Built as scripts (`backup.md`); the verified restore drill is planned |
| Monitoring and alerts (metrics, logs) | Planned; a basic monitor in the admin web comes first |
| Import from Zimbra | Planned |
| Mailbox moves and storage tiers, S3 | Partly built (S3 and pool blob stores, `blob-stores.md`); moves planned |
| Multi-node, HA | Planned (`architecture.md`: scale targets) |
| Kubernetes | Deliberately postponed |

## Optional

Extension mechanism (Zimlet replacement), chat and meetings, compliance archiving and eDiscovery: only if the owner asks.

## Principles

Design for the large target, deploy small, prove each scale stage with a benchmark before claiming it. Every service fails closed. Everything that can be a gate is a gate in CI. Every user-visible
string goes through the message catalogues, the source language is English.
