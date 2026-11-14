# HAProxy H1 verified facts

Verified on 2026-11-03 for the pinned Docker Official Image
`haproxy:3.4.7-alpine3.24`
(`haproxy@sha256:2caaf8b1a1ef67f3f1ea9b721f2821675d3ba4500fb518e23884fecb93e8dbaa`,
image created 2026-10-21). `haproxy -vv` inside the image reports
`HAProxy version 3.4.7-8872a26 2026/10/14` (release date of the build, 3.4 LTS branch,
"will stop receiving fixes around Q2 2031"), built and running with `OpenSSL 3.5.9 29 Sep 2026`.

This is the newest 3.4 patch published on Docker Hub on 2026-11-03 (also 3.2.26, 3.3.17
and 3.0.30 exist; 3.5 is a dev branch). Alpine versus Debian: the Debian `3.4.7-trixie`
image (digest `sha256:64378482a9ad183747a0416fcaaae7f5f46bd7dac9ddd6805aa5db806a2a83ad`)
carries the same HAProxy but OpenSSL 3.5.7 (9 Jun 2026); the Alpine image carries the
newer OpenSSL 3.5.9, is smaller, and HAProxy only uses musl for libc basics (TLS is
OpenSSL). Alpine was therefore kept. An earlier draft pinned 3.4.4; changes between 3.4.4
and 3.4.7 were not checked against release notes (the pin simply follows the newest patch).

