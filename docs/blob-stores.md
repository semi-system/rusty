# Blob stores: giving a domain its own S3 bucket

Design and reasons: `docs/design-s3-per-tenant.md`. This page is what an operator does with it today.

By default every message body is stored in the **platform** store (the directory `blob_dir` of `rusty.toml`). A domain can have a store **of its own**: an S3 bucket the tenant owns. The
bodies of that domain's mail are then objects in that bucket and nowhere else, and garbage collection works on each store separately.

## What you need

* The mail service must hold the **unsealing key** (`[unsealing] key_files` in its `rusty.toml`, `docs/sealed-secrets.md`): the bucket's access key and secret key are sealed to it. Without
  it the service does not offer S3 stores at all.
* `[sealing] public_key` in the configuration that `rustyctl` reads, to seal the keys.
* A bucket, and an access key with `Get`, `Put`, `Delete` and `List` on it. Limiting the key to the prefix is recommended; so is bucket versioning on the tenant's side: if the tenant deletes
  or overwrites objects in their bucket, those bodies are gone.

## Set it up (before the domain holds any message)

```sh
printf '%s\n%s\n' "$ACCESS_KEY" "$SECRET_KEY" | rustyctl --config /etc/rusty/rusty.toml \
  storage set example.org --endpoint https://s3.example-cloud.com --region eu-west-3 --bucket tenant-mail --prefix mail --path-style --credentials-stdin
rustyctl --config /etc/rusty/rusty.toml storage show example.org
```

The two keys come from standard input, one per line, never from the command line; they are sealed and `storage show` never prints them. Running `storage set` again with the **same settings**
rotates the keys. Other settings can change only while the store holds no body; a domain that already holds messages on the platform store cannot be given a store (bodies are not moved
between stores in this version).

## What a tenant may choose

The endpoint is the tenant's input and the server makes requests to it, so it is restricted (`rusty_core::storage_policy`, enforced again at every open and, for the addresses, at every
connection): `https://` only; a public DNS name or public IP address (no `localhost`, no private, loopback, link-local or metadata address, no name ending in `.internal`, `.local`, `.lan`...);
port 443, 8443, 9000 or 9443; no credentials, path or query in the URL; no redirect is ever followed; TLS is verified against the system's trusted roots. The region, bucket and prefix must be
plain S3 names.

## Check and admin web

A store is used only after a check recorded `ok`. `rustyctl storage check DOMAIN` (or the button on the admin web page `/domains/{name}/storage`) queues a `storage.check` job; the mail service,
which holds the unsealing key, opens the store exactly as for real mail, puts, reads (also a range), heads, lists and deletes a probe object and records a fixed category (`ok`, `auth`,
`no-bucket`, `tls`, ...) with its time. Only a platform administrator can set the store or queue a check (`PUT` and `POST` `/api/v1/domains/{d}/storage[/check]`); a domain administrator
sees whether the store is ready and the last result, never the settings or keys. Changing the keys makes the store unchecked again.

## Pools (several backends for one domain)

A store of kind `pool` spreads a domain's bodies over one to eight backends (migration 0024; design section 12). `blob_stores.config` holds the policy
(`{"copies":2,"sync_copies":1,"placement":"ordered","read":"ordered"}`; every field optional) and `blob_backends` the backends, each with a name, a weight and a state
(`active`, `readonly`, `draining`, `disabled`). `blob_locations` records which backend holds which object and is what reads go by.

| behaviour | copies | sync_copies | placement | read |
|---|---|---|---|---|
| JBOD (capacity, no redundancy) | 1 | 1 | `weighted` | `located` |
| mirror (any one may fail) | N | N | `weighted` or `ordered` | `round_robin` or `fastest` |
| failover (second copy made afterwards) | 2+ | 1 | `ordered` | `ordered` |

A write goes first to the backends that already hold the object, then by the placement rule; it succeeds when `sync_copies` copies are written and recorded (fewer healthy backends is a
temporary failure, so the sender retries). A read tries the recorded holders in the read order, verifies the bytes against the name and forgets (and removes) a copy that is wrong or missing.
Each backend has a circuit breaker: after consecutive failures it is left out until a cool-down passed. Garbage collection deletes an object on every backend; one that cannot be reached keeps a
`pending_delete` location that repair retries. `PoolStore::repair` also makes missing copies and empties a `draining` backend (which can then be removed). A pool is used only when every backend
that is not disabled passed a check.

Set up with `rustyctl storage pool DOMAIN --copies 2 --sync-copies 1 --placement ordered --read ordered`, `storage backend add DOMAIN NAME --endpoint ... --credentials-stdin [--weight W]` (also `rekey`, `set --state/--weight`, `remove`), `storage check DOMAIN` (every backend that is used is tried) and `storage show DOMAIN`, or on the admin web page of the domain (platform administrators; a domain administrator sees the state of the backends only). Each backend's keys are sealed to that backend (owner kind `blobbackend`). The upkeep service runs `repair` every minute, and opened stores are re-read every minute, so a changed pool takes effect without a restart.

## The platform store in a bucket, collection leases, limits

* `[blob_s3]` in the configuration puts the **platform** store (what every domain without a store of its own uses, and system mail) in a bucket instead of the `blob_dir` directory:

  ```toml
  [blob_s3]
  endpoint = "http://10.0.0.5:9000"        # http or https, host and port only; a private address is fine (the operator chose it)
  region = "us-east-1"
  bucket = "rusty-mail"
  prefix = ""                              # optional
  path_style = true
  access_key_file = "/run/secrets/s3-access"   # one line each, mode 0600, read through the secret source
  secret_key_file = "/run/secrets/s3-secret"
  max_concurrent = 64                      # requests in flight at once
  request_timeout_secs = 60
  ```

  The section is accepted by every service that uses blobs (smtp-in, submission, imap, outbound, gc) and by none else. `blob_dir` stays in the file (the other stores of the deployment
  live next to it) but holds no message body. A wrong name or unreadable key file stops the start. Moving existing mail from the directory to the bucket is not done by the service:
  copy the objects (`ab/cd/<hash>`) into the bucket under the prefix, then run `rustyctl blobs verify` before switching.
* A garbage collector takes a **lease** on the store in the database (`blob_store_leases`) for as long as it runs, renewing it at every batch and shard: a second collector, in this
  process or in another one or on another host, is refused (`Conflict`) until the lease is given back or runs out (30 minutes without renewal).
* Every S3 client, one per tenant store and one per backend of a pool, has a cap on requests in flight (64 by default; `max_concurrent` for the platform store). A request that cannot
  get its turn within the deadline fails as a timeout (a temporary failure to the sender).

## What is not here yet

Moving a domain's bodies between stores, and the outbound
queue (its bodies stay in the platform store). See the design for the order.
