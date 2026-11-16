# The swarm form (operator's guide)

The same mail server as `docker.md`, run as a Docker Swarm stack and set up by one tool, `deploy/swarm/rusty-swarm` (`design-swarm.md` has the reasons; this page is what you do). Everything the tool
does is repeatable: run a step again and it changes only what is different.

## Get it

Every release has **one file**, `rusty-deploy-TAG.tar.gz`, listed in `SHA256SUMS` (signed with cosign: `docker.md`). It holds the tool, the database roles, the pinned images of that release
(by digest) and the standalone stack. Nothing else is taken from the repository.

```
tar xzf rusty-deploy-TAG.tar.gz && cd rusty-deploy-TAG
./rusty-swarm init
```

On a machine with a terminal and no `swarm.env` yet, `init` asks what it needs (dialogs when `whiptail` is installed, plain questions otherwise) and writes `swarm.env` (mode 0600). Without a
terminal it reads `swarm.env` (copy `swarm.env.example`). Then it does, in order, and stops at the first thing that is wrong:

1. **Docker**: installed (`get.docker.com`, as root) on the nodes that have none; left alone where it is.
2. **A swarm that exists**: offered for removal (`[y/N]`; `--yes` answers y, no terminal keeps it).
3. `cluster check` (what is wrong on the nodes), the swarm and its labels, `cluster verify` (a task on every node, they reach each other).
4. **Secrets** (swarm secrets with versioned names: `pg_imap_v1`; a change is `_v2`) and **configs** (named by the hash of their content): the service passwords, the sealing key, the S3 keys, the
   self-signed certificates, the per-service TOML files, the edge's configuration.
5. With the default PostgreSQL or S3: the stack is started with only those, which are then prepared.
6. **`prepare db`**: the roles of the services in PostgreSQL, each with a random password, and a login check of each.
7. **`migrate`**: the schema, as a job (`replicated-job`) that must finish before the services move.
8. **`deploy`**: the edge and the services, then it waits until every service is at its replicas.

At the end the admin web prints a **setup token** in its log, once (`./rusty-swarm logs admin`): open `https://ADMIN_HOST/` from an allowed network and make the first administrator there. The mail
domains are made in the web, not in `swarm.env`.

## `swarm.env`

A file of shell variables, yours (not in the repository); `swarm.env.example` is the model.

| Variable | Meaning |
|---|---|
| `NODES=("name\|transport\|role\|labels\|public_address" ...)` | transport: `local` (this machine), `ssh:USER@HOST` (a key; root or `sudo` without a password) or `docker:NAME` (the test cluster). One `manager`; labels `outbound=true` (the node whose address mail leaves from), `av=true`, `db=true`, `s3=true` |
| `MAIL_HOST`, `ADMIN_HOST`, `CONTACT`, `ADMIN_ALLOW` | the two host names, the ACME contact, the networks that may open the admin web (never `0.0.0.0/0`) |
| `DB_HOST DB_PORT DB_NAME DB_ADMIN_USER DB_ADMIN_PASSWORD_FILE` | your PostgreSQL. **Empty `DB_HOST`: the stack runs a default PostgreSQL 18** (one instance on the node labelled `db=true`, TLS with its own CA, data in a local volume) |
| `DB_TLS=1\|0` | 1: TLS verified (the system's CAs; `DB_CA` for a private one). 0: plain, with a warning (the services run with `insecure_allow_unverified_tls`); a trusted private network only |
| `S3_ENDPOINT S3_REGION S3_BUCKET S3_ACCESS_KEY S3_SECRET_KEY_FILE` | your S3. **Empty `S3_ENDPOINT`: the stack runs a default RustFS** (one instance on the node labelled `s3=true`; its bucket is made by a job) |
| `TLS_MODE` | `self` (self-signed certificates; the only one built so far: ACME and the certificate sync for STARTTLS are the next step) |
| `IMAGE_TAG`, `REGISTRY`, `REPLICAS_DEFAULT` | the image tag when the kit has no digests (a checkout; default `main`), the registry, replicas per service (default 1) |
| `STATE_DIR` | where the tool keeps what it makes (default `~/.rusty-swarm`, mode 0700): the service passwords, the keys, the certificates, the generated files |

**The high database login stays yours**: it is used for the preparation only, never put in the swarm, and it is how you reach the database for a manual backup or a repair. With the default
PostgreSQL its password is `STATE_DIR/secrets/pg_postgres`.

**Back up `STATE_DIR/keys/unseal.key`** when the tool tells you it made it: it opens the credentials the tenants stored, and without it they cannot be read.

## Commands

| Command | What it does |
|---|---|
| `init [--yes]` | the whole flow above |
| `inventory` | what the tool understood of `swarm.env` |
| `cluster check / install-docker / init / verify` | the cluster steps alone |
| `prepare db / secrets / configs`, `migrate`, `deploy` | the later steps alone, each repeatable |
| `stack` | print the stack file the next `deploy` would use |
| `logs SERVICE [LINES]` | the log of a service (`admin`, `smtp-in`, `edge` ...) |
| `verify` | the greetings of ports 25, 587 and 143 through the edge: they only come when the edge reached a service and the service accepted the edge, found **by name** |

## How the pieces find each other

No address is written anywhere. The services trust the edge, and the admin web trusts the edge, **by DNS name** (`[smtp] trusted_proxy_hosts = ["tasks.edge", "edge"]`, `[admin] allowed_hosts`), resolved
again every few seconds; the admin web reads the metrics of every task by `tasks.<service>:9100` and serves them all (`design-metrics.md`). The edge sends each connection to `tasks.<service>`, so every
replica takes its share and a replica that stops answering is taken out.

## Changing something

* **A new configuration or password**: edit `swarm.env` (or a file it names), then `./rusty-swarm prepare secrets`, `prepare configs`, `deploy`. A changed secret or config is a new object with a new name,
  so the affected services are updated one by one (a failed update goes back by itself: `failure_action: rollback`).
* **More replicas**: `REPLICAS_DEFAULT=2` and `deploy`.
* **A new release**: unpack its kit, run `./rusty-swarm migrate` then `deploy` with the same `swarm.env` and `STATE_DIR`. The migration runs first, on the new image; take a backup before (`operations.md`:
  a migration is forward only, the way back is a restore).

## What is not there yet

ACME certificates and the sync of the certificate to the mail services (so the edge's certificate is the only trusted one, and STARTTLS uses the self-signed one), the spam and virus filters and Valkey in the
stack, resource limits, an upgrade command, and the proof of the encrypted overlay on real machines. `design-swarm.md`, section 13, is the order of work.
