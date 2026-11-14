# Valkey: the shared key-value store

Added on the owner's decision of 2026-11-14 for three uses: rate limits and brute-force counters, login sessions and a read
cache. It is **optional**: without a `[valkey]` section every consumer keeps its own in-process state, which is exactly how
Rusty worked before, so Valkey is an upgrade for deployments with several replicas and never a requirement.

## What is built (step V1) and what is next

| Step | Content | State |
|---|---|---|
| V1 | the `rusty-valkey` crate (the `Kv` trait, an in-memory and a Valkey implementation, one conformance suite for both), the `[valkey]` configuration, the test server and its CI wiring | this change |
| V2 | async rate limiters over `Kv` (login throttle, submission limit) with the in-process fallback | next |
| V3 | the session store (admin web, webmail) | after V2; has no consumer until the web parts exist |
| V4 | the read cache | after V3; see "The cache" below |

## The interface (`crates/rusty-valkey`)

`Kv` offers a sliding window (`window_try_record`, `window_count`), a counter that lives from its first increment
(`counter_incr`, `counter_get`) and expiring values (`set`, `get`, `touch`, `delete`). The window is atomic and uses the
**server's** clock (a Lua script reading `TIME`), so replicas with skewed clocks agree and parallel callers cannot all
pass a full window. `MemoryKv` implements the same trait in process; `tests/conformance.rs` runs one suite against both.

The client library (`fred`, exact pin, rustls with `ring`, no OpenSSL and no aws-lc) is used only inside
`crates/rusty-valkey/src/adapter` (`scripts/wrapped-libs.txt`; the gate fails if it appears anywhere else).

## What happens when Valkey is not there

Every call returns `Error::Busy("key-value store unavailable")` and nothing else (no host, no port, no driver text). Calls are
bounded (`command_timeout_ms`, 200 ms by default, plus 250 ms outer bound); the client reconnects for ever with a delay from
100 ms to 5 s. What each consumer does with the error is its own policy, fixed here so it is not decided later under pressure:

| Consumer | When Valkey is unavailable | Why |
|---|---|---|
| rate limits, login throttle | **fall back to the in-process limiter** (today's behaviour) | failing closed would lock every user out of mail because a cache server restarted |
| sessions (admin web, webmail) | **fail closed**: the session cannot be checked, so the user logs in again | a session that cannot be validated must not be honoured |
| read cache | **bypass**: read from Postgres | a cache is never the source of truth |

## Security

* One ACL user per service, with the commands it needs (`+@all` in the test container only). The password is a file read through
  `SecretSource` (`password_file`, mode 0600); the schema has no inline password key. `Debug` of the options redacts it.
* TLS with the certificate chain and host name verified (`ca_bundle`, or the platform roots). A plain-text link is accepted for
  loopback, and in the development profile with a warning; production refuses it for any other host.
* `key_prefix` keeps installations apart on one server. Keys are printable ASCII (at most 200 bytes), values at most 64 KiB.
* The consumers must not put addresses or other personal data in a key in clear; the limiter step hashes the identity.
* Persistence should be **off** for limiters and cache (they must never depend on surviving a restart); a session store that
  loses its data only makes users log in again.

## The cache (step V4), decided now so that it is built carefully

A cache of directory data is a security risk when it is stale: a disabled account or a removed alias must stop working at
once. Rules for V4: the password hash and the account **status used to authenticate are never cached** (authentication reads
Postgres every time); only address resolution (domain, alias, list expansion) is cached, with a short TTL, the domain id in the
key, and invalidation by the same `NOTIFY` mechanism as the change feed (a trigger announces a directory change; every process
drops its entries). Before building it, the benchmark must show that these lookups are actually hot.

## Testing

`bash scripts/kv-up.sh` starts a throw-away Valkey (digest-pinned in `deploy/images.lock`, loopback only, read-only root, all
capabilities dropped, the default user off and an ACL user `rusty`, a random password per run) and writes
`valkey://rusty:<password>@127.0.0.1:<port>` to `~/.rusty-kv-test-url.<run>`; `kv-down.sh` removes it. The tests read
`RUSTY_TEST_VALKEY_URL`; without it they are skipped, and `RUSTY_REQUIRE_VALKEY=1` (CI and `scripts/ci.sh` set it) turns a
missing server into a failure.

## Not verified yet

* The **TLS handshake** to a real server: the configuration, the CA bundle handling and the refusal of a bad bundle are tested, the
  encrypted connection itself is not (the test container has no TLS). A TLS-enabled container with a generated certificate is the next
  addition to the test server.
* Valkey **Cluster** and **Sentinel**: only a single server is supported.
* Behaviour with **PgBouncer-like proxies** in front of Valkey: not applicable, none is used.
