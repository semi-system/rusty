# First mail in about 15 minutes

One host, Docker with Compose, nothing else: PostgreSQL, the five mail services (smtp-in, submission, imap, outbound, gc: one image each), the admin web and the edge (HAProxy) come from `deploy/standalone`.
What was run end to end for this page: a message sent to port 25 through the edge was delivered and read back over IMAPS on 993, STARTTLS on 25 used the
service's own certificate, the admin page answered over HTTPS, and the services saw the client's real address (PROXY protocol v2).

## 0. What you need

* A Linux host with Docker (Compose v2) and ports 25, 80, 143, 443, 465, 587, 993 free.
* Two names pointing at the host: the **mail host** (`mail.example.org`) and the **admin host** (`admin.example.org`).
* A network you will use the admin web from (an address or a CIDR block): the admin web answers only from there.
* A test host that the internet cannot reach? Use `--self-signed` (below). A real server gets real certificates over ACME.

## 1. Set it up

```sh
cd deploy/standalone
./setup.sh --mail-host mail.example.org --admin-host admin.example.org --contact you@example.org \
           --public-ip 203.0.113.5 --admin-allow 198.51.100.0/24 --staging
```

`--staging` uses the Let's Encrypt test authority (no rate limits; mail clients will not trust its certificates). When it works run `setup.sh` again without it.
For a LAN test host: `./setup.sh --self-signed --mail-host mail.lan --admin-host admin.lan --public-ip 192.168.1.10 --admin-allow 192.168.1.0/24`.

`setup.sh` makes, under `~/rusty-state` (`--state-dir` or `RUSTY_STATE` to change it; it lives outside the repository on purpose: an image build reads the repository, and it cannot read what belongs to the service users): the database passwords (one for each of the four database roles), a private CA and certificate for the database link (clients verify it),
the sealing key pair (the private half stays with the services that open message bodies), and the configuration file of every service (and one for `rustyctl`) from `templates/`. It pulls the images from the registry (`docker compose pull`; `--image-tag 1.2.3` picks a release, the default is `main`; `--build` builds them from the checkout instead, BuildKit needed;
`SKIP_BUILD=1` when they already exist). Nothing is sent anywhere. Run it again to change a setting: existing secrets and keys are kept.

## 2. Start it

```sh
docker compose up -d
docker compose ps          # everything running; db healthy
```

Start order is handled: the database, its roles, the schema migration, the blob directory, then the services and the edge.

## 3. The first administrator

```sh
docker compose logs admin | tail -3       # one line: a 64-character setup token
```

Open `https://admin.example.org/` from the allowed network, enter the token and the address and password of the first platform administrator. The page is gone for good
once an administrator exists. Lost access? `docker compose run --rm --name rusty-ctl-1 ctl admin grant <address>`.

## 4. A domain, an account, DNS

In the web (or with `rustyctl`): add the domain, create an account, make a DKIM key (it is sealed: the web never sees it again), then publish the records the DNS page
shows (MX, SPF, DKIM, DMARC, the domain-ownership record). The page checks them against live DNS, and with `[dns_check] enabled = true` (set in the generated
`gc.toml`) the gc service queues a check of every domain once a day and shows drift.

```sh
docker compose run --rm --name rusty-ctl-1 ctl domain add example.org
printf '%s' 'a-long-password' | docker compose run --rm -T --name rusty-ctl-1 ctl account add me@example.org --password-stdin
docker compose run --rm --name rusty-ctl-1 ctl domain dns example.org      # the records to publish
```

## 5. Use it

* Mail client: IMAP `mail.example.org` 993 (TLS) or 143 (STARTTLS); submission 465 (TLS) or 587 (STARTTLS); user name = the full address.
* Check by hand: `openssl s_client -quiet -connect mail.example.org:993 -servername mail.example.org`, then `a1 LOGIN me@example.org '...'`.
  Always connect by **name**: the edge answers only for the names it has a certificate for (`strict-sni`), so a connection by bare IP is refused.

## 6. Day two

* Backups: `scripts/backup.sh` / `restore.sh` (database first, then blobs), `docs/backup.md`.
* Upgrade: back up first, then `./setup.sh ... --image-tag <new version>` again (it pulls) and `docker compose up -d` (the migration runs first). Rollback after a migration is a restore: see `operations.md`.
* Certificates (ACME mode): the edge renews them itself; `cert-sync` copies the certificate for the STARTTLS ports and `cert-keep-*` keeps the files so a restarted edge does not
  order new ones (`docs/acme.md`). Self-signed mode: valid 825 days; run `setup.sh` again after deleting `state/edge/certs/*` and `state/tls/*` to make new ones.
* Sessions of the admin web live in the admin process (a restart signs everyone out); add `[valkey]` to `admin.toml` to keep them in Valkey (`docs/admin-deploy.md`).

## What is not here yet

Spam and virus filtering (design: `docs/design-filters-and-resolver.md`), the webmail, two-factor sign-in, a tenant's own certificates, and several instances of a service.

## Mail client auto-setup (Thunderbird, Outlook, Apple Mail, and the clients that read the SRV records)