The manual read was docs.haproxy.org/3.4 ("HAProxy version 3.4.7-57 - Configuration
Manual"); every directive is additionally checked by the pinned binary with `haproxy -c`.

Primary references:

- [HAProxy release table](https://www.haproxy.org/)
- [HAProxy 3.4 configuration manual](https://docs.haproxy.org/3.4/configuration.html)
- [HAProxy 3.4 management guide](https://docs.haproxy.org/3.4/management.html)
- [Docker Official HAProxy image](https://hub.docker.com/_/haproxy)

## Confirmed configuration facts

| Area | Confirmed fact and evidence | H1 use |
|---|---|---|
| Certificate selection | The 3.4 manual's `strict-sni` entry says TLS "negotiation is allowed only" for an SNI matching a certificate. | All terminated listeners inherit `strict-sni`; absent and unknown SNI fail closed. |
| Certificate list | `crt-list` is a bind option and its filters associate hostnames with already configured PEM paths. | SNI names are fixed in `crt-list.txt`; client SNI is never interpolated into a path. |
| TLS floor | `ssl-min-ver` is valid in the shared bind/server option blocks; cipher and TLS 1.3 cipher-suite lists have distinct directives. | Both sides set TLS 1.2 minimum and explicit cipher policies. |
| Session tickets | The manual says `no-tls-tickets` "disables the stateless session resumption" mechanism. | It is shared by all terminated frontend and backend TLS contexts. |
| Backend verification | Server options support `ssl`, `verify required`, `ca-file`, client `crt`, `sni`, and `verifyhost`; hostname checking is tied to `verify required`. | Every re-encrypted backend uses the pinned internal CA, an edge client certificate, SNI, and `verifyhost`. |
| PROXY v2 | The server option "enforces use of the PROXY protocol version 2" and carries layer 3/4 addresses. | Every mail backend server line uses `send-proxy-v2`; fake backends reject a missing header. |
| Health checks | The manual documents `tcp-check connect`, send/expect rules, `check-ssl`, `check-sni`, and `check-send-proxy`. | STARTTLS paths check the protocol banner; TLS paths perform verified TLS, SNI, client-cert and PROXY setup. |
| Abuse controls | Frontends support IP stick tables with `conn_cur` and `conn_rate`, connection tracking, and connection rejection rules. | Each public frontend has bounded current/rate thresholds plus global `maxconn` and finite timeouts. |
| Runtime certificate update | The management guide describes a transaction that can "duplicate the certificate in memory", then requires `commit ssl cert`. | Capability exists but no admin socket is enabled in H1; H2 must decide reload versus a private socket. |
| Reload | The management guide documents master-worker mode and SIGUSR2 reload; listener handoff is supported by the master. The pinned image entrypoint adds `-W -db`. | The image runs master-worker mode without the deprecated config keyword, but zero-drop behavior remains an H2 measurement. |
| Name resolution at run time | A server name is resolved once, at start-up, unless the configuration has a `resolvers` section and the server line names it (`resolvers <id>`); `parse-resolv-conf` takes the nameservers from `/etc/resolv.conf`; `hold valid/nx/other/refused/timeout/obsolete`, `timeout resolve`, `timeout retry` and `resolve_retries` bound the caching and the retries; `resolve-prefer ipv4` picks the address family. Evidence: the edge log line `smtp_starttls/smtp: IP changed from '172.24.0.3' to '172.24.0.8' by 'platform/127.0.0.11'` and the case `backend_that_changes_address_is_followed`. | Every backend `server` line uses `resolvers platform resolve-prefer ipv4`; the `platform` section reads the container's own DNS (Docker, Kubernetes, Swarm), so no nameserver address is written in the file. A later `server-template` for several replicas belongs in this section. |

The short quoted fragments above total fewer than 25 words from each source; the table
otherwise paraphrases the manuals.

## Deliberately unverified until H2

- Whether HAProxy 3.4.7 sends PROXY v2 at the exact point the Rust TLS backend expects
  when `ssl` and `send-proxy-v2` are combined. This is a go/no-go behavior test.
- Byte-for-byte pass-through and end-to-end STARTTLS behavior on 25/587/143/110.
- Resistance to a client-supplied spoofed PROXY preamble.
- Actual SNI certificate selection, absent/unknown-SNI failure, TLS versions and cipher
  negotiation against clients.
- Enforcement and fairness of per-IP current/rate limits, slow-client deadlines, total
  connection bounds, and protocol-aware removal of unhealthy backends.
- Verified rejection of wrong-name, wrong-CA, expired, and non-mTLS backend endpoints.
- Zero-drop reload/runtime certificate replacement and removal of the old certificate.
- HTTP host routing, request bounds, mTLS re-encryption and absence of backend detail leaks.

No item in this section is claimed as production-ready by H1.

## Additional manual findings (3.4.7-57 manual) that become H2 tests

- Without `strict-sni` the manual says the "first loaded certificate will be presented" for
  absent or unknown SNI; H1 uses `strict-sni` and H2 must prove the handshake is refused.
- `no-tls-tickets` is documented as applying to the ticket mechanism "only used up to TLS 1.2";
  TLS 1.3 resumption behaviour is not documented there, so `session_resumption_policy_as_configured`
  must measure it.
- The manual does not state in what order `send-proxy-v2` and the TLS handshake happen to a
  `ssl` server; only the behaviour test can show it (go/no-go).
- `accept-proxy`/`expect-proxy` is not enabled on any bind; whether a client-sent PROXY header is
  treated as payload (not as metadata) is untested.
- The manual default for server `verify` is `required`; H1 still sets it explicitly.
- Configuration items accepted by `haproxy -c` but whose behaviour is untested: all of the
  above and every stick-table/timeout/tcp-check value in `haproxy.cfg`.

## H1 smoke observations (single run, not formal tests; H2 repeats them as tests)

- `haproxy -c -f` with the pinned image: exit 0 (accepted, behaviour untested for items above).
- `docker compose up`: all five fake backends and the edge become healthy; only the edge has
  published ports. No server was reported DOWN by the edge afterwards.
- A bare `tcp-check connect` (or `http-check connect ssl sni ...`) in a backend does NOT inherit
  the server's `send-proxy-v2` / `ssl` / `verify` / `crt` options and made every backend DOWN
  (fake backends require PROXY v2 and mTLS). `tcp-check connect default` ("Use default options of
  the server line") fixed it; the manual says this for `default`, and the run confirmed it.
- 25/110/143/587 returned the fake backend banners through the edge; 993 with SNI `mail.test`,
  `imap.test`, `other.test`, `web.test` presented the matching certificate (chain verified
  against the test CA); an unknown SNI and no SNI got TLS alert 112 (unrecognized name).

## H2 outcomes (edge behaviour suite, 3 consecutive full runs, all identical)

Result: 26 PASS, 0 FAIL, 2 GAP, 2 INFO. Per-behaviour rows with measured numbers are in
`EDGE-DECISION.md`. The items listed above under "Deliberately unverified until H2" are now
settled as follows: PROXY v2 plus TLS to the backend work together (PROXY first, then the
verified mTLS handshake); pass-through is byte-exact and STARTTLS ends at the backend
certificate; client PROXY preambles cannot spoof the source; unknown and absent SNI are refused
(alert 112); wrong-CA, wrong-name, expired and client-cert-rejecting backends never pass traffic;
reload replaces certificates without dropping established connections.
New facts learned: `timeout tunnel` (not `timeout client`) governs idle pass-through TCP
connections (measured 300 s); inactivity timers do not bound a dripping handshake; HAProxy
accepted a backend certificate whose CN matched although its SAN list did not (CN fallback);
`no-tls-tickets` disables stateless tickets but session-ID/stateful resumption stays available
(bounded by `tune.ssl.cachesize`/`lifetime`); `req.body_size`-style limits do not cover chunked
bodies, only `hdr_val(content-length)` is capped.
Not tested: runtime API certificate update (no admin socket by design), key delivery on tmpfs.
