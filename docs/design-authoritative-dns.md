# Design: an authoritative DNS server for the hosted domains (`rusty-dns`)

Status: **proposal for the owner's review**; the owner asked for it on 2026-11-16 (together with the filters). Step 1 of section 7 is built (2026-11-24): the pure answer engine (`crates/rusty-dns`: zones, `answer`, its tests); nothing else is. Rule of this project: no service is added without its image, its trust boundary and its behaviour when it is down written first. This document is only about the **authoritative** side (answering for tenants' zones); the recursive resolver (Unbound) is in `design-filters-and-resolver.md`.

## 1. What it is for

Today a tenant adds a domain and then has to publish MX, SPF, DKIM, DMARC, SRV, the ownership TXT and `autoconfig` records at whatever DNS host the domain has, by hand, and the DNS page only checks them. With `rusty-dns` the tenant instead points the domain's **NS records at the platform's name servers** (one change at the registrar) and the platform publishes everything itself: the records follow the domain automatically (a new DKIM selector, a changed outbound IP, a new certificate host name) and drift cannot happen.

It is an opt-in mode per domain (`dns_mode`: `external`, the default and today's behaviour, or `hosted`). A domain in `hosted` mode is answered by `rusty-dns`; its DNS page shows the delegation status and the editor of the extra records instead of the "records to publish" table.

## 2. The service

* One static binary and image, `rusty-dns`: UDP and TCP on port 53, **authoritative only, never recursive** (no cache, no forwarding: it cannot be used as an open resolver), at least two instances on two addresses (a delegation needs two name servers; both are stateless and read the same database).
* The DNS wire protocol is parsed from the internet: the codec is a wrapped library behind a Rusty-owned adapter (`hickory-proto` and `hickory-server`'s request handling stay inside `adapter/`, per the wrapped-library rule), with the answers built by our own pure function `answer(zone, question) -> Response` that is unit-tested and fuzzed on its own (no I/O).
* **Zone data is generated, not stored as zone files**: for a hosted domain the zone is `records_for_domain(...)` (the one source of truth the DNS page already uses: MX, SPF, DKIM per selector, DMARC, SRV, autoconfig, the ownership TXT, the mail host's A/AAAA when the platform's own domain is hosted), plus the tenant's **extra records** (table `dns_extra_records`), plus `SOA` and `NS` at the apex. The serial is the time of the newest change to the domain's DNS data (a monotone counter kept in the database), so a secondary or a monitor can see changes.
* A snapshot of all hosted zones is built in memory and refreshed on change (the database announces changes with `NOTIFY`, the instance also re-reads on a slow timer); an answer is a lookup in the snapshot, so a query costs no database round trip.
* **Behaviour when the database is down: keep answering** from the last snapshot (stale serving, bounded by a configured maximum age, default 24 hours, after which it answers `SERVFAIL` for zones whose snapshot is older), because a DNS outage takes mail down far faster than a stale record does. A start with no snapshot and no database does not serve.

## 3. Trust boundary and abuse

* It is the most exposed service of the platform and has the smallest authority: its database role (`rusty_dns`) can **read** the domain, DKIM public data, relay-free facts it needs and its own tables, nothing of mail, no sealed secret, no password hash. No key of any kind is held by it in v1 (no DNSSEC signing in v1, section 6). A compromise yields what is public DNS data anyway.
* **Amplification and floods**: response rate limiting per client prefix (slip to TC so a spoofed source gets a truncated answer and a real client retries over TCP), EDNS payload capped at 1232 bytes, `ANY` answered minimally (RFC 8482), a hard cap on concurrent TCP connections and on message size, TCP idle timeout of a few seconds, no `AXFR`/`IXFR` (refused: the zone data is public through ordinary queries and the generator is not meant to be copied wholesale), `NOTIFY`, `UPDATE` and `CHAOS` queries refused.
* **A query never changes state.** The only writers are the admin web, `rustyctl` and the services that already change DNS data.
* Names are validated like every other name in the project (`parse_address` and the domain rules); a tenant's extra record is checked on entry (types `A`, `AAAA`, `CNAME`, `TXT`, `MX`, `SRV`, `CAA`; name inside the zone only; no `CNAME` at the apex or next to other data; TXT value length and count limits; at most N records per domain), and **a tenant cannot override the generated records** (the mail-critical ones are read-only in the editor).
* The ownership of the delegation is proven by the existing ownership challenge (a TXT the platform generates, checked at the platform's resolver once the NS records point here, so a domain cannot be hosted by someone who does not control it at the registrar).

## 4. Delegation and glue

* The platform needs two name server names under a domain it controls (`ns1.<platform zone>`, `ns2.<platform zone>`) with `A`/`AAAA` glue at the registrar of the platform zone; the platform zone itself is a hosted zone of the same service (the instance answers for its own NS and mail host names).
* Configuration `dns.toml`: `[dns]` listen addresses, the NS names and the contact (`hostmaster`) for `SOA`, the TTLs, the rate limits, the maximum stale age; `[store]`; `[valkey]` optional (shared rate-limit state is not needed: limits are per instance).
* The admin web (platform administrators) sets a domain to `hosted`; the DNS page of a hosted domain shows the NS names to set at the registrar and checks the delegation like it checks other records (`dns.check` asks the public resolvers for the `NS` of the domain and compares).

## 5. Interaction with the rest

* **ACME**: with the zone in our hands, `DNS-01` becomes possible for wildcard certificates (not in v1: the edge's HTTP-01 per domain is the decided way); the service has no ACME role.
* **`dns.check`**: for a hosted domain it keeps its meaning (it asks public DNS what is published) and now proves the platform's own server answers correctly from outside.
* **Outbound IP or host change**: the generated SPF and MX change the moment the configuration does; no tenant action.
* **DKIM rotation**: the new selector's record is published as soon as the key exists, the old one stays until it is retired (the retire time is part of the DKIM key's own lifecycle, not this service's business).

## 6. What is not decided

1. **DNSSEC**: v1 serves unsigned zones (`DS` is not published). v2 signs online with ECDSA P-256 keys per zone sealed in the database and loaded by the instances; that gives these instances a secret to protect and needs a key-rollover design (CSK, DS shown to the tenant for the registrar). Say if v1 must already be signed.
2. **A third party as secondary** (a DNS provider as backup) needs AXFR with TSIG, which section 3 refuses; two instances on two hosts are the proposed redundancy.
3. **The name servers' addresses**: two public IPs are needed (the second can be another host or another address of the same machine: weaker). The installer asks for them when `rusty-dns` is enabled.
4. **IPv6** for the name servers: assumed yes when the host has it.
5. Reverse zones (PTR for the outbound IPs) belong to the owner of the IP block, not to this service.

## 7. Order of work

1. The pure `answer` function and its tests (every record type, NXDOMAIN, NODATA, CNAME rules, wildcard absence, `ANY`, case-insensitive names, compression), the fuzz target.
2. The adapter over the DNS codec, the listeners with the abuse limits (RRL, TCP caps), tests with a real resolver library as the client.
3. Migration: `dns_mode` on the domain, `dns_extra_records`, the zone serial; repository methods and grants; the `rusty_dns` role.
4. The snapshot builder and the `NOTIFY` refresh; stale serving and its tests (database stopped mid-run).
5. Admin API and web: mode switch, delegation status and check, the extra-records editor with its validation.
6. The image, the standalone stack (two instances, published port 53 on UDP and TCP), `scripts/check-service-image.sh` for it, the documentation.
