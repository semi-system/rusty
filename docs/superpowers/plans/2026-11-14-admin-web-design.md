# Admin web: design (approved 2026-11-14)

Status: **approved; section 14 (secrets, first administrator, per-tenant relay) was added later the same day and is also approved.** The owner answered the six questions of section 12 on 2026-11-14: Q1 its own binary from
the first commit, Q2 `hyper` with our own router, Q3 sessions in `Kv`, Q4 no 2FA now, Q5 assets embedded in the binary, Q6 all
user-facing text in message catalogues with placeholders (section 8). Owner decisions already taken (2026-11-14): the admin web and the webmail are written in
**Svelte 5** (Solid was the alternative); the webmail is its own service and has its own design document later; the
platform is on-premises first; mail leaves through an SMTP relay when the operator chooses one (see `docs/relay.md`).
This document replaces the "server-rendered HTML, no JS framework" line of task U10 in
`2026-11-08-usable-mailserver.md`; everything else in U10 (the requirement that a new domain shows its DNS records ready to
copy, the validation list, the auth rules) still stands and is repeated here only where it changes the design.

## 1. Goal and scope of the first version

An operator must be able to run a mail server **without a shell**: create a domain, see the DNS records to publish and whether
they are right, create accounts and aliases, and know whether outgoing mail works.

In scope (English only; the structure allows translations later):

| Area | Pages and actions |
|---|---|
| Sign-in | e-mail and password, sign-out, session timeout, throttling |
| Domains | list, create, disable/enable; **after creation the DNS page opens at once** |
| DNS page | the generated records (MX, SPF, DKIM, DMARC, SRV/autoconfig, ownership TXT) in a copyable table with a status per record and a **Check now** button; the last check and the history of status changes |
| Accounts | list per domain, create, set password, disable/enable |
| Aliases | list, add, remove |
| DKIM | **generate** a key for a domain and see its record (the private key is sealed for the outbound service and never shown; section 14) |
| Relay | per tenant: host, port, TLS mode, user name and password (sealed, write-only), SPF include, **Test connection** (section 14) |
| Outbound | the state of the relay (the `relay check` result), the queue length and the oldest queued message |
| First start | the setup page that creates the first platform administrator with a one-time token (section 14) |

Out of scope for the first version: quotas and plans, lists management UI, per-domain TLS certificates, audit search, 2FA
(hooks are designed in, see section 5), webmail, any setting that is a deployment secret of the platform (S3, the database, the TLS keys,
the sealing key of the outbound service: these stay in configuration files and environment, as decided; the relay credentials of a
tenant are not platform secrets, see section 14).

## 2. Architecture

```
browser ──HTTPS──> edge (HAProxy, own host name, IP allow-list) ──mTLS──> rusty-admin ──> Postgres (own role)
   Svelte 5 SPA                                                              │
   (static files)                                                            └─> Valkey (sessions, optional)
```

* **One service, one image** (`rusty-admin`, number 7 in the image design). It serves the built Svelte files **and** the JSON
  API on the same origin, so there is no CORS and the cookie can be `SameSite=Strict`.
* **Crates** (each with one reason to change):
  * `rusty-http`: the HTTP server as a Rusty-owned seam. It wraps the HTTP library (section 3) behind `src/adapter` (the
    wrapped-libraries gate applies, as for `smtp-proto` and `fred`) and offers our own small types: request, response, router,
    limits, cookies, security headers, a body reader with a hard size cap. Used by the admin now and by the webmail later.
  * `rusty-admin`: the handlers and the session logic over the existing `Directory` facade, the DNS repository and the
    job queue. No SQL, no HTTP library types.
  * `web/`: the front end (`web/admin`, and later `web/webmail`), with shared code in `web/ui` (API client, components) under
    npm workspaces. It is not part of the Cargo workspace.
