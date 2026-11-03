# Edge decision: HAProxy (option A) or fall back to `tlsd` (option B)

Status: evidence for the user's decision, 2026-11-03. The decision itself belongs to the user.

Evidence: `tools/edge-tests` (`cargo test -p edge-tests -- --ignored --nocapture`, ~19 minutes)
against HAProxy 3.4.7 (`haproxy@sha256:2caaf8b1...`, OpenSSL 3.5.9) with the fake backends in
`deploy/compose`. The full suite was run three times on the same machine (WSL2, Docker 29.9);
the table shows run results (see "Flake check"). Raw per-case output is printed by the suite;
`deploy/VERIFIED.md` records the outcome per behaviour.

## Recommendation: KEEP HAProxy (option A), with the conditions below

No go/no-go criterion failed. All five fall-back criteria (next section) passed without
needing any weakening of a rule. Conditions and known limits are listed under "Findings"
and "Not proven"; none of them is a reason to fall back today, but several must be closed
before production.

## Criteria for FALL BACK to `tlsd` (option B)

Fall back if any of these FAILS and cannot be fixed by configuration:

| Criterion | Result |
|---|---|
| `proxy_v2_and_tls_to_backend_work_together` | PASS: backend parses PROXY v2 first, then the verified-mTLS handshake, on 465/993/995 |
| `backend_link_is_verified_mtls` | PASS: wrong CA, wrong name, expired cert, backend rejecting the edge client cert: no byte ever reached a client (16 attempts per scenario over 14 s) |
| `client_supplied_proxy_header_cannot_spoof_source_ip` | PASS |
| `per_ip_connection_limit_*` (and rate limit) | PASS: cap held exactly, second IP unaffected |
| `cert_replacement_does_not_drop_connections` | PASS (reload by SIGUSR2) |

## Requirement table

| Requirement (plan Global Constraints / Review Focus) | Test | Result | Measured |
|---|---|---|---|
| Only the edge publishes ports; exactly 25,80,110,143,443,465,587,993,995 | `only_edge_publishes_ports` | PASS | 6 containers |
| Read-only rootfs, uid 1000, cap_drop ALL, no-new-privileges, not privileged/host, limits | `edge_runs_read_only_nonroot_without_caps` | PASS | edge CapEff/Prm/Inh/Bnd/Amb all 0, NoNewPrivs 1 |
| No runtime socket mounted, no unix socket in the edge | `no_runtime_socket_mounted_anywhere` | PASS | |
| No stats/admin reachable | `stats_and_admin_not_reachable_from_outside` | PASS | edge listens only on its nine ports (/proc/net/tcp) |
| Pass-through keeps bytes | `tcp_passthrough_preserves_bytes` | PASS | 1 B, 4093 B, 1 MiB random on 25/587/143/110 |
| STARTTLS end to end with the backend cert | `starttls_upgrade_survives_the_edge` | PASS | peer cert == backend cert; TLS 1.3 |
| Real client IP at the backend (PROXY v2) | `proxy_v2_reports_real_client_ip` | PASS | second container IP seen on all four plain ports; host IP on 465/993/995 |
| PROXY spoofing | `client_supplied_proxy_header_cannot_spoof_source_ip` | PASS | spoofed 6.6.6.6 ignored; preamble before TLS closes the connection; X-Forwarded-For irrelevant |
| SNI cert selection, unknown/absent SNI explicit | `terminated_ports_present_the_right_cert_by_sni` | PASS | unknown and absent SNI: handshake refused, TLS alert 112 (strict-sni), on 465/993/995/443 |
| SNI never reaches a path | `sni_never_reaches_a_file_path` | PASS | traversal, absolute, encoded, very long (5000 B), NUL, empty: all alert 112, edge healthy afterwards, logs clean |
| TLS floor and ciphers on every terminated port | `tls_versions_and_ciphers` | PASS | 1.0/1.1 refused (positive control: the probe client can speak 1.1), CBC/DHE/CCM suites refused; pass-through 25/587: backend refuses <=1.1 |
| Session resumption policy | `session_resumption_policy_as_configured` | PASS with note | no stateless tickets; resumption through a bounded stateful cache (20000 entries, 300 s) IS possible in 1.2 and 1.3 |
| HTTP routing, unknown host, no backend leak | `http_routes_by_host_unknown_host_404_no_backend_leak` | PASS | unknown/absent/IP host: 421 (not 404), no body detail; port 80 no longer reflects an arbitrary Host |
| HTTP re-encrypted with verified mTLS | `http_tls_reencrypted_with_mtls` | PASS | wrong-CA backend: 503 without detail |
| Oversized header/body | `http_oversized_header_and_body_rejected` | PASS with note | 64 KiB header 431, 300 headers 400, Content-Length 100 MB 413; chunked bodies NOT limited (5 MiB accepted) |
| Per-IP concurrency limit | `per_ip_connection_limit_caps_one_ip_not_another` | PASS | caps held exactly: 50/30/50/30 accepted of 70/50/70/50; second IP served; slots recover |
| Per-IP rate limit | `per_ip_connection_rate_limit` | PASS | 60 of 110 accepted in 57 ms on 110; limited IP recovered after ~16 s |
| Many idle connections bounded | `many_idle_connections_bounded` | PASS | 160 held (<= caps); edge 0.4-0.6% CPU, ~40 MiB. Global `maxconn 4096` NOT exercised |
| TLS handshake flood | `tls_handshake_flood_bounded` | PASS | one IP, 4 threads, 11 s: ~75 full handshakes completed (6.6/s) of ~36500 attempts (3200/s); edge CPU ~100% of its 1-core limit while flooded |
| Slow clients closed | `slowloris_closed_within_timeout` | PASS with caveat | measured close: terminated 465/993/995 and half ClientHello 30 s; HTTP/HTTPS partial request 10 s; pass-through 25/587/143/110 idle 300 s (`timeout tunnel`) |
| Dripping handshake | `slowloris_drip_handshake_info` | INFO | 1 byte/10 s on 993 was closed after 60 s (test stopped dripping at 60 s); inactivity-based timeouts do not bound a dripper, only the per-IP cap does |
| Broken backend removed by protocol-aware check | `health_check_removes_a_broken_backend` | PASS | TCP-open but mute backend stays DOWN (re-checked after 12 s), `smtp_starttls/smtp is DOWN` logged, re-admitted after repair |
| Cert replacement without dropping connections | `cert_replacement_does_not_drop_connections` | PASS | held TLS (993) and pass-through (143) connections survived two reloads; 0 of ~12 new connections failed during reloads; 3 haproxy processes (old worker lingers) |
| Old cert not served afterwards | `old_cert_not_served_after_replacement` | PASS | 0 of 16 fresh handshakes on 465/993/995/443 |
| Runtime API cert update | `runtime_api_certificate_update` | GAP | not tested: no admin socket by design |
| No key material in logs/inspect/env/images | `no_key_material_in_logs_inspect_or_env` | PASS | no PEM marker, none of 36 key-body lines; fake image holds only `/fake-backend`; HAProxy image has no key files |
| Keys only on read-only tmpfs | `keys_only_on_readonly_tmpfs` | GAP | keys reach the edge through a read-only bind mount of the git-ignored test directory; tmpfs/secret injection belongs to `certd` and is not built |

