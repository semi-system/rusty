# Sealed secrets: how they are stored

A secret that the web creates or receives (a DKIM private key, a tenant's relay password) is **sealed** (`rusty-seal`, HPKE) to the outbound
service's public key before it is stored, so that the admin service can write it and **cannot read it back**. This page is about the
storage; the sealing is described in `docs/superpowers/plans/2026-11-14-admin-web-design.md` (section 14.1).

## The table

`sealed_secrets` (migration 0019): one row per `(owner_kind, owner_id, purpose)`.

| Column | |
|---|---|
| `owner_kind` | `domain` today; 1 to 32 of `a-z 0-9 - _`; no foreign key, so other owners (a tenant) can be added |
| `owner_id` | the owner's row id |
| `purpose` | `dkim-private-key`, `relay-password`, ...; same alphabet |
| `key_id` | the id (16 hex digits) of the sealing key the blob was sealed to; how the outbound service picks the key when keys are rotated |
| `sealed` | the blob, 16 to 66000 bytes; the database cannot open it |
| `created_at`, `updated_at` | unix seconds; a write replaces the whole row, `created_at` stays |

Constraints repeat what the store checks, so a careless writer is stopped by the database too. A trigger removes the secrets of a deleted
domain (`owner_kind = 'domain'`); another kind with the same id is another owner and is untouched.

## Two facades, so that the split is a type

* `SealedWriter` (the admin service): `put`, `delete`, `meta`. `put` returns metadata (kind, id, purpose, key id, size, times), **never the ciphertext**.
  There is no method that returns one.
* `SealedReader` (the outbound service): `get` (the ciphertext, to be opened with `rusty-seal` for the same kind, id and purpose) and `meta`. It cannot write.

## Privileges

* **Now**, with one runtime role: `rusty_app` has the usual DML on every table, including this one, so the split above is enforced by the
  facades, not by the database. `rusty_ro` (reporting) has **no `SELECT`** on `sealed_secrets` (`grants_pg.sql`; tested in `pg_roles.rs`):
  it cannot open a blob, but it has no business seeing one either.
* **With the per-service roles** (one Postgres role per service, an item of the image split): the admin role gets `INSERT`, `UPDATE` and `DELETE` on this table and
  **no `SELECT`**, the outbound role gets `SELECT`. That is a grants block to add with those roles, and a test that connects as each of them;
  it is not done yet and nothing here pretends it is.

## The key pair, and how the outbound service uses sealed DKIM keys

```
rustyctl seal keygen /etc/rusty/unseal.key     # makes the pair: the private half to the file (0600, never replaced), the public half printed
rustyctl seal show   /etc/rusty/unseal.key     # prints the public half and the key id of an existing file
```

The output shows the two configuration sections to add:

```toml
# on whatever seals (the admin service, rustyctl): the public key, not a secret
[sealing]
public_key = "rusty-seal-v1:<64 hex digits>"

# on the outbound service only: where the private key is; the first file is the current key, up to four, the others only open what an
# earlier key sealed (rotation)
[unsealing]
key_files = ["/etc/rusty/unseal.key"]
```

**Keep the key file backed up.** Without it nothing sealed to it can be opened, and `keygen` refuses to replace an existing file for that reason.

With `[unsealing]` set, the outbound service signs with the **sealed** DKIM keys of the database:

* a key is one row of `domain_dkim_keys` (the public half) and one sealed secret whose `purpose` is `dkim-` and 24 hex digits of the SHA-256 of the
  lower-cased selector (`rusty_smtp::dkim::sealed_purpose`: a selector may hold dots and capitals, a purpose may not);
* every key of a domain that has its private half sealed here signs (an ed25519 and an RSA key sign together, as with key files);
* a public row **without** a sealed private half (made elsewhere) is not signed with;
* a sealed key that **cannot be opened** (the wrong unsealing key, a blob moved to another domain, a damaged blob) is an error: the worker
  **defers the mail** and records the reason on the job, it never sends the message unsigned;
* loaded keys are kept for 60 seconds, so a removed or replaced secret is noticed within a minute;
* a domain with no sealed key keeps signing with its key files in `[outbound] dkim_dir`: an installation that does not use sealed keys changes
  nothing, and when a domain has both, **the sealed keys win** (one selector never signs twice).

## Making a sealed DKIM key

```
rustyctl dkim generate example.org --sealed [--selector S]
```

Needs `[sealing] public_key` in the configuration. It makes an ed25519 key, seals the private half to that public key, stores it in
`sealed_secrets` and the public half in `domain_dkim_keys`, and prints the TXT record. **No file is written and the private key is never
shown**: only the outbound service, holding the unsealing key, can open it. Like the file command it never replaces a key: the same selector
again changes nothing and shows the record; a selector whose key was made as a file is refused (use another selector).

## Not in this change

The admin endpoint that does the same from the web, the per-tenant relay route and its SSRF policy come next.
