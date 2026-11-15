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
the sealing key pair (the private half stays with the services that open message bodies), and the configuration file of every service (and one for `rustyctl`) from `templates/`. It builds the images (`docker compose build`,
BuildKit needed; `SKIP_BUILD=1` when they already exist). Nothing is sent anywhere. Run it again to change a setting: existing secrets and keys are kept.

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
once an administrator exists. Lost access? `docker compose run --rm ctl admin grant <address>`.

## 4. A domain, an account, DNS

In the web (or with `rustyctl`): add the domain, create an account, make a DKIM key (it is sealed: the web never sees it again), then publish the records the DNS page
shows (MX, SPF, DKIM, DMARC, the domain-ownership record). The page checks them against live DNS, and with `[dns_check] enabled = true` (set in the generated
`gc.toml`) the gc service queues a check of every domain once a day and shows drift.

```sh
docker compose run --rm ctl domain add example.org
printf '%s' 'a-long-password' | docker compose run --rm -T ctl account add me@example.org --password-stdin
docker compose run --rm ctl domain dns example.org      # the records to publish
```

## 5. Use it

* Mail client: IMAP `mail.example.org` 993 (TLS) or 143 (STARTTLS); submission 465 (TLS) or 587 (STARTTLS); user name = the full address.
* Check by hand: `openssl s_client -quiet -connect mail.example.org:993 -servername mail.example.org`, then `a1 LOGIN me@example.org '...'`.
  Always connect by **name**: the edge answers only for the names it has a certificate for (`strict-sni`), so a connection by bare IP is refused.

## 6. Day two

* Backups: `scripts/backup.sh` / `restore.sh` (database first, then blobs), `docs/backup.md`.
* Upgrade: pull, `./setup.sh ...` again, `docker compose build && docker compose up -d` (the migration runs first).
* Certificates (ACME mode): the edge renews them itself; `cert-sync` copies the certificate for the STARTTLS ports and `cert-keep-*` keeps the files so a restarted edge does not
  order new ones (`docs/acme.md`). Self-signed mode: valid 825 days; run `setup.sh` again after deleting `state/edge/certs/*` and `state/tls/*` to make new ones.
* Sessions of the admin web live in the admin process (a restart signs everyone out); add `[valkey]` to `admin.toml` to keep them in Valkey (`docs/admin-deploy.md`).

## What is not here yet

Spam and virus filtering (design: `docs/design-filters-and-resolver.md`), the webmail, two-factor sign-in, a tenant's own certificates, and several instances of a service.
