# Design: live updates (server-sent events)

Status: written 2026-11-17 at the owner's request (SSE, so that pages receive information continuously and nothing needs a reload, in the whole system). Built in steps (section 9). The webmail (`design-webmail.md`, step 6) uses the same transport and the same rules; this page builds it for the admin console first.

## 1. What it is for

A page shows a state that changes without the person doing anything: the result of a storage or DNS check made by another service, the queue figures, the health of the services, an account that another administrator locked. Today the person reloads. With this design the page learns that something changed and refreshes the part that changed.

## 2. The rule: an event is a hint, never data

The stream carries **topics that changed** (`storage:example.test`), nothing else: no record, no address, no count. The page answers a hint by asking the ordinary API again. So:

* authorization and the isolation between tenants stay where they are (every API endpoint decides what the caller may see); the stream cannot leak what the API would not show, because it holds nothing;
* a lost or repeated hint is harmless: the page refreshes once more or one change later; after any reconnection the page refreshes everything it shows (the server sends a `resync` event first);
* no ordering or delivery guarantee is needed, which is what lets the sources below be simple.

## 3. Transport

`GET /api/v1/events`, `Content-Type: text/event-stream`, `Cache-Control: no-store`, `X-Accel-Buffering: no`. The session cookie authenticates it as any `GET` (`401` without one; no CSRF token, it changes nothing). Frames:

```
event: resync            sent first on every connection: refresh everything you show
data: {}

event: changed
data: {"topics":["storage:example.test"]}

: keep-alive             a comment every 20 seconds (so proxies and the edge keep the connection)
```

* **Scope.** A platform administrator receives every topic; a domain administrator only the topics of its own domain (the topic names carry the domain name, the hub filters by the principal's domain). Topics without a domain (`monitor`, `admins`) go to platform administrators only; `domains` goes to both (a domain administrator's list shows its own domain's status).
* **The session ends the stream.** The hub looks at the session on connect and on every keep-alive; a revoked or expired session, a revoked grant or the shutdown of the service ends the stream. A stream lives at most 30 minutes; the browser's `EventSource` reconnects by itself (and gets `resync`).
* **Limits.** At most 4 streams per session and 256 per process; a further connection is `429 throttled`. Each stream has a small queue (64 hints); a stream that cannot keep up is not buffered without bound: its queue is merged into one `resync` (hints are idempotent).
* **No new port, no new service.** It is a route of the admin service, behind the same edge route as the rest of the API. The edge must allow a long response: `timeout server` (and `timeout tunnel`) of the admin backend at least 90 seconds with the 20 second keep-alive (swarm and standalone templates).
* **HTTP server.** `rusty-http` gets a streaming body (`Response::stream`): a bounded channel of byte chunks behind a hyper body. The request timeout covers the handler up to the response head, as now; the stream is ended by its own rules above and by the service's shutdown token. Other responses are unchanged.

## 4. Topics

| Topic | A page shows | Changes when |
|---|---|---|
| `domains` | the list of domains and their status | a domain is created, enabled or disabled |
| `accounts:<domain>` | accounts and aliases of a domain | an account or alias is created, changed or removed |
| `dns:<domain>` | DNS records and their check | a check finishes; DKIM keys change |
| `outbound:<domain>` | the relay, the queue figures of the domain | a relay is set, checked or removed; a job changes state |
| `storage:<domain>` | the store, the pool, their checks | a store or backend is set, checked or removed |
| `admins` | the administrators | a grant is made or removed |
| `monitor` | services, instances, queue, storage and DNS summary | every 5 seconds while a platform administrator is connected |

## 5. Sources

1. **This service's own mutations.** After an administrator's request changes something, the handler publishes the topic in memory: the person who made the change sees the result at once, on every tab.
2. **Other processes.** Results written by the outbound service (a storage check, a DNS check, a relay check) and changes made by another replica of the admin service or by `rustyctl`. The database announces them: statement triggers `NOTIFY rusty_admin` with the topic as payload (a migration, like `0017_notify_changes.sql`: no `SECURITY DEFINER`, nothing but `pg_notify`; a notification is a hint). Each admin process keeps ONE `LISTEN` connection (the change feed's pattern: separate from the pool, retried with backoff, a `resync` to every stream when it comes back).
3. **The timer.** `monitor` is not announced by the database (heartbeats change every few seconds): the hub emits it every 5 seconds, only while a platform administrator is connected.

If the `LISTEN` connection is down, the streams still work for sources 1 and 3, and the page's own actions work as before: the feature degrades to what exists today.

## 6. Page behaviour (the console)

One `EventSource` for the whole console (`web/admin/src/lib/live.svelte.ts`), opened after sign-in and closed at sign-out. A page says which topics it shows (`live.watch(['storage:' + name], refresh)`); a hint for such a topic runs `refresh` after a short delay that merges a burst into one request (250 ms). `resync` runs every refresh. The page keeps what it has on screen while it refreshes (no flicker) and shows no error for a failed background refresh (the next hint retries; an error shows only for what the person asked for). A small "live" mark in the top bar says the stream is connected; when it is not, the page works as before.

## 7. What the stream must never do

Carry data, a secret or a name of another tenant; keep a session alive (an open stream does not extend the idle timeout of the session); grow without bound (queues are bounded, merged into `resync`); outlive the session, the grant or the service; be a way to count what exists (a domain administrator never receives a topic of another domain, not even an empty one).

## 8. Tests

* `rusty-http`: a streamed body reaches the client in chunks while the handler continues; the stream ends when the sender is dropped; a client that goes away stops it.
* Admin: no session is `401`; the first event is `resync`; a platform administrator receives every topic and a domain administrator only its own (the cross-tenant matrix); a revoked session or grant ends the stream; the limits (`429`); a slow stream is merged into `resync`; the keep-alive.
* Database (step 2): each trigger announces its topic once per statement; the role of the service can `LISTEN`; a rolled-back change is not announced.
* Console: a hint refreshes the page that watches the topic and no other; a burst is one request; reconnection refreshes; no stream, no error.

## 9. Order of work

1. This page. `rusty-http` streaming, the hub, `GET /api/v1/events`, publishing from the handlers, the console's live store and pages, the edge timeouts. After this step the lists refresh after any administrator's action (on every tab and for every administrator of the same process) and `monitor` ticks.
2. The `NOTIFY` migration and the `LISTEN` connection, so that results written by the outbound service (the storage check that started this request) and by other replicas arrive without a reload.
3. The webmail uses the transport for new mail (`design-webmail.md`, step 6): its own topics (`mailbox:<id>`) from the existing `rusty_mailbox` feed.