* **No new auth path in the core**: the API calls the same `Directory`, `DnsRepo` and `JobQueue` methods the CLI uses, so a rule
  enforced there (tenant isolation, address normalisation, password length) holds for the web too.
* **Assets are embedded in the binary** (generated at build time from `web/admin/dist`), with a hash in each file name for
  `Cache-Control: immutable` and a content hash as `ETag`. The image holds one executable and nothing else to keep in step.

## 3. The HTTP stack (the one new dependency family on the network path)

The workspace has no HTTP server today, so this is a real decision, not a detail. Requirements: HTTP/1.1 (the edge speaks to
the backend over mTLS; HTTP/2 to the browser is the edge's job), request-line, header and body limits, timeouts, graceful
shutdown, no OpenSSL, no `aws-lc`, rustls with `ring` like everything else.

Recommendation: **`hyper` 1.x with `hyper-util` and `http-body-util`**, behind the adapter, plus a router of about 150 lines of our own.
`axum` would give routing and extractors for free but brings `tower`, `tower-http` and more macros and generics onto the
network path for a handful of routes; we would keep its types out of the handlers anyway. Writing our own HTTP parser is not
on the table (a parser is attack surface; `hyper` is the most reviewed one). Decided by the owner (Q2): `hyper` with our own router.

Limits fixed in code and covered by tests: header block 16 KiB, at most 64 headers, request body 64 KiB for the API (nothing in the
first version uploads files), 10 s to receive the head, 30 s per request, 256 concurrent connections, 32 per source address,
`Connection: close` after an error response. Anything above is answered with a fixed status and no body from the handler.

## 4. The API

JSON under `/api/v1/`, one verb per meaning, plural nouns, ids in the path are the ones the CLI already uses (domain name,
address). Every response has `Cache-Control: no-store` and `Content-Type: application/json`; errors are
`{"error":"<fixed code>"}` with a fixed code per case (`invalid`, `exists`, `not_found`, `forbidden`, `throttled`, `busy`) and never
carry backend text, SQL, paths or what the user sent.

| Method and path | Does |
|---|---|
| `POST /session` / `GET /session` / `DELETE /session` | sign in, who am I (and the CSRF token), sign out |
| `GET /domains`, `POST /domains` | list, create |
| `POST /domains/{d}/disable`, `/enable` | status |
| `GET /domains/{d}/dns` | the generated records with their last status |
| `POST /domains/{d}/dns/check` | run the check now (see section 7); returns the new statuses |
| `GET /domains/{d}/accounts`, `POST /domains/{d}/accounts` | list, create |
| `POST /accounts/{a}/password`, `/disable`, `/enable` | account actions |
| `GET /domains/{d}/aliases`, `POST ...`, `DELETE /aliases/{a}` | aliases |
| `POST /domains/{d}/dkim` | generate a key, return the public record only |
| `GET /outbound` | relay state and queue figures |

Pagination by `after` cursor and a fixed page size, as `Directory` does today. The JSON shapes are the contract between the two
halves (section 8).

## 5. Identity, roles, sessions, CSRF

**Who may enter.** The directory has no administrator concept today. Proposal: an administrator is an ordinary account (e-mail
and password, Argon2 as now) that has a row in a new table `admin_grants(account_id, domain_id NULL)`: `domain_id` NULL means
platform administrator (all domains, may create domains), a value means administrator of that one domain only. The first platform
administrator is created from the shell (`rustyctl admin grant <addr>`; also the recovery path), never by the web. Enforcement is in `rusty-admin`
before any call to `Directory`, and again in the repository queries (a domain administrator's queries carry the domain id), so a
missing check in a handler cannot leak another tenant's data. The migration number is assigned when it is written.

**Sessions.** A session id is 256 random bits (OS random generator) in a cookie named `__Host-rusty_admin`
(`Secure; HttpOnly; SameSite=Strict; Path=/`, no `Domain`). The server keeps only `SHA-256(id)` as the key, and the value
(account, grant, CSRF token, created, last seen). Sessions live in the **`Kv` store**: Valkey when `[valkey]` is configured
(V3 of the Valkey plan, fail closed: a session that cannot be checked is not honoured), otherwise the in-process `MemoryKv`
(a restart signs everyone out, acceptable for a single admin instance). Idle timeout 30 minutes, absolute 8 hours, a new id on
sign-in, every session of an account dropped when its password changes or it is disabled.

**CSRF.** `SameSite=Strict` is the first layer. Second: every state-changing request must carry `X-CSRF-Token` equal to the
token of its session (given by `GET /session`), `Content-Type: application/json`, and an `Origin` header that equals the
configured public origin (a missing or different `Origin` on a mutating request is refused). All three are tested separately.

**Sign-in throttling.** The existing `AuthThrottle` (per source address and account name) now; the Valkey limiters (V2) replace its state
when they exist, with the in-process fallback. Constant work for unknown and wrong users (a dummy Argon2 verification), one
fixed error for both, as the IMAP and submission logins do.

**2FA (TOTP)**: not in the first version; the session record and the sign-in flow leave a step for a second factor, and the edge
IP allow-list for the admin host name (already in the container spec) is the interim control. Decided by the owner (Q4): not now.

## 6. Browser-side security

* Response headers on every response: `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self';
  img-src 'self'; connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'`
  (no inline script, no inline style, no `eval`), `Strict-Transport-Security`, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin`,
  `Permissions-Policy` with everything off.
* The Svelte compiler escapes text; `{@html}` is **forbidden by a lint rule that fails CI**, there is no markdown rendering and no
  user-controlled URL is placed in an `href` without a scheme check.
* Nothing secret is ever in a response: no password hash, no DKIM private key, no relay or database setting. The DKIM page
  shows the public record only.
* No third-party script, font or image is loaded; no analytics.

## 7. The DNS page needs the validation engine first

"Check now" and the per-record status are task W1 (the validation engine of U10 and the `dns.check` job), which is not built
yet; the record generator (`rusty_core::dns`) and the state tables (migrations 0014, 0015) already are. So the order is: first
the engine and `rustyctl domain dns` / `rustyctl check <domain>` (useful on their own), then the web page on top. "Check now" runs
the engine inline with an overall timeout of 20 s and a limit of one running check per domain; the daily job and the page use
the same function. The page says plainly that a check from inside the network is not proof that the ports are reachable from
outside.

## 8. The front end

* Svelte 5 (runes) and TypeScript, built with Vite. **No SvelteKit** (no server runtime, no router magic: a small hash-less
  client router of our own, ten routes). Runtime dependencies in the shipped bundle: none beyond Svelte's compiled output.
* Build and test tooling is the supply-chain risk of this change and is treated as such:
  `npm ci --ignore-scripts` from a committed lockfile (integrity hashes), exact versions, a **dependency allow-list gate** in CI
  (like `wrapped-libs.txt`: a new package fails the build until the list is edited in a reviewed change), `npm audit`, a
  licence check, the Node image pinned by digest in `deploy/images.lock`, and a **reproducibility check** (build twice, compare
  the hashes of the output). The tools: `svelte`, `vite`, `@sveltejs/vite-plugin-svelte`, `typescript`, `svelte-check`, `eslint`
  with `eslint-plugin-svelte`, `vitest` with `@testing-library/svelte` and `jsdom`. A browser test runner is **not** in the
  per-commit path (see testing).
* **One contract, checked from both sides.** The JSON examples of every endpoint live in `web/contract/*.json`. A Rust test
  fails when a handler's output differs from its file (and regenerates it on request); `vitest` uses the same files as mock
  responses for the components and checks them against the TypeScript types. A change on one side that is not made on the other
  fails a test on one of them.
* **Every user-facing string lives in a message catalogue** (decision Q6), never in a component. `web/ui/src/i18n/en.ts` is a
  typed object of keys to messages with `{name}` placeholders (`"domain.created": "Domain {name} was created"`); each further
  language is a file that must satisfy the **same type**, so a missing or extra key fails `svelte-check` and CI. Components call
  `t("domain.created", { name })`; plurals use `Intl.PluralRules`, dates and numbers `Intl.DateTimeFormat` / `Intl.NumberFormat`
  (no hand-written formatting). A lint rule fails the build on a string literal in a template or an `aria-label` that is not a key.
  The API never sends text meant for people: it sends the fixed error codes of section 4, and the client maps each code to a catalogue
  key, so the server needs no translation. Languages are separate chunks loaded on demand (and embedded like every asset); the
  choice comes from a stored preference, then `Accept-Language`, then English; the page's `lang` attribute follows it. Only
  `en` ships in the first version; the test suite includes a pseudo-locale (every message wrapped and lengthened) that is
  rendered in the component tests to catch truncated layouts and missing keys.
* Accessibility and plain behaviour: real forms and buttons, labels, focus handling after a navigation, keyboard use, visible
  focus, contrast (the `design:accessibility-review` checklist, WCAG 2.1 AA) and a "copy" button that has a text fallback.

## 9. Testing

Written first, each failing for its stated reason before the code exists:

* **API over a real listener** (Rust, Postgres 18, a fake resolver): sign-in works and fails with one error for a wrong password
  and an unknown user; a request without a session is refused everywhere; mutating requests without a CSRF token, with a wrong one,
  without the JSON content type, or with a foreign `Origin` are each refused; a domain administrator cannot read or change another
  domain, its accounts, its records or its DNS state (every endpoint, table-driven); a disabled account's session stops working;
  session timeout and absolute limit with a manual clock; the limits of section 3 (oversized head, body, slow head); the response
  headers of section 6 on every route including errors and the static files; no response contains the strings `password`,
  `PRIVATE KEY` or a hash prefix; HTML-escaping and IDN names end to end.
* **Fuzzing**: the request path and the JSON decoders get fuzz targets and join the nightly fuzz job.
* **Front end** (vitest): each page against the contract files; error and empty states; the copy button; the router; a test that
  scans the bundle for `eval`, `new Function` and inline-script patterns.
* **Browser end to end** (Playwright, against the compose stack: sign in, create a domain, see the records, check, create an
  account, sign out): runs **nightly with the edge suite**, not on every pull request (it downloads a browser, which is more
  supply chain; pinned by version and checksum in the workflow). The same flow then also exercises the edge's HTTPS routing.
* CI gains a `web` job (path-filtered by "what changed"): type check, lint, unit tests, build, bundle budget (a size ceiling that
  fails the build), allow-list, audit, reproducibility.

## 10. Order of work

| Step | Content | Depends on |
|---|---|---|
| R | **the runtime extraction and role-based configuration of the image design (its steps 2 and 3)**: the start-up code that `rusty d` holds today moves into a `rusty-runtime` library so that `rusty-admin` is its own binary from its first commit (decision Q1); this also unblocks every other service split | the image design |
| A | W1: the DNS validation engine, `rustyctl domain dns` and `check` | nothing (the generator exists) |
| B | `rusty-http`: the adapter, limits, headers, router, cookies, with its tests and fuzz targets | nothing |
| C | `admin_grants`, sessions over `Kv` (Valkey V3), sign-in with throttle, CSRF, the session API, the `rusty-admin` binary | R, B; Valkey V1 (done) |
| D | the domain, account, alias, DKIM, DNS and outbound endpoints with the contract files | A, C |
| E | `web/`: tooling and CI gates first, then the shell and sign-in, then the pages | contract from D (pages can start on the contract files) |
| F | packaging: embedded assets, the Dockerfile stage, the compose service and the edge route, the browser test | E |

R, A, B and E's tooling are independent of each other and can proceed in parallel. Estimate for a usable first version with a single
implementer: **about 6 to 7 weeks** (the extraction R adds about a week to the earlier estimate, and it is work the other service
splits need anyway), of which the front end is about half. It is an estimate, not a measurement.

## 11. How it runs

`rusty-admin` is its own binary and image from its first commit (decision Q1), a thin `main` over `rusty-runtime`, with the
role-specific configuration (`[admin]`, `[valkey]`, the store section with its own Postgres role) and nothing from the mail
services. There is no interim "service inside the shared process": step R comes first so that the exception is never made. In
development it runs as one more container of the compose stack.

## 12. The six questions and the owner's answers (2026-11-14)

| # | Question | Answer |
|---|---|---|
| Q1 | Start the admin inside the existing process until the image split lands, or as its own binary from the first commit? | **its own binary from the first commit** (so step R comes first) |
| Q2 | HTTP library: `hyper` with our own thin router, or `axum`? | **`hyper` + own router** |
| Q3 | Sessions in the `Kv` store (Valkey when present, in-memory otherwise), or always in Postgres? | **`Kv`** |
| Q4 | TOTP second factor in the first version? | **not now** (the edge IP allow-list is the interim control; the sign-in flow keeps a place for it) |
| Q5 | Embed the front end in the binary or serve it from a directory? | **embed**: fewer reads and writes, and the interface files cannot be edited on the server |
| Q6 | English only, or a structure for later translation? | **a modular message-catalogue model with placeholders**, so another language is a new file (section 8); only English ships first |

## 13. Risks

* The npm tool chain is a large dependency tree for a project that otherwise counts its crates. The gates of section 8 reduce it; they do not remove it.
* `hyper` and its helpers are new on the network path; the adapter and the fuzz targets are the answer, and the limits are ours.
* The estimate is the weakest number here: the front end is the part of this project with no code yet, and a design that looks right on paper tends to grow in the browser.
* `admin_grants` is a new privilege surface: it needs its own tenant-isolation tests before any endpoint is reachable, and a review focused on it.

## 14. Secrets entered or generated in the web, the first administrator, per-tenant relay (owner, 2026-11-14, later)

These decisions change three lines above (DKIM generation, the relay password, how the first administrator is made) and are
recorded here so that the rest of the document is read with them.

### 14.1 One mechanism: sealed secrets (HPKE)

A secret that the web **creates or receives** (a DKIM private key, a tenant's relay password) must be usable by the outbound service and by
nothing else, **not even by the admin that stored it**. So it is encrypted to a public key whose private half only the outbound service has
(HPKE, RFC 9180: a sealed box). The admin can write a secret and overwrite it; it cannot read it back. A compromised admin therefore leaks
no signing key and no relay password, which is what the earlier "secrets only where they are used" rule was for.

* The outbound service has one **sealing key pair**. Its private half is a deployment secret (a file, mode 0600, like the TLS keys; never in the
  database, never in the web); its public half is in the database (or the admin's configuration) and is shown to the operator. Rotation: a new
  key pair, the outbound service accepts the old and the new for unsealing, secrets are re-sealed when rewritten.
* Table `sealed_secrets` (migration to be numbered when written): `(owner kind, owner id, purpose)` as the key, the HPKE ciphertext, the
  public key id it was sealed to, `created_at`. The purpose and the owner are bound into the HPKE context (`info`/`aad`), so a ciphertext cannot be
  moved to another domain or used for another purpose. A write replaces the whole row; the admin role gets `INSERT`, `UPDATE` and `DELETE` and **no
  `SELECT`** on this table, only the outbound service's role can read the ciphertext.
* **Library**: no hand-assembled primitives. A vetted HPKE crate behind an adapter (`wrapped-libs.txt`), pure Rust, no `aws-lc`. The candidates
  are the RustCrypto `hpke` family and `rust-hpke`; the choice is made by a short spike (licence, maintenance, advisories, the size of the new tree,
  ciphersuite DHKEM(X25519)/HKDF-SHA256/ChaCha20-Poly1305 or AES-GCM) before any code, and recorded in the pull request.
* The outbound service reads DKIM keys and tenant relay credentials from the database (a small cache with a short lifetime) and unseals them in memory.
  Key **files** in `dkim_dir` stay supported (the `rustyctl` path and existing installations); the keyring looks at the database first.

### 14.2 DKIM keys generated in the web

The admin generates an ed25519 key (the library that makes them today), seals the private key to the outbound service (14.1), stores it with the public
half in `domain_dkim_keys` (already there, write once), and shows the **record to publish** in the standard record table of the DNS page (name, type,
value, a copy button, status from the DNS check). The private key is never displayed and never leaves the process unsealed. The same selector rules
apply: a key is made once and never replaced; a new key is a new selector. RSA keys can still be imported with `rustyctl` (the web does not accept
a pasted private key). `rustyctl dkim generate` keeps working and writes the same places.

### 14.3 The first platform administrator: a setup token

When the admin service starts and **no administrator exists**, it prints one random 256-bit token to **standard output** once (nothing else is
printed with it) and serves the setup page. The page asks for the token and then for the e-mail and password of the first platform administrator.

* The token is kept in memory only, as a hash; it is compared in constant time; a **new token is made at every start** until an administrator exists.
* At most 5 wrong tokens per source address and 20 in all per hour, then the setup page answers `429` for an hour.
* The page is **gone for good** the moment one administrator exists (the route is not registered; the check is a database fact, not a flag in memory).
* It is only reachable through the edge, with the admin host's IP allow-list, like every admin page.
* The risk to know: whoever can read the service's standard output sees the token. It is accepted because the window is the time between the first
  start and the first administrator, and because the token is useless afterwards. `rustyctl admin grant` remains the recovery path.

### 14.4 Relay credentials per tenant

A tenant administrator (or the platform administrator) enters the relay of a domain in the web: host, port, TLS mode (`starttls`, `implicit`, `none` without a
login), user name, password, and the provider's SPF `include:`. The password is sealed (14.1) and can only be overwritten. The platform-wide relay of
`[outbound.relay]` stays as the default for domains without their own.

* New implementation of the `OutboundRoute` seam: `route(sender_domain, rcpt_domain)` reads the sender domain's relay from the database (cached) and falls
  back to the global one. The worker does not change.
* **SSRF policy**, because a tenant now chooses where the outbound service connects: the host must resolve to **public addresses only** (the
  existing `forbidden_address` list plus private ranges, loopback, link-local, the cloud metadata address and CGNAT), checked **again at every
  connection** (a name that resolved to a public address when it was entered may point inside later), and the port must be one of 25, 465, 587, 2525.
  A platform administrator may allow named private hosts (an in-house relay) by setting, not a tenant.
* TLS is verified (`verified` against the platform bundle); a tenant cannot lower it.
* **Test connection**: the admin cannot read the password back, so the test runs in the outbound service: a job (`relay.check`) that unseals the credentials,
  runs the same check as `rustyctl relay check` and stores the result (the fixed category and the time). When the password is first entered the admin
  may run the check at once with the plaintext it just received, before sealing.
* A failed login is shown as such (a fixed category) on the domain page and in the outbound page; mail stays queued, as today.

### 14.5 Order of work

New step **S** before C: the spike and the `rusty-seal` crate (the HPKE adapter, `Sealer` and `Unsealer` with the context binding, the table and its
privileges). Then the outbound service reads sealed DKIM keys and the per-tenant route; then the web endpoints and pages. The first-administrator setup
page belongs to step C (sign-in).

### 14.6 Open: SCRAM

The owner asked whether SCRAM can be tried. Where it would apply (mail client login, web login, the relay) is not decided. Nothing in sections 1 to 14
depends on it.
