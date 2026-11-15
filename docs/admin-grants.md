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

## Management endpoints: domains, accounts, aliases

The routes, their request and response shapes and their authorisation are fixed by **`crates/rusty-admin/contract/v1.json`**; the front end's types are generated
from it. A test runs the endpoints and checks the real responses have exactly the declared fields and types, and that every listed route exists.

| | Platform administrator | Domain administrator |
|---|---|---|
| list domains | all | its own domain only |
| create, disable, enable a domain | yes | `403 forbidden` |
| accounts, aliases of a domain | every domain | its own domain |
| account password, disable, enable | every account | accounts of its own domain |

* **Scope is decided in the handler, after the session**: a domain, account or alias outside the administrator's scope is `404 not_found`, **byte for byte the answer for one
  that does not exist**, so the API is no oracle for other tenants' names (tested with both spellings and with odd encodings of the other domain). An alias in your domain
  pointing at another tenant's account is "no such account". Platform-only actions are `403` (a rule, not a missing thing).
* **No self-lockout**: an administrator cannot disable the account it is signed in with, nor disable the domain that account lives in.
* **Sessions follow the account**: a new password or a disabled account ends every session of that account at once.
* The first account of a domain also receives `postmaster@` and `abuse@` (as with `rustyctl account add`; the rule is repeated here, not shared).
* Lists are paged by `?after=<cursor>` (page size 100); `next` is `null` at the end; any other query parameter is `400`. A mutation takes no query at all. Unknown JSON
  fields are `400`. Passwords: at least 8 characters for an account (as with `rustyctl`).

## DKIM and relay endpoints

Both are in `contract/v1.json` and follow the scope rules above (a domain administrator reaches its own domain only; anything else is the same `404`).

* `GET/POST /api/v1/domains/{d}/dkim`: `GET` lists the **public** records (name, value, and whether the private half is sealed). `POST` makes an ed25519 key and seals
  its private half to `[sealing] public_key` (only the outbound service's `[unsealing]` key opens it); the response is the public record only. The web never accepts a
  pasted private key (RSA keys are imported with `rustyctl dkim import`). A selector is made once: a second request for it is `409 exists`. The **public row is written
  first** (it is write-once, so concurrent requests for one selector have exactly one winner and only the winner stores its private half): if sealing then fails, the listing
  shows `sealed: false` and a new selector is used.
* `GET/PUT/DELETE /api/v1/domains/{d}/relay`, `POST .../relay/check`: the relay of the domain (host, port, TLS, login, SPF include), under the rules of `rustyctl domain relay`
  (four ports, a public host, a login only over TLS). The password is sealed, can only be overwritten and is **never returned**; a login always comes with its password in the same
  request, and a relay without a login deletes the old password. `check` queues a `relay.check` job; the outbound service records the fixed category shown by `GET`.
* A deployment without `[sealing]` answers `503 unavailable` where a secret would be sealed (a relay without a login still works).
* The admin binary currently links `rusty-smtp` (without its default features) for the key generation and the job names; the SMTP crate split of the image design removes that.

## The DNS page: records, check, `dns.check`

* `GET /api/v1/domains/{d}/dns` generates the records of the domain from the operator's facts and the stored DKIM keys with the same code as `rustyctl domain dns`
  (`rusty_core::dns::records_with_facts`, now shared by the CLI, the web and the job) and joins them with the stored check state. A record whose expected value changed since its
  last check shows as `unchecked`: the stored status described another record. The facts come from `[admin]`: `public_ips`, `spf_include`, `via_relay` (the same values as the
  outbound service's `[outbound]`) and the shared `hostname`; without a `hostname` the page answers `503 unavailable`, without `public_ips` it warns (`no_public_ips`).
* `POST .../dns/check` queues a **`dns.check` job**. The admin never resolves names; the all-in-one daemon (the outbound role) runs `DnsCheckWorker`, which checks the records against live
  DNS with the engine of `rustyctl domain check` and records the observations as drift state (an `ok` record that fails three checks in a row raises an alert).
* Not yet: the **periodic** re-check of every active domain (the `[dns_check]` section exists; its scheduler does not).

## Queue figures

`GET /api/v1/domains/{d}/outbound` returns `{queued, running, failed, dead}`: the outgoing messages of this domain by state, from the queue's own table (a domain administrator sees its
own domain only; another tenant's domain is the same `404` as a missing one). Mail waiting for a failing relay shows as `queued`; `rustyctl`-less diagnosis of *why* is the relay check.
