# Design: spam and virus filtering (rspamd, ClamAV) and a resolver of our own

Status: **proposal for the owner's approval**. Nothing here is built. Rule of this project: no service is added without its image, its trust boundary and its behaviour when it is down written first.

## 1. What is added

| Service | Image (pinned by digest in `deploy/images.lock`) | Job |
|---|---|---|
| `resolver` | Unbound (validating, caching, DNSSEC) | Recursive resolver for everything that looks names up: outbound MX/A lookups, `dns.check`, rspamd's blocklist queries |
| `rspamd` | `rspamd/rspamd` | Scores a message: spam score, symbols, action (accept, add header, soft reject, reject) |
| `clamd` | `clamav/clamav` (clamd only) | Virus scan of a message stream |
| `freshclam` | same image, update command only | Downloads signature updates into a volume `clamd` reads |

Rusty talks to rspamd over its HTTP scan API (`POST /checkv2`) and to clamd over its TCP `INSTREAM` command, from the SMTP inbound path after the message is received and before `250` is answered, so a rejection is a real SMTP rejection, never a bounce.

## 2. Trust boundaries

* **A message is hostile input and both scanners parse it.** They are the likeliest components to be exploited. So each runs as a non-root user, read-only root file system, no capabilities, memory and pid limits, in a network `filter` that has **no route to the internet and no access to the database**. A compromised scanner sees the messages sent to it and nothing else (no credentials, no keys, no blobs).
* `rspamd` needs DNS (blocklists): it uses `resolver`, which is on the `filter` network. Only `resolver` and `freshclam` have outbound access: `resolver` to the root servers on 53, `freshclam` to the ClamAV mirrors on 443. `clamd` itself never reaches out.
* Rusty services reach the scanners on the `filter` network only; the edge cannot reach it.
* Messages travel to the scanners unencrypted inside the host's private network (same trust level as the database link inside the compose network, which is TLS-verified; for a multi-host setup the scanners get TLS too: rspamd supports an HTTPS worker, clamd needs a TLS wrapper such as stunnel, to be decided then).
* Scan results are stored as headers (`X-Rusty-Spam-Score`, `X-Rusty-Scan`) and in the log as a score and an action only, never message content.

## 3. Behaviour when a scanner is down or slow

One setting per scanner, `on_error`:

* **rspamd: `accept` (default).** An unreachable or slow rspamd (timeout 10 s) must not stop mail; the message is accepted with `X-Rusty-Scan: spam=unavailable` and counted in a metric. `tempfail` is available (451, the sender retries).
* **clamd: `tempfail` (default).** Delivering a message that could not be scanned for viruses is the worse outcome; the sender is told to retry (451) with a timeout of 30 s and the message is not stored. `accept` is available for operators who prefer availability.
* The same message is never scanned twice in one attempt; an oversize message (above clamd's `StreamMaxLength`, 25 MiB default; rspamd's `max_message` limit) is not scanned for that scanner and gets `X-Rusty-Scan: skipped-size`.
* When `resolver` is down: outbound deliveries are deferred with the normal retry schedule (a DNS failure is a temporary failure, never a bounce), `dns.check` records `unavailable` (not `missing`), rspamd loses its blocklist checks and keeps scoring.
* A scanner restart does not lose mail: nothing is queued inside the scanners.

## 4. Where it plugs into Rusty

* `[filter]` section in `rusty.toml` (absent = no scanning): `rspamd_url`, `clamd_addr`, `on_error` per scanner, timeouts, size limits, which listeners are scanned (inbound on 25 by default; submission 587/465 optional, to catch a compromised account sending spam, with the score only logged at first).
* Actions: rspamd `reject` -> `550 5.7.1`; `soft reject` / greylist -> `451 4.7.1`; `add header` and `rewrite subject` -> header added, message delivered (and filed to Junk when the account has a Junk mailbox: IMAP/JMAP folders `\Junk` special-use); `no action` -> accept.
* Per-domain and per-account policy (turn scanning off for a domain, lower thresholds) goes through the admin web later; the first version has one global policy.
* The resolver is used by setting `[outbound] nameserver` and `[dns_check] nameserver` to it (both settings exist today).

## 5. Deployment

All of it is added to `deploy/standalone` as optional compose profiles (`--profile filter`), so a first installation can start without it. Image digests join `deploy/images.lock`; a nightly job refreshes the ClamAV signatures check and runs a real EICAR and a GTUBE message through the whole path (expected: rejected, rejected) and a message with the scanner stopped (expected: the `on_error` behaviour).

## 6. What is not decided

* Greylisting (rspamd's greylist module needs Redis/Valkey; Valkey is already planned for sessions, so it can share an instance with a separate database number).
* Training (Bayes) from users moving mail to Junk: needs the IMAP side and per-user storage; later.
* An **authoritative** DNS server for the hosted domains (answering for tenants' zones) is a different and much bigger service; this document is only about the recursive resolver. Say if the authoritative one is wanted.

## Inbound message checks and `[resolver]` (built)

The inbound SMTP service checks SPF, DKIM and DMARC of the mail it receives when it has a resolver: `[resolver]` with an optional `nameserver = "host:port"` (the validating Unbound of the `filter`
profile in the standalone stack; without `nameserver`, the system resolver). Without the section the checks are off, because the service in the standalone stack sits on private networks with no
route out and has no DNS of its own. `./setup.sh --filter` writes `[resolver] nameserver = "10.78.0.3:53"` into its configuration. The results go into `Authentication-Results` and are inputs of
rspamd. The delivery worker keeps its own `[outbound] nameserver`.
