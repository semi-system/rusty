# Design: the webmail (`rusty-webmail`, SolidJS)

Status: **in progress**; the scope (a complete webmail, SolidJS) was decided 2026-11-16. Built so far (step 1 of section 9): the configuration role `webmail` (`[webmail]`: listen, public origin, allowed peers and hosts, PROXY protocol), the crate `rusty-webmail` with the session and sign-in (one fixed 401, Origin, JSON and CSRF checks, the throttle shared with IMAP, a disabled account signed out at its next request), `GET /api/v1/me`, the health check, and tests against a real Postgres. Then (step 2, read path): its own database role, the image and the swarm service (`WEBMAIL_HOST`), and the read path: `GET /api/v1/folders` (with the counts), `GET /api/v1/folders/{f}/messages` (newest first, by cursor), `GET /api/v1/messages/{m}` (headers, the plain text, the attachments) and `/raw` (the original, always a download), every one through the mailbox of the account behind the session. **Plain text only so far**: HTML needs a sanitiser and a sandboxed frame (a step of its own), an HTML-only message says so and offers the original. Not yet: attachments as downloads, threads and search, the write path, the front end, a tenant's own blob store (it needs a key this service does not hold). Rule of this project: no service is added without its image, its trust boundary and its behaviour when it is down written first. The owner writes Vietnamese; the interface texts live in message catalogues (English first, like the admin web).

## 1. What it is

