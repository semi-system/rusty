# Design: POP3

Status: written 2026-11-17, built right after (the owner asked for POP3 now: `design-admin-parity.md`, D2). Rule of this project: no service is added without its image, its trust boundary and its behaviour when it is down written first; section 2 says why there is no new service.

## 1. What it is for

Some clients and devices (old phones, scanners, a script that fetches mail, a migration from another host) speak POP3 and not IMAP. POP3 gives them the **Inbox** of an account to download, and to delete after download if they choose. It is not a second mailbox protocol: folders, flags, search and sharing stay IMAP's.

## 2. Where it runs: a listener of the IMAP service, a crate of its own

* **No new service, image or database role.** POP3 reads and expunges messages of one account's mailbox, which is exactly what the IMAP service's role (`rusty_imap`) is for: it holds `SELECT` on the mailbox rows and `DELETE` on messages, and the same password check, the same throttle, the same TLS material, the same blob access and the same change feed. A separate service would need the same grants, so it would add an image, a stack entry and a heartbeat for nothing.
* **A crate of its own, `rusty-pop3`**, so the parser and the session machine can be fuzzed and tested without the IMAP service (the layers of `rusty-imap`: a codec with no I/O, a session that answers with bytes, a driver that owns the stream). The IMAP service's daemon starts its listeners next to the IMAP ones (`ListenerKind::Pop3`, `Pop3s`).
* **Ports.** `995` implicit TLS (ended at the edge, like 993) and `110` with `STLS` (passed through with the PROXY protocol, like 143). In `[imap]` (the service's own section, so the connection limits, the PROXY protocol and the trusted edge are the IMAP service's, not a second set to keep in step): `listen_110` and `listen_995`, both off until an address is named.
* **When the service is down** POP3 is down with IMAP (one process); clients retry. When the database is down a login answers `-ERR [SYS/TEMP]` and nothing is deleted.
* **Heartbeat and metrics** are the IMAP service's: the heartbeat detail says how many listeners are up (the count grows by two), and the listener counters carry `listener="pop3"` / `"pop3s"`.

## 3. Protocol (RFC 1939, 2449, 1957, 5034, 6856)

| Command | Behaviour |
|---|---|
| `CAPA` | `TOP UIDL USER RESP-CODES PIPELINING UTF8 EXPIRE NEVER LOGIN-DELAY 0 IMPLEMENTATION rusty`, `STLS` before TLS on 110, `SASL PLAIN` after TLS. |
| `STLS` | on 110 only, before authentication: `+OK`, then TLS; the session starts over (RFC 2595). On 995 or after TLS: `-ERR`. |
| `USER` / `PASS` | **only inside TLS.** On plain 110 `USER` answers `-ERR [AUTH] TLS is required before a password` (a password is never accepted in clear text; as in the IMAP service there is no setting that allows it: only the tests of the listener can). |
| `AUTH PLAIN` | the same check, with the initial response or a continuation; only inside TLS. |
| `APOP` | **not offered**: it needs the plain secret on the server, and passwords here are only stored as argon2 hashes. |
| `STAT` `LIST` `UIDL` `RETR` `TOP` `DELE` `NOOP` `RSET` `QUIT` | as the RFCs say, on the **Inbox** snapshot taken at login. |

Details that matter:

* **Snapshot.** At login the session lists the Inbox once (oldest first, at most 200,000 messages: the rest is not offered and the greeting after `PASS` says nothing of it; a mailbox that large is downloaded in several sessions, the oldest first, as the client deletes). A message that arrives during the session is not part of it; one that another protocol (IMAP) expunges during it answers `-ERR no such message` to `RETR`, and is dropped at `QUIT`.
* **Numbers and sizes.** Messages are numbered 1 to n in the snapshot's order; the size is the message's size in bytes as stored (RFC 5322, CRLF), what `RETR` sends before dot-stuffing.
* **`UIDL`** is `<UIDVALIDITY>.<UID>` of the Inbox (letters and digits and `.` only, at most 70 characters): stable for the life of the message, and a new `UIDVALIDITY` makes a client download again, as it should.
* **`RETR` and `TOP`** send the message dot-stuffed, ending `.` + CRLF; a bare LF in a stored message is sent as CRLF (the store keeps CRLF, so this does not happen, and the test says so). `TOP n lines` sends the headers, the empty line and at most `lines` lines of the body. Both read the message through the mailbox facade (`get_raw`), one at a time: a session holds one message in memory at once, never the mailbox.
* **`DELE`** marks; **nothing leaves the server before `QUIT`** reaches the UPDATE state, which expunges what was marked in one batch (`expunge_many`). A connection that drops, times out or is cut leaves every message where it was (RFC 1939 section 6). `RSET` unmarks.
* **Concurrency.** RFC 1939 asks for an exclusive lock of the maildrop. This server does not lock: the same account is read by IMAP at once by design, and a second POP3 session would only see the same snapshot and delete the same messages, which is harmless (the second `QUIT` finds them gone). `[IN-USE]` is therefore never answered.
* **The mailbox's other folders** are not reachable. A message the person marked `\Deleted` in IMAP but did not expunge is not in the snapshot (it is as good as gone).
* **Memory.** A `RETR` reserves four times the size of the message from the same budget of messages in flight that SMTP `DATA` and IMAP `APPEND` draw on (the bytes read, the normalised copy, the byte-stuffed reply and its write buffer), before it reads the message: a service short of memory answers `-ERR [SYS/TEMP] busy` and the message stays where it is. The reservation is given back when the reply is made.
* **Quota and policy** are the IMAP service's: a closed or locked account cannot sign in (`-ERR [AUTH]`), and what an account may do is what IMAP lets it do.

