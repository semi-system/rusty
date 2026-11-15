# Who may use the admin web: administrator grants

An administrator is an **ordinary account** (e-mail and password, Argon2 as for IMAP) that has a row in `admin_grants` (migration 0021). Nothing in the
table is a credential; signing in will be the account's own password, checked by the directory.

| Grant | Meaning |
|---|---|
| platform (`domain_id` NULL) | every domain; may create domains |
| domain (`domain_id` = d) | the administrator of that one domain only; the account must belong to that domain |

One grant per account: granting again widens or narrows it. The database enforces "a domain administrator is an account of that domain" for any
writer (trigger, SQLSTATE `RS002`), a grant goes with its account (`ON DELETE CASCADE`), and a missing account or domain is an error, not a row.

## `rustyctl admin`

```
rustyctl admin grant ops@example.org            # platform administrator
rustyctl admin grant boss@client.net client.net # administrator of client.net only
rustyctl admin revoke boss@client.net           # the account stays; it is no longer an administrator
rustyctl admin list
```

* The account must exist and have a password (otherwise it could never sign in).
* This is also the **recovery path**: if no administrator can sign in, grant one from the shell.
* The first platform administrator is normally made on the web with the setup token (design section 14.3); that page exists only while
  `AdminStore::has_platform_admin()` is false, a database fact read each time.

## Not in this change

The HTTP handlers (session endpoints, CSRF, the setup page) and the `rusty-admin` binary come next; until then a grant changes nothing a user can do.

## Sessions and sign-in (crate `rusty-admin`)

The logic behind the admin web, without HTTP: `rusty_admin::SessionStore` and `AdminAuth`.

* **Session id**: 256 random bits (OS generator), 64 hex digits in the cookie. The store keeps **only `SHA-256(id)`** as the key, so reading the store
  does not give a usable id. The value (account, scope at sign-in, CSRF token, creation time) lives in the `Kv` store (Valkey, or the in-process store).
* **Idle timeout** 30 minutes (renewed at every use), **absolute** 8 hours after sign-in. A new id at every sign-in.
* **Revocation**: `revoke_account` makes every session of the account made up to that moment invalid (password changed, account disabled, grant removed)
  with a single marker, no index of sessions needed.
* **Fails closed**: when the store cannot answer, `get` is an error, never "signed in"; a malformed id is "no session" without touching the store.
* **Sign-in** uses the directory's authenticator (Argon2, dummy verification for unknown accounts) and the same `RateLimiter` as IMAP and submission
  (keys of the resolved account and of the canonical address, per source address). **One fixed error** for a wrong password, an unknown or disabled
  account and an account that is not an administrator; the last counts as a failure, so probing for administrators is throttled like guessing.
* **Every request** goes through `AdminAuth::principal`, which re-reads the grant and the account status from the database: narrowing or removing a grant,
  or disabling the account, takes effect at the next request whatever the session remembers, and a refused session is ended.