The admin service answers the requests of the clients that set an account up from an address alone. The answer is the same for every domain and says nothing about which domains or accounts exist: it names the
configured mail host with IMAP over TLS on 993 and submission over TLS on 465 (Thunderbird: STARTTLS on 587), and the address as the user name. Nothing needs a session.

| Client | How it finds the settings | What serves it |
|---|---|---|
| Thunderbird, K-9, FairEmail and others that read Mozilla's autoconfig | `http://autoconfig.<domain>/mail/config-v1.1.xml?emailaddress=...` (port 80) | the edge sends that one request (host `autoconfig.*`, exactly that path) to the admin service; every other request on port 80 is redirected to HTTPS |
| Outlook (Windows, Mac) | Autodiscover: `POST https://autodiscover.<domain>/autodiscover/autodiscover.xml`, then the SRV record `_autodiscover._tcp.<domain>` (the mail host, port 443) | the edge ends TLS for the **mail host** on 443 and sends exactly `/autodiscover/autodiscover.xml` (either spelling of the path), `/mail/config.mobileconfig` and `/mail/config-v1.1.xml` to the admin service; everything else on the mail host is refused (`421`) |
| Apple Mail (iPhone, iPad, Mac) | a configuration profile: `https://<mail host>/mail/config.mobileconfig?emailaddress=user@example.org` (give the person this link, or show it as a QR code); installing it adds the account and asks for the password | the same route on the mail host; the profile holds no password and its identifiers depend on the address, so installing it again updates the account |
| The newer Outlook (Autodiscover v2) | `GET https://autodiscover.<domain>/autodiscover/autodiscover.json?Email=...&Protocol=AutodiscoverV1`, then the XML form above | the same route on the mail host answers `{"Protocol":"AutodiscoverV1","Url":"https://<mail host>/autodiscover/autodiscover.xml"}`; any other protocol (ActiveSync, EWS) is `400 InvalidProtocol`, which Outlook reads as "not here" |
| Clients given the mail host itself (K-9, FairEmail, Spark, eM Client, Samsung Email read the same Mozilla/Microsoft answers) | `https://<mail host>/.well-known/autoconfig/mail/config-v1.1.xml` | the same Mozilla answer, at the well-known path as well |
| Clients that read RFC 6186 SRV records | `_imaps._tcp`, `_submission._tcp`, `_submissions._tcp` | DNS only |

What you do per domain: publish the records the DNS page lists as optional: `autoconfig.example.org` (an `A`/`AAAA` record at the addresses of the edge, or a `CNAME` to the mail host: the check compares addresses, so
either passes), `_autodiscover._tcp.example.org` (`SRV 0 1 443 <mail host>`) and the three RFC 6186 SRV records; keep port 80 open to the edge (autoconfig, and the certificate challenges).

Outlook first asks `https://autodiscover.example.org/...`. This installation has a certificate for the mail host and the admin host only, not for every customer domain's `autodiscover.` name, so that first request
cannot succeed (the TLS handshake is refused, the edge serves no certificate for a name it was not given) and Outlook goes on to the SRV record, which sends it to the mail host: Outlook then asks the person to confirm
the redirection once. A domain whose owner serves `autodiscover.<domain>` over HTTPS with a certificate of its own (a reverse proxy in front of it) can send that request to `https://<mail host>/autodiscover/autodiscover.xml`
and avoid the question. Per-domain certificates at the edge (the decided ACME HTTP-01 per domain) would remove it.

## POP3

For old devices and scripts that speak POP3 and not IMAP (`docs/design-pop3.md`). It is a listener of the IMAP service, on by default (switch it off under Protocols in the admin web, `docs/design-protocols.md`). The edge passes port `110` (clear text with `STLS`) and ends TLS on `995`; open both in the firewall. A password is taken only inside TLS (`STLS` on 110, or port 995); `APOP` is not offered. Messages are downloaded from the **Inbox** only, are deleted only when the client says `DELE` and then `QUIT`, and a connection that drops deletes nothing. A client that leaves its messages on the server (the default of most) just downloads what it has not seen, by `UIDL`.

## Filters from a mail client (ManageSieve)

A client that edits Sieve filters (Thunderbird with the Sieve add-on, Roundcube-style tools, `sieve-connect`) talks ManageSieve to the mail host on port 4190 with STARTTLS (`docs/design-sieve.md`). Sign in with the address and the password, or with an app password. The scripts are the ones of the filter editor in the webmail: one active script runs on every incoming message of the account, at most ten scripts of 64 KiB each. The mail rules of the account are not applied while a script is active.

## Calendars and contacts (CalDAV and CardDAV)

When the operator has turned the service on (`DAV=1`, `docs/swarm-runbook.md`), the calendar and the address book of an account are at `https://mail.example.org/dav/` (the mail host; `/.well-known/caldav` and `/.well-known/carddav` lead there). Sign in with the address and the password, or with an app password (needed when the second factor is on). Every account starts with a calendar and an address book named `default`. Thunderbird, iOS, macOS and DAVx5 find them from the server name; `docs/design-caldav-carddav.md` lists what is and is not supported (no invitations by mail and no sharing yet).
