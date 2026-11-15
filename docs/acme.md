# Automatic TLS certificates (ACME HTTP-01) at the edge

Decision (owner, 2026-11-14): **HTTP-01 only**, in **HAProxy's own ACME client**, not in a separate `certd` service and **not in the admin web**. Port 80 may be open to the internet. A tenant uploading its
own certificate was dropped for now (it would put a private key on the web); if it returns, a CSR flow (the key stays at the edge) is the preferred shape.

`deploy/edge/haproxy-acme.cfg` is the template:

* `acme letsencrypt`: the directory URL, the contact address, `challenge HTTP-01`, an ECDSA key type and the account key file. HAProxy makes the account key (a file under `/var/lib/haproxy/acme`) itself.
* `crt-store certs`: `load crt "mail.pem" acme letsencrypt domains "mail.example.org,imap.example.org,..."` and one for the admin host. HAProxy makes a temporary key pair at start so that the
  listeners come up, gets the real certificate, renews before expiry and swaps it in **without a reload**.
* Port 80 answers `/.well-known/acme-challenge/<token>` from the ACME map and redirects everything else to HTTPS (`308`).
* The IMAPS and SMTPS listeners use `crt "@certs/mail.pem"`; the admin host (`admin_https`) uses `@certs/admin.pem` and keeps the IP allow-list of `docs/admin-deploy.md`.

## Rules

* **The acme section is experimental in HAProxy 3.4** (the pinned 3.4.7 refuses it without `expose-experimental-directives` in `global`): the template sets it and says why. Re-check the keyword
  list when the image is bumped; `scripts/check-edge-acme.sh` fails if the configuration stops parsing.
* The keys are **files of the edge** (`/var/lib/haproxy`, directory mode 0700, owned by the haproxy user, a volume that survives restarts so that renewals keep their account). HAProxy writes the account key
  with mode 0644, so the **directory** mode is what protects it. Nothing else reads them; the admin web never sees a key.
* **Use the staging directory first** (`https://acme-staging-v02.api.letsencrypt.org/directory`): certificates are not trusted, there are no rate limits. Switch to the production directory when the
  names, DNS and port 80 are right.
* Names only: no wildcard (that needs DNS-01, which is not planned). Every name in `domains` must resolve to the edge and be reachable on port 80 from outside.
* DNS records of the mail domains (MX, SPF, DKIM, DMARC) are the admin web's DNS page; the certificate names (`mail.`, `imap.`, `smtp.`, `admin.`) are A/AAAA records you publish yourself.

## What is checked, and what is not

`scripts/tests/acme-pebble.sh` (the nightly edge job) runs a **real issuance and a renewal**: HAProxy 3.4.7 registers with Pebble (the Let's Encrypt test CA, images pinned by digest), answers the HTTP-01 challenge, installs the
certificate for the requested name (issued by the CA, not its temporary self-signed one) and, on `acme renew`, swaps in a new one (another serial) without a reload. (Found while writing it: with the challenge test server's
default AAAA answer the validation connected to `::1` and failed; the names in a real installation must resolve to the edge on IPv4 and IPv6 alike, or only IPv4.)

`scripts/check-edge-acme.sh` (run by CI in the image smoke job) parses the template with the pinned HAProxy image, checks that HAProxy makes the account key and the temporary key pairs, and that nothing
needs the network. **Not checked here: Let's Encrypt itself** (rate limits, the production directory, port 80 reachable from the internet): the first issuance of an installation should be watched against the staging directory.


## Behind the edge: PROXY protocol v2

The edge sends each connection to a service with a PROXY protocol v2 header (`send-proxy-v2` in HAProxy) so that the service knows the **client's** address (the per-address connection cap and the login
throttle must see the client, not the proxy). Set, per service (`[smtp]`, `[imap]`):

```toml
proxy_protocol = true
trusted_proxies = ["10.0.0.0/24"]     # the addresses the proxy connects from
```

* The header is read **only from `trusted_proxies`**; a connection from any other address is dropped without reading a byte, so a client cannot claim an address. The list is required (1 to 16 blocks) and may not
  hold every address (`0.0.0.0/0`, `::/0` are refused). Without `proxy_protocol` a list is refused too.
* The parser is strict (signature, version, command, bounded length, checked TLVs; the text protocol v1 and UNIX or datagram families are refused) and fuzzed (`fuzz_proxy_v2`). `LOCAL` (HAProxy's health checks) keeps the proxy's
  own address; a `PP2_TYPE_SSL` TLV with the client bit means the client's connection used TLS at the edge.
* **TLS split**: the implicit-TLS ports (465, 993) end TLS at the edge, so with `proxy_protocol` they need **no certificate of their own** and their streams count as protected. The STARTTLS ports (587, 143) are
  passed through by the edge and upgraded by the service itself, so they **still need `[tls]`**: the edge cannot hold the key for a protocol upgrade it does not speak.
