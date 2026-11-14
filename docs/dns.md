# DNS records of a domain: `domain dns` and `domain check`

Rusty works out the DNS records a domain needs from its configuration and the DKIM keys it holds, shows them ready to copy, and checks
them against live DNS.

```
rustyctl --config /etc/rusty/rusty.toml domain dns example.org      # the records to publish; nothing is queried
rustyctl --config /etc/rusty/rusty.toml domain check example.org    # the same records against live DNS; exit 1 if a required one is wrong
```

Both take `--json`. The web setup page uses the same functions.

## What the records are made from

| Input | Where it comes from |
|---|---|
| mail host name | `hostname` (required: the records point at it) |
| addresses mail leaves from | `[outbound] public_ips = ["203.0.113.5", "2001:db8::5"]` (at most 8; unicast addresses that can be published) |
| a relay provider's SPF include | `[outbound.relay] spf_include = "amazonses.com"` |
| DKIM keys | the database (the public half of every key made with `rustyctl dkim generate` or `import`; see `docs/dkim.md`) |

Without `public_ips` the address records and the SPF record cannot name the addresses mail leaves from, and both commands say so
(`warning: [outbound] public_ips is not set ...`) instead of pretending. With a relay, set `spf_include` to the provider's include
(`amazonses.com`, `sendgrid.net`, ...): the generated SPF record names it, and the check verifies that the published record does.

## What `domain check` decides

One line per record: `ok`, `missing`, `mismatch` or `warning`, the finding codes, and what DNS showed when it is not `ok`.

* **SPF** is judged by what it *authorises* (RFC 7208 evaluation of `ip4`, `ip6`, `a`, `mx`, `include`, `redirect`), not by its spelling:
  our addresses must be authorised, the lookups counted statically must be at most 10 (and void lookups at most 2), exactly one record, no
  `+all`. With a relay, the provider's include must be named in the published record, directly or through another include.
  With neither an address nor an include configured there is nothing to verify: `spf.not_verifiable`, a warning.
* **DKIM**: the published key must equal the key Rusty holds (`dkim.key_differs`, `dkim.revoked`), testing mode and a short RSA key are warnings.
* **DMARC**: one well-formed record (a stricter policy than the generated `p=none` is fine).
* **MX**, **A/AAAA**, **PTR** (present, equal to the mail host, forward-confirmed; not checked at all through a relay) and **SRV** for client setup.
* A check that cannot be decided (a DNS failure, an SPF macro) is a `warning`, never a verdict.

The run **fails** (exit 1) when a *required* record is missing or wrong; a recommended or optional one that is wrong, and anything undecided,
is reported without failing it. The result is recorded: the next check reports the **change** since the last one (`ok -> mismatch`), and a record
that was `ok` and fails three checks in a row raises an alert (drift). A key that was not generated yet shows as a placeholder and is not recorded.

## Not covered

Whether the ports are reachable from outside and the TLS certificate they serve (they need connections, not DNS), DNSBL listings, DNSSEC,
and the domain-ownership challenge record. A periodic check (the `dns.check` job) comes next; today the command is run by hand or by cron.
