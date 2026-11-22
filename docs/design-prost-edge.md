# Prost as the edge (next to HAProxy)

Status: **built as an option, HAProxy stays the default** (2026-11-22). Prost (`github.com/semi-system/prost`, image `ghcr.io/semi-system/prost:latest`, pinned by digest in `deploy/images.lock`) describes itself as "MVP/pilot with supervision, not confirmed for important production". So the swarm form offers it beside HAProxy, in a pilot mode that cannot take mail away from HAProxy, and the change of the default waits for a soak.

## What Prost brings and what it does not

| | HAProxy (now) | Prost |
|---|---|---|
| 25, 587, 143, 110 passed through with PROXY v2 | yes | yes (`send_proxy_v2`, health checks with the `LOCAL` command) |
| 465, 993, 995 TLS ended at the edge, strict SNI | yes | yes (`reject_unknown_sni`, TLS 1.2 and up, no resumption) |
| 443: admin host from the allow-listed networks, the mail host only for the clients' auto-setup, 421 for other hosts | yes | yes (`host_acl`, a route-level `ip_acl`, routes by host and path) |
| 80: HTTP-01 challenge, Thunderbird auto-setup, 308 to HTTPS | yes | yes (routes and the `rewrite` redirect) |
| Backends by service name, every task a server | `tasks.<service>` through Docker's resolver | `tasks.<service>` with the DNS answer's own time to live |
| ACME | HAProxy's native ACME (HTTP-01) | `prost-certd` (HTTP-01, DNS-01, wildcard), a separate process; its certificates are in versioned directories, renewal without restart and without cutting sessions |
| Certificates from S3 | no | `s3://bucket/key` with SigV4, `?versionId=`, loaded at start or apply, no polling |
| Metrics | none | Prometheus on **loopback only** (so not scraped across the overlay yet) |

Differences that matter: a certificate for **a new domain** needs a group in `prost-certd`'s configuration (HAProxy's `crt-store` has the same need); Prost reads certificates from S3 only when it starts or applies a configuration; the mail host's non-setup paths on 443 have no route (Prost answers as it answers any unrouted request, not with HAProxy's 421).

## What was built

`EDGE` in `swarm.env` (`deploy/swarm/edge-prost.sh`, templates `deploy/standalone/templates/prost-edge.toml.in` and `prost-certd.toml.in`):

* `EDGE=haproxy` (default): as before.
* `EDGE=prost-pilot`: HAProxy keeps the real ports. A second service, `edge-prost`, runs beside it on the ports `PROST_PILOT_OFFSET` above them (20025, 20587, 20143, 20110, 20465, 20993, 20995, 20443, 20080). The mail services and the admin web accept the PROXY header from it by name as well. It gets no certificate of its own and answers no ACME challenge: with `TLS_MODE=acme` it needs `CERT_S3=1` and reads HAProxy's certificates from the bucket, with `TLS_MODE=self` it uses the same self-signed ones.
* `EDGE=prost`: Prost is the edge on the real ports (the service keeps the name `edge`, so nothing else changes), and with `TLS_MODE=acme` `prost-certd` runs beside it on the Manager: one group `edge` with both host names, certificates in a volume, handed to the edge through its admin socket. Both run read-only, without capabilities, as user 10001. The edge's own state is a tmpfs rebuilt from its seed at every start, so the configuration of the stack is what runs.
* **Certificates in S3** (`CERT_S3=1`): `certs sync` also puts them in the bucket `CERT_S3_BUCKET` (default `certs`, of the stack's own S3 or of the one in `CERT_S3_*`; **never the bucket of the mail**; refused when equal): `s3://certs/mail/` and `s3://certs/admin/` from HAProxy, `s3://certs/edge/` from `prost-certd`. The upload is curl's SigV4 in a throw-away container on the Manager (`curlimages/curl`, pinned); the key and the S3 credentials are in a private directory there only for the call. The Prost pilot reads the objects with its `s3://` URIs and the AWS shared credentials file (a Docker secret); a new certificate in the bucket restarts the pilot onto it.
* `certs sync` with `EDGE=prost`: the services' own STARTTLS certificate (`tls_cert`, `tls_key`) is prost-certd's, read with `docker cp` (the image has no shell), as HAProxy's was read from its runtime socket. `rusty-swarm edge status` shows what Prost and prost-certd say (including whether each certificate is ready).
* **The admin web reads the client address from the PROXY header** (`[admin] proxy_protocol`, new): switched by `ADMIN_PROXY=1` (default 0 until the admin image that knows the key is the one deployed; an older admin refuses the unknown key, which is also why this pull request's swarm run, which uses main's images, needs it off).
  Details: it was seeing the edge's address, so the audit log's "from address" and the sign-in throttle were per edge. HAProxy's `be_admin` sends it too now, and the container's health probe sends a `LOCAL` header.

## Checked

`scripts/tests/swarm-stack.sh` renders the stack for `prost`, `prostself` and `pilot` and checks it (ports, services, volumes, secrets, paths, trusted names, no key in the stack file, refusals of bad values); when the Prost image is on the machine it also runs the real `prost check` on the rendered configuration. The rendered edge was also run with the real image (read-only, no capabilities, tmpfs state) against fake backends: the greeting arrives through port 25 and through 465 after the TLS handshake, an unknown SNI is refused, the admin host from outside the allow-list is `403`, a foreign host `421`, port 80 redirects with `308`.

## How to try it (the pilot, on the test VM)

1. In `swarm.env`: `EDGE=prost-pilot`, `CERT_S3=1` (and, with the stack's own S3, nothing more). With `TLS_MODE=acme` HAProxy must already have its certificates.
2. `./rusty-swarm prepare secrets`, `prepare configs`, `deploy`. HAProxy is not touched; `edge-prost` is added.
3. `./rusty-swarm certs sync`: the certificates go to `s3://certs/`, and the pilot restarts onto them.
4. `./rusty-swarm verify` checks the greetings on the real ports and on the pilot's. By hand: `openssl s_client -connect HOST:20993 -servername MAIL_HOST`, a mail client on 20993 and 20465, `curl --resolve ADMIN_HOST:20443:IP https://ADMIN_HOST:20443/`.
5. `./rusty-swarm edge status`, `logs edge-prost`.
6. **To replace HAProxy** (after the pilot has run for days): `EDGE=prost`, `./rusty-swarm prepare secrets`, `prepare configs`, `deploy`: HAProxy's service goes (stop-first, a few seconds without an edge) and Prost takes the ports; `prost-certd` orders new certificates (try `ACME_DIRECTORY` of Let's Encrypt staging first, the real one counts against rate limits). Back: `EDGE=haproxy` and the same three commands.

## Not done

* The soak: days of real mail through the pilot and a renewal that runs by itself.
* A group of `prost-certd` for **each tenant domain** (tenants' own host names): the certificate there is for the platform's two names; per-tenant ACME is the earlier decision (per-domain ACME at the edge), still to be built for either edge.
* The manual upload of a certificate in the admin web (a platform administrator's certificate and key, sealed, written to the bucket): the bucket and the way Prost reads it are in place; the page and the writer in the admin service are not.
* The Certificates page reading `prost-certd`'s `/certificates` (next renewal): it is a private endpoint of that container; the page still probes from outside.
* Prost's metrics (loopback only) in the monitoring stack.
* The standalone (non-swarm) stack stays on HAProxy.
