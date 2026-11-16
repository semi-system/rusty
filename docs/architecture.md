# Architecture

Rusty is a from-scratch rewrite of the Zimbra FOSS mail stack in Rust: standard protocols (SMTP, IMAP, later JMAP and DAV), a new data model, and an import tool for Zimbra later. It does not keep
Zimbra's SOAP, LDAP or MariaDB schema.

This page is the stable picture. Per-feature designs are the `design-*.md` files next to it; how to run things is in `docker.md`, `first-mail.md` and `postgres-production.md`.

## Services

One binary per service (one image each, see `design-per-service-images.md`), all built from the same workspace:

| Service | Job |
|---|---|
| `rusty-smtp-in` | Port 25: unauthenticated inbound mail, optional spam and virus scanning (`[filter]`) |
| `rusty-submission` | Ports 587 and 465: authenticated submission, sending budget per account |
| `rusty-imap` | Ports 143 and 993 |
| `rusty-outbound` | Delivery worker: queue, DKIM signing, relays, DNS and storage checks. No port |
| `rusty-gc` | Garbage collection, blob store upkeep, periodic jobs. No port |
| `rusty-admin` | Admin web and API (`design-admin-web.md`) |
| edge (HAProxy) | Public ports, TLS ends here for 465/993/443, PROXY v2 to the services, ACME HTTP-01 per domain |

`rustyctl` is the operator CLI. `rusty` (multi-call) is the image of the one-shot jobs (migrations, blob directory setup).

## Libraries (crates)

```
rusty-core       ids, errors, configuration, shared types (no database dependency)
rusty-store      repository traits and the Postgres implementation, blob stores, the change feed
rusty-directory  domains, accounts, aliases, lists, plans, authentication
rusty-mime       a thin layer over mail-parser / mail-builder: envelope, headers, preview, size
rusty-smtp, rusty-imap, rusty-outbound, rusty-s3, rusty-valkey, rusty-seal, ...
```

Dependencies point one way: `core` at the bottom, then `store`, then `directory`, then the protocol crates, then the binaries.

## Data model rules

* PostgreSQL is the only database. The schema lives in `migrations/`; every foreign key is real, columns are `NOT NULL` unless NULL means something, enumerations are `CHECK` constrained.
* Domain = tenant. A tenant is a security boundary (quota, keys, administration, blob store), not a unit of data placement.
* Ids are newtypes (`AccountId`, `MailboxId`, `MessageId`, `FolderId`) and cannot be mixed. `MessageId` grows monotonically per mailbox, the IMAP UID per folder.
* Composite foreign keys `(mailbox_id, x)` stop one mailbox from referring to another mailbox's rows.
* An address belongs to exactly one of account, alias, list. Local part and domain are stored lower case; the full address is never stored twice.
* Message bodies are blobs addressed by a tenant-keyed hash, reference counted by the database, and removed by `rusty-gc` after a grace period. Blob stores are per tenant (`blob-stores.md`).
* Secrets at rest are sealed (`sealed-secrets.md`): only the services that open message bodies hold the unsealing key.
* Correctness never depends on the wall clock; clocks are only for TTLs, grace periods and logs (`clock-policy.md`).

## Consistency and background work

* No distributed transactions. The write order is fixed (blob first, metadata second), every step is idempotent, and reconciliation plus garbage collection clean up after a crash.
* Writes to one mailbox are serialised by locking its `mailboxes` row; `uid`, `msg_id` and the change sequence have no gaps. Different mailboxes write in parallel.
* Inbound delivery is at-least-once with an exactly-once effect: a stable `delivery_id` has a unique row, so a repeated delivery is a no-op.
* Coordination is through PostgreSQL, not a consensus protocol: leases for jobs and the outbound queue, with the lease expiry computed by the database clock. A fencing `epoch` stops a stale worker from
  finishing an old job. Cross-process wake-ups use `NOTIFY` (the change feed) and, optionally, Valkey.
* Valkey (when configured) holds only derived, rebuildable state: shared rate limits, the sending budget, login throttling. Losing it never loses data; limits fall back to per-process windows.
* Failure behaviour: authorisation and authoritative reads fail closed (SMTP 451, IMAP `NO [UNAVAILABLE]`); only caches may fail open by being skipped.

## Scale targets

The design target is 100,000 mailboxes and about 1,000,000 messages a day in total (inbound plus outbound), about 12 messages a second on average and about 120 at the peak. With roughly ten statements per
delivered message that is within one PostgreSQL primary, with asynchronous replication.

* Asynchronous replication means a failover can lose the last few transactions. For inbound delivery the mitigation is to commit the delivery transaction with `synchronous_commit` when a synchronous standby
  exists, so an accepted (`250`) message is not lost.
* Anything above that (10 million mailboxes) is a stretch goal, not a requirement. If it is ever needed, the shard key is the mailbox, not the tenant (a single tenant can be larger than one cluster):
  the directory (domains, accounts, aliases, lists) moves to a small control cluster and mailbox data is routed by a cached mailbox-to-shard map. Constraints that span mailboxes then become control-plane
  references instead of foreign keys.
* A scale claim needs a benchmark: one primary with 100,000 mailboxes and 230 messages a second at the peak (p99 delivery under 200 ms), then multi-shard, each stage a gate before it is advertised.

## What is not built yet

See `roadmap.md`: webmail, JMAP, search, calendars and contacts, folder sharing (`design-folder-sharing.md`), an authoritative DNS server (`design-authoritative-dns.md`), the Zimbra import tool.