## Findings (things the suite showed, in order of importance)

1. **CN fallback.** HAProxy accepted a backend certificate whose SAN list does not contain
   the server name but whose subject CN does (`backend_cert_cn_fallback_probe`). A certificate
   with a wrong CN *and* wrong SAN is refused. Mitigation: the internal CA must issue certificates with
   an authoritative SAN and never a CN that names another service. Tighten (or keep the
   backend-name pinning by separate CA per tier) when `certd` exists.
2. **Pass-through idle time is 300 s.** In TCP mode without analysers HAProxy uses `timeout
   tunnel` instead of `timeout client`, so a client that connects to 25/587/143/110 and goes
   silent holds a slot for 5 minutes. The per-IP caps (30-50) bound the damage; pre-auth
   timeouts must also exist in the services (spec 3b.3b). Lowering the value would cut
   legitimate IMAP IDLE sessions.
3. **Dripping handshakes** are bounded only by the per-IP cap (inactivity timers reset).
4. **Chunked HTTP bodies have no size limit at the edge**; Content-Length is capped at 64 MiB.
   The services must enforce their own limits.
5. **Resumption** works through the stateful cache although tickets are off (documented, bounded).
6. **Flood CPU.** One IP can keep the edge's single CPU busy with refused connections
   (3200 attempts/s) even though only ~7 full handshakes/s succeed. The 1-CPU limit contains it;
   a network-level limiter (nftables/ISP) is needed for volumetric attacks.
7. **Host-header policy is a fixed name (`web.test`)** in the test config; real multi-host routing
   needs a generated map, tested with the same cases.

## Not proven (do not read these as PASS)

- Only fake backends; no real Rusty service, no real mail client, no HTTP/2 request (ALPN `h2` is
  advertised on 443; no h2 test), no IPv6 client, no PROXY v2 TLVs.
- Global `maxconn` and many-source-IP floods; one machine, one source IP (plus one container IP).
- IMAP/POP3 STARTTLS TLS-version probes use the same fake code path as SMTP; only 25/587 were
  probed with `openssl -starttls smtp`.
- Reload under real traffic volume; rootless Docker; resource numbers are from WSL2 and are not
  production capacity figures.
- Key delivery to tmpfs (GAP above), ACME, certificate renewal automation.

## What FALL BACK to `tlsd` would buy and cost

Option B keeps private keys in a small memory-safe Rust process and lets HAProxy (or nothing) see
only ciphertext, at the price of writing and fuzzing a TLS-terminating proxy with PROXY v2, SNI
policy, rate limits, reload and health checks ourselves. Nothing measured here argues for paying
that price now. Revisit if finding 1 or 2 becomes unacceptable, or if a HAProxy CVE affects the
TLS path and the update lag is too long (this is the zero-day argument: HAProxy sees plaintext and
holds the keys on 465/993/995/443 by design of option A).

## Flake check

See the end of this file for the three run results.

Three consecutive full runs on the final configuration: 26 PASS, 0 FAIL, 2 GAP
(`runtime_api_certificate_update`, `keys_only_on_readonly_tmpfs`), 2 INFO
(`backend_cert_cn_fallback_probe`, `slowloris_drip_handshake_info`) each time, ~18.5 minutes per
run (slowest cases: pass-through idle 312 s, verified-mTLS backend scenarios ~305 s, per-IP limit
72 s). Measured values were stable between runs (e.g. 75/75/75 full handshakes in 11.4 s;
0 of 12 new connections failed during reloads). An earlier run of the first suite version had
five FAILs that were test or configuration defects, fixed before these runs and not hidden:
a stale relative path in the TLS 1.1 positive control, Docker's DNS listener counted as an
extra edge port, a PEM-marker grep matching `libcrypto.so`, a "wrong name" backend certificate
that still carried the right CN (this exposed finding 1), and a TLS 1.2 resumption assertion
that did not match the intended stateful-cache policy (the policy was made explicit with
`tune.ssl.cachesize`/`tune.ssl.lifetime`).
