# Design: blob storage on S3, one store per tenant

Status: **approved in principle** (decided 2026-11-15: S3, a store of its own for each tenant, not one S3 for the whole system; **tenant = domain**). Nothing here is built.
Builds on `docs/superpowers/plans/2026-11-14-per-service-images-design.md` (blobs on S3 only, any S3-compatible server, RustFS in tests, credentials per service).

## 1. What changes, in one paragraph

Today every message body is a file in one directory, named by its SHA-256, shared by everybody; isolation between tenants is in the database. After this change a **blob store** is a
first-class object: a domain has at most one of its own, the store is an S3 bucket (and prefix) that belongs to the tenant, and the message bodies of that domain are objects in it. The platform
keeps a default store (the one in `rusty.toml`, a filesystem directory or an S3 bucket the operator runs) for domains that have none of their own and for system mail.

## 2. Vocabulary: what a "tenant" is here

**A tenant is a domain** (decided 2026-11-15): the unit administrators are scoped to, and the unit that owns storage. A domain either has a store of its own (one row, one bucket, bound to that domain
only) or uses the platform store. Two domains never share a store row, so the settings, the credentials, the check status and the garbage collection of a store are all that one domain's.

## 3. Data model (migration 0023)

* `blob_stores(id, domain_id UNIQUE NULL, kind 'fs'|'s3', config jsonb, check_status, checked_at, created_at)`: `domain_id` names the one domain that owns the store (NULL only for the platform row). `config` holds the non-secret part only: endpoint, region, bucket, key prefix, path-style flag.
  Row `1` is `platform` (made by the migration; its settings come from `rusty.toml`, never from the database).
* Credentials are **not** in `config`: the access key and secret key are sealed secrets (HPKE, `docs/sealed-secrets.md`, owner kind `blobstore`, purposes `s3-access-key` and `s3-secret-key`),
  written by the admin service (which cannot read them back) and opened only by the services that read or write bodies.
