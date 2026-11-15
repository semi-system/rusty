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

The `rusty-admin` binary (which serves this API) comes next; until then a grant changes nothing a user can do.

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

## The HTTP API: session and setup (`rusty_admin::api`)

| Request | Does |
|---|---|
| `POST /api/v1/session` `{address, password}` | sign in; `200` with `{address, scope, domain, csrf}` and the cookie `__Host-rusty_admin` (`Secure; HttpOnly; SameSite=Strict; Path=/`, no `Domain`, lives 8 h) |
| `GET /api/v1/session` | who is signed in, with the CSRF token; `401` when no one |
| `DELETE /api/v1/session` | sign out; the cookie is cleared |
| `GET /api/v1/setup` | `{required: true}` while setup is open, `404` otherwise |
| `POST /api/v1/setup` `{token, address, password}` | make the first platform administrator (below); `201`, signed in at once |

* **Every response** has `Cache-Control: no-store`. Errors are `{"error":"<code>"}` with a fixed code: `invalid` (400, also for unknown fields), `unauthorized` (401),
  `forbidden` (403), `not_found` (404), `exists` (409), `unsupported` (415), `throttled` (429, with `Retry-After`), `busy` (503). No backend text, path or input is echoed.
* **Sign-in** is checked for `Origin` (exactly the public origin) and a JSON body before anything else; there is no CSRF token yet at that point.
  **Sign-out and every later state-changing request** need all three: `Origin`, `Content-Type: application/json` and `X-CSRF-Token` equal to the session's.
* **Setup**: open only while a token is outstanding **and** no platform administrator exists (the second is a database fact read on every request, so granting
  one with `rustyctl admin grant` closes the page at once). Wrong token `401`; 5 wrong from one address refuse that address for its hour; 20 wrong from anywhere
  lock it for everyone for an hour (`429`). A right token with a bad address or a password under 12 characters is `400` and is **not** spent. Setup runs one at a
  time and re-checks under the lock, so concurrent requests make exactly one administrator. It creates the domain if it does not exist, the account, the platform
  grant, spends the token and opens the session.
* `AdminApp::require(req, mutating)` is what the later endpoints call: the session from the cookie, the administrator from the database (not the session), and for a
  mutation the three guards.

## The binary: `rusty-admin`

```
rusty-admin [--config FILE]        # default /etc/rusty/admin.toml
```

Its configuration is the file of the **admin role** (sections it accepts: `store`, `limits`, `tls`, `valkey`, `sealing`, `admin`; any other service's section is refused
without echoing it). `blob_dir` is not needed and not accepted.

```toml
hostname = "admin.example.org"

[admin]
listen = "127.0.0.1:8080"                  # default; behind the edge, which terminates TLS and applies the IP allow-list
public_origin = "https://admin.example.org" # exactly what the browser sends in Origin: lower case, no path

[store]
url = "postgres://rusty_app@db.internal:5432/rusty"
password_file = "/run/secrets/pg"
```

* With `[tls]` naming a certificate the service terminates TLS itself (SIGHUP reloads it); without it, put the edge in front.
* **Standard output carries one line, and only when no platform administrator exists: the setup token** (64 hex digits). Logs go to standard error and never contain it.
  A new token is made at every start; once the first administrator exists there is no token and the setup page answers `404`.
* Sessions are kept in the process for now (a restart signs everyone out). A `[valkey]` section is accepted but not used yet; a warning says so.
* Exit status: 0 after a clean stop (SIGTERM or SIGINT, sessions drain within `shutdown_grace_secs`), 1 for a runtime failure, 2 for a bad command line or configuration.