## 4. Limits (hostile input and slow clients)

* A command line is at most 512 bytes (RFC 1939: 255 plus the command; 512 leaves room for `AUTH PLAIN` initial responses, 1,024 for that one), a client that sends more is told `-ERR line too long` and the connection ends. Commands are not pipelined beyond 100 per read.
* **Timeouts.** 60 seconds to authenticate (a connection that has not signed in by then is closed), 10 minutes without a command in the transaction state (the RFC's minimum is 10; the autologout timer), and a write that makes no progress for 60 seconds ends the session.
* **Failed logins** go through the shared throttle (`ValkeyThrottle` when `[valkey]` is set, else the process's own), the same counters and the same refusal as IMAP: after the limit `-ERR [LOGIN-DELAY] try later`, and the connection ends. Three bad `PASS` in a session end it too.
* **Connections** are capped per listener and per address (`[pop3]`), refused before the greeting like IMAP's.
* Nothing the client sends reaches a log or an error text; errors are fixed strings (the IMAP rule).

## 5. Edge and deployment

* **Built as described, with these differences:** the configuration is `[imap] listen_110` and `listen_995` (above); the standalone `setup.sh --pop3` adds `compose.pop3.yaml` (the two published ports) and switches on the `#POP3` lines of the templates; the swarm has `POP3=1` in `swarm.env` (the edge publishes 110 and 995 in host mode, the IMAP service listens); `[admin] pop3 = true` is set with it so that the auto-setup lists POP3. The RFC 6186 SRV records of POP3 are **not** in the generated DNS records: the records are per domain and the same for every installation, while POP3 is something one installation runs; a domain owner who runs POP3 adds `_pop3s._tcp` by hand.
* The edge (`haproxy.cfg.in`) gets `pop3s_in` (`bind :995 ssl crt mail.pem`, to `be_pop3s`, `imap:1995`, PROXY) and `pop3_in` (`bind :110`, to `be_pop3`, `imap:1110`, PROXY), next to the IMAP frontends. The swarm stack and the compose file publish `110` and `995`, and the firewall note of `docs/first-mail.md` lists them. The DNS page lists the RFC 6186 SRV records `_pop3s._tcp` (995, optional) and `_pop3._tcp` (110, optional).
* **Off by default.** `[pop3]` absent means no POP3 listener (nobody opens a port they did not ask for); `swarm.env` has `POP3=1` and the standalone setup `--pop3`. When on, the edge frontends and the published ports appear.
* The auto-setup answers list POP3 only when POP3 is on: Thunderbird's autoconfig adds a `pop3` incoming server (995, TLS); Outlook's Autodiscover adds a `POP3` protocol after IMAP; the Apple profile for POP3 is `/mail/config.mobileconfig?emailaddress=...&protocol=pop3` (its own account, so one address may have both). The answers are the same for every domain.

## 6. Tests

* `rusty-pop3`: the parser (every command, bad arguments, a line too long, a null byte), the session machine over a fake mailbox (snapshot, numbering, `LIST`/`UIDL` with and without an argument, `RETR` dot-stuffing and a message that ends without CRLF, `TOP` with 0, with more lines than there are, `DELE` twice, `RSET`, `QUIT` expunging exactly the marked ones, a dropped connection expunging nothing, an IMAP-expunged message), proptests on the dot-stuffing round trip, fuzz targets for the command parser.
* The service: a real listener and a real database: `USER`/`PASS` in TLS and refused in clear text, the throttle, the 200,000 cap (with a smaller limit set for the test), a message delivered by SMTP and downloaded and deleted by POP3 and then gone for IMAP, two sessions at once, the idle and the login timeouts, `STLS`.
* The edge: the HAProxy configuration check with the new frontends (`scripts/ci-gates.sh`), and the swarm static test (ports, the off switch).

## 7. Order of work

1. This page. 2. The crate `rusty-pop3` (parser, session machine, driver) with its tests and fuzz target. 3. The listener in the IMAP service, `[pop3]` in the configuration, the metrics and the heartbeat. 4. The edge, the standalone and swarm deployment, the DNS records and the autoconfig answer, the documentation (`first-mail.md`, `swarm.md`).