* `domains.blob_store_id` (default 1; a domain's own store is the row whose `domain_id` it is). `blobs` is keyed by `(store_id, hash)` and `messages` gain `blob_store_id`, fixed when the message is stored; the reference-count triggers and the
  foreign key use the pair. Existing rows get store 1: no data moves.
* Same hash in two stores = two objects and two rows (see section 9).

## 4. The code seam

* `BlobStore` stays the trait of one store, with one addition: `get_range(hash, start, len)` (IMAP partial fetch should not pull a 50 MiB body for a header).
* A registry `BlobStores::for_store(id) -> Arc<dyn BlobStore>` builds a store lazily from its row, caches it and drops it when the row changes (the cross-process change feed that exists
  for IDLE carries a "store changed" event). Every place that today holds one `Arc<dyn BlobStore>` (SMTP inbound, outbound queue, IMAP, GC, `rustyctl`) asks the registry with the store id of
  the domain of the account (or of the sender's domain for the outbound queue; system mail and bounces use the platform store).
* **The outbound queue stays as it is** (decided 2026-11-15): the body of a message waiting in the queue is stored in the **platform** store and referenced by the job payload, protected by the
  `gc_grace_seconds` rule (grace at least the queue's `max_age_secs` plus a day). Moving queue bodies to the sender's store, with a reference of their own (`job_blobs`), a memory budget
  for the sending workers and a read cache, is left for later; nothing in this design depends on it.

## 5. The S3 adapter

* Key layout `<prefix>/<hh>/<hh>/<64 hex digits>` (two shard levels, as on disk). Put is idempotent (same name, same bytes) and sent with `x-amz-checksum-sha256`; a get is hashed and compared
  with its name before any byte is used (a tampered or swapped object is an error, never mail). Bodies are at most `max_message_bytes` (default 50 MiB), so single-part PUT: no multipart in v1.
* Listing for GC and `blobs verify` uses `ListObjectsV2` under the prefix and only ever touches keys that look like a hash under our prefix.
* Client: `aws-sigv4` for signing plus our own small request layer on the HTTP stack already in the tree, with a connector of our own (needed for section 6); no `aws-sdk-s3` (a very large
  dependency tree for four calls). Each new dependency passes the supply-chain gates (`cargo deny`, the wrapped-libs list).
* Credentials are static keys only. No instance profile, no environment lookup for a tenant store: a tenant's endpoint must never make the server present the platform's own identity.

## 6. A tenant's endpoint is untrusted input (SSRF)

A tenant chooses the endpoint, so the server makes network requests where a tenant says. The same policy as for tenant relays (`rusty_core::relay_policy`) applies, enforced **at every connection**
by the connector, not only when the setting is saved: HTTPS only; port 443 or from a short allow-list; the resolved addresses must all be public (no loopback, private, link-local, metadata
ranges); no redirect is ever followed (it would carry the Authorization header away); TLS is verified against the web roots; request and response sizes and times are capped. Only the
**platform** store, configured by the operator in `rusty.toml`, may use an internal address (the RustFS next to the mail services) and a private CA.

## 7. Who configures it

Version 1: **the platform administrator** (admin web, API and `rustyctl`): create a store, set its credentials (sealed), bind domains to it. A domain administrator sees the status of
their store, not the settings. Letting a domain administrator bind their own bucket is a policy switch for later (the section 6 checks make it safe to allow).

A `storage.check` job (like `relay.check`, run by a service that can reach S3) puts, reads (whole and ranged), lists and deletes a probe object under `<prefix>/.rusty-probe/` and records a
category: ok, name does not resolve, address refused by policy, TLS, bad credentials, bucket missing, access denied, timeout. A store is usable only after a check passed.

Changing the store of a domain that has messages is **refused in v1** (it needs a background move: copy, switch, delete); a domain without messages can be re-pointed freely.

## 8. When a tenant's S3 is down or slow

* Delivery to that tenant's mailboxes answers `451 4.3.0` (the sender retries; nothing is lost and nothing is stored half-way). Reading a body answers IMAP `NO [UNAVAILABLE]`; flags, folders
  and counts come from the database and keep working.
* Per store: a concurrency cap (one slow bucket cannot take every worker) and a circuit breaker (after N failures in a row requests fail fast for a while, then one probe goes through).
  Other tenants are not affected. The state is visible in the admin web (check status, breaker state, last error category) and as a metric.
* The platform store down: everything that uses it fails the same way; the database-only parts of the service stay up.

## 9. Garbage collection and the tenant's bucket

* GC runs **per store**, driven by the database (`blobs.refcount`, `zero_since`, grace period), under a per-store lease (migration 0023 also replaces the `.gc.lock` file, which means nothing on S3).
  It deletes the object, then the row; a crash between the two leaves a row for a missing object, which the next run removes.
* It deletes nothing that the database does not name: objects of the tenant under our prefix that have no row are **reported** (`rustyctl blobs verify --store N --orphans`) and never removed
  automatically; objects outside our prefix are never listed.
* `blobs verify` learns to read S3 (a missing or mismatching object is reported per store).
* The tenant's bucket is part of their mail storage: if they delete or overwrite objects, bodies are lost. The operator documentation recommends a credential limited to the prefix with
  `Get`, `Put`, `Delete`, `List`, and bucket versioning or an object lock on the tenant's side. We cannot enforce what the tenant does in their own bucket.
* Deleting a domain removes its rows; its objects are removed by GC after the grace period (a store that is no longer used by any domain is left in place for the tenant to delete).

## 10. What is given up and what stays

* **Cross-tenant de-duplication is gone**: the same attachment in two tenants is stored twice, and a message to recipients of two tenants is stored once in each store. That is the price of isolation.
* Isolation improves: a tenant's data lives in a place they control, a compromise of one bucket exposes one tenant, a credential leak of one store cannot read another.
* Database isolation (rows per domain, per-service roles) is unchanged.

## 11. Order of work, each step with its tests (and a mutation check on every guard)

1. **Schema and registry**, filesystem stores only: migration 0023, `BlobStores`, two filesystem stores proving that two domains never see each other's bodies, the platform store unchanged for existing data.
2. **S3 adapter** (one bucket) against RustFS (pinned image, started by CI): put/get/range/list/delete, checksum mismatch, tampered object, large body, errors mapped to the categories.
   **2b. The pool** (section 12): migration 0024, the `blob_backends` and `blob_locations` tables, a pool store that implements `BlobStore` over several backends with the three presets, health and failover
   (a backend stopped in the middle of reads and writes), location-aware GC, `storage.repair`; tested with several RustFS buckets (and with filesystem backends in unit tests).
3. **Endpoint policy and credentials**: the connector enforcing section 6 (every refused class has a test), sealed credentials, `storage.check`, admin API, `rustyctl storage ...` and the page in the web admin.
4. **GC per store and `blobs verify` on S3**, the lease, orphans report.
5. **Breaker and limits**, status in the admin web, the failure behaviours of section 8 tested with a store that is stopped and one that hangs.
6. Later: moving a domain between stores, a domain administrator binding their own bucket, per-store byte quotas, application-level envelope encryption.

## 12. Several S3 backends for one tenant (failover, JBOD, load balancing)

A tenant may set up **more than one S3 backend** (bucket, endpoint, credentials) under its store (decided 2026-11-15). A store of kind `s3` is then a small pool, at most 8 backends, and the
store's policy says how objects are spread and read. The three behaviours the owner asked for are presets of four settings:

| Behaviour | `copies` | `sync_copies` | `placement` | `read` | What it means |
|---|---|---|---|---|---|
| **JBOD** | 1 | 1 | `weighted` | `located` | Each object lives on exactly one backend, spread by weight (capacity adds up). No redundancy: a lost backend loses its objects. |
| **Mirror** | N (2..=backends) | N | `weighted` or `ordered` | `round_robin` / `fastest` | N copies written before the message is acknowledged; reads are balanced over the holders; any one may fail. |
| **Failover** | 2 (or more) | 1 | `ordered` | `ordered` | Written to the first healthy backend, copied to the others afterwards by the repair job; read from the first healthy holder. |

* `copies`: how many backends should hold each object. `sync_copies`: how many must have it before the write succeeds (`1 <= sync_copies <= copies`); the rest are made by repair.
* `placement`: `ordered` (the listed order, skipping unhealthy ones) or `weighted` (rendezvous hashing over weights and backend ids, so the choice is deterministic and spreads evenly).
* `read`: `located` (the recorded location), `ordered`, `round_robin` or `fastest` (lowest recent latency) among the backends that hold the object and are healthy.

### Data (migration 0024)

* `blob_backends(id, store_id, name, kind, config, weight, state, check_status, checked_at)`: one row per backend. `state`: `active`, `readonly` (reads yes, no new writes), `draining` (the repair job
  moves its objects away), `disabled`. Credentials are sealed secrets per backend (owner kind `blobbackend`). A store of kind `fs` or `platform` has no backend rows: the store is its own single backend.
* `blob_locations(store_id, hash, backend_id, state)`: where each object is, with `state` `ok` or `pending_delete`. It is the authority; the placement function is only how a new location is chosen.
* The store policy (`copies`, `sync_copies`, `placement`, `read`) lives in `blob_stores.config`.

### Write, read, delete

* **Write**: choose `copies` healthy writable backends by the placement rule, PUT to them (in parallel); the write succeeds when `sync_copies` have confirmed; the locations are recorded in the same
  transaction as the blob row. If fewer than `sync_copies` backends are healthy the write fails (`451` on delivery: the sender retries, nothing is lost). A copy that is still missing is made by repair.
* **Read**: candidates are the `ok` locations whose backend is healthy, ordered by the `read` rule; try the first, on an error or a timeout the next; every body is verified against its hash, and a
  copy that fails verification is marked bad and left to repair. If no holder can be reached the read fails for that object only (`NO [UNAVAILABLE]`); objects held elsewhere are unaffected.
* **Delete** (GC): the row goes, then every location's object is deleted; a backend that cannot be reached keeps its location as `pending_delete` and is retried, so a backend that was down when GC ran does
  not keep garbage forever, and a backend that comes back never resurrects a deleted blob.

### Health and failover

Each backend has its own concurrency cap and circuit breaker (section 8); a breaker that is open takes the backend out of both writes and reads until a probe succeeds. Failover is therefore automatic
and needs no operator: the pool simply stops choosing a backend that fails. The admin web shows each backend's state, check result, breaker and last error category.

### Repair and rebalance (`storage.repair`, a job)

* Objects with fewer `ok` locations than `copies` (a backend was down for a write, a copy failed verification, a backend was added) are copied from a healthy holder to a chosen backend.
* Objects on a `draining` backend are copied away, then deleted there; a backend can be disabled and removed only when no location is left on it.
* JBOD does not move existing objects when a backend is added (new objects use it); a `rebalance` option moves a share by weight, in the background, rate-limited.
* A periodic scrub reads a sample of objects, verifies the hash and reports/queues repair for bad copies.

### What it costs and what it does not give

* Mirror writes are as slow as the slowest of the `sync_copies` backends and use `copies` times the storage; that is the price of surviving a backend.
* JBOD gives capacity and spread, not durability. The admin web says so when a store is configured with `copies = 1` and more than one backend.
* Consistency is simple because bodies are immutable and addressed by content: a copy is either the right bytes or it is detected on read.
* The endpoint rules of section 6 apply to **every** backend; the check (section 7) runs against every backend and a store is usable only when each active backend passed.

## 13. Points to confirm

1. ~~A tenant is a group of domains~~ **Decided: tenant = domain** (section 2).
2. Version 1: only the platform administrator binds a store (section 7). *Taken as the default unless the owner says otherwise.*
3. Refusing to re-point a domain that already has messages in v1 (section 7). *Same.*
4. `aws-sigv4` plus our own request layer instead of `aws-sdk-s3` (section 5). *Same.*
