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

## What is not here yet

The `storage.check` job and the admin web page for stores, using several buckets for one tenant (failover, JBOD, load balancing), moving a domain's bodies between stores, and the outbound
queue (its bodies stay in the platform store). See the design for the order.