One more service, `rusty-webmail`: a static binary with the front end embedded (the pattern of `rusty-admin`), its own image, reached by the browser through the edge on its own host name (`@WEBMAIL_HOST@`, HTTPS with a certificate from the edge's ACME, see decision 2026-11-16). It is **not** a second copy of the admin web: the admin administers domains, the webmail is for the people whose mail it is.

What a person can do (v1 = everything below; the order is the order of the pull requests, section 9):

| Area | Features |
|---|---|
| Account | sign in with the mail address and password, sign out, sessions that end at idle and absolute timeouts, change password, the list of the account's active sessions with "sign out everywhere" |
| Folders | the folder tree with unread counts, create, rename, move, delete (system folders protected), subscribe, empty Trash and Junk |
| List | paged and sorted (date, from, subject, size), conversation view (threads from the existing `find_thread`), flags (read, starred, answered), select many, move, copy, delete, mark, "load more" without page numbers |
| Search | in the folder or everywhere; from, to, subject, body, date range, has attachment, flagged, unread (the store's search, no new index in v1) |
| Reading | headers, plain or sanitised HTML, inline images of the message (`cid:`), attachments (download, preview of images and PDFs in a sandboxed frame), remote content blocked until the person allows it for this message or this sender, "show original" (the raw message as a download), print |
| Writing | compose in plain text or rich text, reply, reply all, forward (inline or as attachment), drafts saved automatically to the Drafts folder, attachments (drag and drop, size check before upload), several identities with a signature each, address completion from the people the account has written to and from the address book, "send later" through the outbound queue's `run_after` |
| Address book | contacts and groups (new tables), import and export vCard, completion in the composer |
| Settings | identities and signatures, language, time zone, density, theme (light, dark, system), keyboard shortcuts, notification of new mail while the page is open (the IMAP IDLE path through the same change feed), automatic reply (vacation) when the delivery side has it, mail rules when the delivery side has them (both are server features first: a later design, the pages are placed behind the capability) |
| Quality | keyboard first (a shortcut map like the common webmails), screen-reader labels, works at phone width, no horizontal scroll, offline notice, errors as fixed codes mapped to texts (the admin web's rule), no secret on the page |

Not in v1: calendar, tasks, end-to-end encryption (PGP, S/MIME), a mobile app, shared mailboxes and delegation (the store's ACL design has them; the web comes after the protocol side).

## 2. Trust boundary and what it may touch

* The service is reachable from the internet (through the edge) and parses hostile content (every message). It holds **no** sealing or unsealing key, no DKIM key, no relay credential: sending a message is an enqueue of an outbound job (`JobOutbound`), exactly as the submission service does; signing and delivery are the outbound service's.
* It reads and writes through the same facades as IMAP (`Mailbox`, `Directory`, `JobQueue`, the blob router): **no SQL in the service** (`check-raw-db` stays green). The blobs of a tenant's S3 store need that store's keys, which are sealed to the unsealing key; so the webmail needs `[unsealing]` like the other services that open bodies (decision 2026-11-15). That key is the main thing a compromise of this service would take; the exposure is the same as IMAP's.
* **No presigned URLs and no browser access to the storage** (decision 2026-11-14): every byte of a message goes through the service, which checks that the session's account owns the message before it reads it.
* Its own Postgres role (`rusty_webmail`, per-service roles) with only what it needs: the mailbox and folder tables, the directory lookups, the job queue insert, its own tables (contacts, identities, settings, sessions' labels).
* Sessions live in the shared key-value store (`[valkey]`; without it in the process, and a restart then signs everyone out): random id, only the SHA-256 stored, CSRF token per session, `Secure; HttpOnly; SameSite=Lax` cookie `__Host-` prefixed, idle 30 minutes and absolute 8 hours (the admin web's `SessionStore`, reused). The password is verified by the directory's authenticator and the failure throttle of the IMAP service (`ValkeyThrottle`: one counter for a pair across the services).

## 3. Messages are hostile input

* **Parsing is on the server**, in one place: `rusty-mime` (already the parser of the IMAP side) gives the structure; the HTML body is sanitised **server-side** by an allow-list sanitiser behind an adapter crate (the wrapped-library rule: a Rusty-owned trait `HtmlSanitizer`, the library only inside `adapter/`). Output allows a fixed set of tags and attributes, no scripts, no event handlers, no `style` attribute values beyond a small safe set, `target` and `rel` forced on links, `cid:` and `data:` images rewritten to the attachment route, **every remote URL removed** (images, CSS, fonts, links in `<a>` stay as links but go through a "you are leaving" interstitial that shows the real destination).
* The browser receives the sanitised HTML and puts it in a **sandboxed iframe** (`sandbox` without `allow-scripts`, `allow-same-origin` only when the CSP is applied by the server) served from the webmail origin with its own CSP: `default-src 'none'; img-src 'self'; style-src 'unsafe-inline'`. Even if the sanitiser misses something, the frame cannot run script and cannot read the application.
* Attachments are served with `Content-Disposition: attachment` (and `inline` only for an allow-list of image types and PDF in the sandboxed preview), `X-Content-Type-Options: nosniff`, a content type chosen from the part's declared type checked against its bytes for the preview types, and a separate cookie-less path-scoped route so a hostile HTML attachment cannot act as the application.
* Remote content ("load images") is a per-message and per-sender choice kept in the account's settings, never a global default. No tracking pixel is fetched by the server on the person's behalf (no proxy in v1: a proxy would turn the service into an open fetcher).
* The composer sends **plain text and sanitised HTML** built from a small, fixed rich-text model (no arbitrary HTML from the editor reaches the outbound message unfiltered); size limits per message and per attachment follow `max_message_bytes`.

## 4. The API

A JSON API under `/api/v1`, with a contract file (`contract/v1.json`) checked by tests and a TypeScript client generated from it (the admin web's pattern: the shapes of the contract are the types; a mismatch fails a test). Every state-changing request needs the session, the CSRF token and an `Origin` that matches. Representative endpoints (all scoped to the session's account; another account's object is the same `404` as a missing one):

* `POST /session`, `DELETE /session`, `GET /me`, `GET /sessions`, `POST /password`
* `GET /folders`, `POST /folders`, `PATCH /folders/{id}`, `DELETE /folders/{id}`
* `GET /folders/{id}/messages?cursor=&sort=&q=`, `POST /messages/actions` (move, copy, flag, delete: many ids, one call, one transaction), `GET /messages/{id}` (headers, structure, sanitised body), `GET /messages/{id}/raw`, `GET /messages/{id}/parts/{n}`
* `GET /threads/{id}`, `GET /search?q=...`
* `POST /drafts` and `PUT /drafts/{id}` (the Drafts folder), `POST /send` (enqueue; answers once the job is queued), `GET /outbox` (the account's queued and failed messages)
* `GET/POST/PUT/DELETE /contacts`, `/groups`, `/identities`, `GET/PUT /settings`
* `GET /events` (server-sent events: new mail in a folder, folder counts; the change feed again, one subscription per open page, bounded per account)

Paging is by cursor, never by page number; limits on every list (the admin web's rule); a request body over a few MiB is refused (attachments go through a streamed upload route with its own cap).

## 5. The front end (SolidJS)

* **SolidJS** with Vite and TypeScript, built into static files that the binary embeds (reproducible build checked like the admin web's: `check-reproducible.sh`), no CDN, no runtime fetch of code, a locked dependency list with an allow-list file (`deps.allow`) checked in CI, a bundle size budget.
* Structure: `api/` (client generated from the contract), `state/` (stores: session, folders, list, thread, composer, settings), `components/` (list, reader, composer, editor, folder tree, contact picker, dialogs), `i18n/` (message catalogue, `en` first, a pseudo-locale in tests to catch hard-coded strings and truncation), `routes/` (a small router as in the admin web).
* The list is virtualised (thousands of messages stay fast); the reader and the composer are the two heavy components and are loaded on demand.
* The rich-text editor is a small component of ours over a fixed model (bold, italic, underline, lists, links, quote, signature), not a general HTML editor: that keeps what can be sent small and checkable.
* Tests: vitest with the Solid testing library for components and stores, a fake server built from the contract (the admin web's `fakeServer`), accessibility checks (axe) in the component tests, and the Rust side's own integration tests (below).

## 6. What can go wrong, and what the person sees

| Failure | Behaviour |
|---|---|
| The database is down | sign-in and every call answer a fixed `unavailable`; the page shows a banner and keeps what it already shows; nothing is lost that was not sent |
| The key-value store is down | existing sessions keep working from the service's own short cache of verified sessions for a few minutes, new sign-ins are throttled by the in-process floor, never switched off |
| A tenant's S3 store is down | the messages in it show a per-message error ("temporarily unavailable"), the rest of the folder works (the pool and breaker of the storage layer) |
| The outbound queue is full or the domain is over its limits | `send` answers a fixed code, the draft stays in Drafts |
| A message cannot be parsed | it opens as "show original" with a banner, never as an error page |
| The sanitiser rejects a body | the text part is shown, or the original download is offered |

## 7. Tests that make it trustworthy

* The isolation suite of the IMAP and SMTP services, repeated for every route: an account never reaches another account's folder, message, part, draft, contact or setting, whatever the spelling of the identifier (a table-driven test over the contract's routes, like the admin web's cross-tenant matrix).
* A corpus of hostile messages (script in HTML, `javascript:` links, CSS exfiltration, malformed MIME, huge nesting, zip bombs in attachments, header injection in the composer): the output of the sanitiser is checked with an independent parser for forbidden content, and the fuzz targets of `rusty-mime` are reused.
* The contract test (every route listed, each answers 401 without a session), the CSRF matrix, the throttle, the session lifecycle.
* An end-to-end test with a real browser (Playwright in the CI web job): sign in, read, reply, search, attach, send and see it arrive on the other account.

## 8. Deployment

* `deploy/docker`: target `rusty-webmail` (distroless static, user `65532`, one executable), `scripts/check-service-image.sh` extended for it. The standalone stack gets a service on the app network (fixed address), and the edge a frontend for `@WEBMAIL_HOST@` (HTTPS only, no network allow-list: it is the public face, protected by the throttle and, later, 2FA), certificate by ACME like the admin host's.
* Configuration `webmail.toml` with only its sections (`[webmail]` listen address and public origin, `[store]`, `[valkey]`, `[unsealing]`, `[blob_s3]` where used); the role matrix gets the `webmail` role.

## 9. Order of work (each a pull request with tests first)

1. `rusty-webmail` crate and binary skeleton: configuration role, the session and sign-in over the shared pieces, `/me`, the contract and the generated client, the image and its checks, the edge entry.
2. Read path: folders, list, threads, message structure, the sanitiser adapter and its corpus, attachments, `raw`, search.
3. Write path: actions (flags, move, copy, delete), drafts, the composer, `send` through the outbound queue, the outbox.
4. The front end of 2 and 3 (list, reader, composer, editor), keyboard map, i18n, accessibility.
5. Address book and identities (migration, repository methods, grants), completion.
6. Settings, remote-content choices, events (SSE), sessions list, password change.
7. The Playwright end-to-end job; documentation (`docs/webmail.md`).

## 10. Points for the owner

1. **Host name**: `webmail.<platform domain>` for everybody, or also `mail.<tenant domain>` per tenant (needs a certificate per tenant: possible now with per-domain ACME at the edge, one more edge frontend per domain). Default proposed: one platform host first.
2. **Single sign-on with the admin web**: none in v1 (two services, two sessions). A domain administrator who also has a mailbox signs in twice.
3. **Vacation replies and mail rules** are server features that do not exist yet; the pages stay hidden until they do (a separate design: Sieve or a simple rule model).
4. **Spam folder training** (move to Junk teaches rspamd) arrives with the filter design's "training" item.
