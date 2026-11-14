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

## Not in this change

Nothing reads or writes a secret yet: the outbound service reading sealed DKIM keys, the per-tenant relay route and the admin endpoints are the next steps.
