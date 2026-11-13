# Running Rusty locally (development profile)

> **DEV ONLY. Do not put real mail in this.** `rusty d --dev` keeps the mail metadata in a PostgreSQL
> database you name in the environment, the message bodies in a blob directory under a data dir, uses a
> self-signed certificate kept in the data dir, and binds high ports. Production runs with `--config`. Treat
> the dev data dir and the dev database as disposable.

## The dev database

PostgreSQL is the only backend, so the dev profile needs a server. Name it in the environment:

| Variable | Meaning |
|---|---|
| `RUSTY_DEV_PG_URL` | `postgres://user@host:5432/database` (required; no password and no query parameters in the URL) |
| `RUSTY_DEV_PG_PASSWORD` | the password of that user (optional; never logged) |
| `RUSTY_DEV_PG_SSLMODE` | `disable`, `require`, `verify-ca` or `verify-full` (default: `disable` on loopback, `verify-full` elsewhere; a mode that does not verify a remote server is accepted here and warned about at every start) |
| `RUSTY_DEV_PG_CA_BUNDLE` | absolute path of the CA bundle that signs the server certificate |

The dev profile accepts any database role. Build the schema once, as a role that may create tables:

```sh
export RUSTY_DEV_PG_URL=postgres://rusty@127.0.0.1:5432/rusty RUSTY_DEV_PG_PASSWORD=...
$R ctl --dev --data-dir $D db migrate --without-roles     # idempotent; `db status` shows whether it is current
```

`bash scripts/pg-up.sh` starts a throw-away Postgres 18 container for this (its URL, with the password inside,
is in `~/.rusty-pg-test-url.dev`).

## What works and what does not

Works: SMTP inbound (port 25 equivalent), submission with STARTTLS and implicit TLS, IMAP with
STARTTLS and implicit TLS, `rustyctl` (domains, accounts, aliases, DKIM keys), local delivery between local
accounts, the outbound queue (direct MX delivery with retries and bounces, DKIM signing when keys are
configured).

Does **not** work yet:

- **Remote delivery from a dev machine.** The queue worker delivers to the recipient's MX on port 25,
  which most home and office networks block; it retries for up to 5 days and then bounces to the
  sender. Without a domain whose SPF/DKIM/DMARC records point at this machine, receivers will reject or
  junk the mail anyway. Set `[outbound] enabled = false` to make submission refuse remote recipients
  (`451 4.3.2`) instead of queueing them.
- **Valkey.** Not used yet.
- **DNS page / `rustyctl check` records for a real domain.** There is no admin UI; the domain
  `example.test` used below is fictional and has no DNS.
- **No real Internet mail.** There is no MX, no PTR, no public certificate. Do not point
  anything real at this.
- Hostname in the greeting, `Received` and `Authentication-Results` headers is `localhost`
  in dev.

## Ports

The dev profile uses high ports so no root is needed:

| Service | Port | TLS |
|---|---|---|
| SMTP inbound (MTA) | 2525 | plain, offers STARTTLS |
| Submission | 5587 | STARTTLS required before AUTH |
| Submission (implicit TLS) | 4465 | TLS from the first byte |
| IMAP | 1143 | STARTTLS (plaintext LOGIN refused) |
| IMAPS | 9993 | TLS from the first byte |

`--bind IP` chooses the address (default 127.0.0.1); `--port-offset N` shifts all five ports.

## Run it by hand (inside WSL)

```sh
# build once (from Windows Git Bash at the repo root; target dir lives on the WSL disk)
bash scripts/wsl-cargo.sh build --release --target x86_64-unknown-linux-musl -p rusty

# in WSL (the binary is ~/rusty-target/x86_64-unknown-linux-musl/release/rusty)
R=~/rusty-target/x86_64-unknown-linux-musl/release/rusty
D=$HOME/rusty-dev          # must not be group/world accessible; created 0700 if missing
$R ctl --dev --data-dir $D db migrate --without-roles   # once (see "The dev database" above)
$R d --dev --data-dir $D --bind 127.0.0.1 &      # leave running; Ctrl+C / kill -TERM stops it

$R ctl --dev --data-dir $D domain add example.test
$R ctl --dev --data-dir $D account add alice@example.test --prompt        # asks twice, no echo
printf '%s\n' 'a-long-password' | $R ctl --dev --data-dir $D account add bob@example.test --password-stdin
$R ctl --dev --data-dir $D account list example.test
```

Passwords are never accepted on the command line (minimum length is enforced).

Automated version of all of this, with assertions and teardown (uses a throwaway data dir):

```sh
bash tools/acceptance/local-run.sh          # inside WSL; add KEEP=1 to keep the data dir and log
```

It starts a throw-away Postgres 18 container (TLS with a generated CA, `deploy/db/roles.sql`), builds the schema as
the migrator role, runs the daemon as the unprivileged runtime role, sends a mail to alice through 2525, reads it over
IMAPS 9993 (LOGIN, LIST, SELECT, FETCH, STORE, EXPUNGE), sends alice -> bob through 5587 with STARTTLS + AUTH PLAIN,
checks bob receives it, and kills the daemon with SIGKILL in the middle of a burst of deliveries (no message lost, none
duplicated). It needs `bash`, `openssl`, `base64`, `timeout` and Docker.

## Reaching the ports from Windows

With default WSL2 NAT networking, a service in WSL that listens on `127.0.0.1` is reachable
from Windows as `localhost:<port>` (WSL2 localhost forwarding). This was verified: from Windows
PowerShell,

```powershell
Test-NetConnection localhost -Port 9993     # TcpTestSucceeded : True
Test-NetConnection localhost -Port 5587
```

returned True for 2525, 5587, 4465, 1143 and 9993 with the daemon bound to 127.0.0.1 in WSL.

What does not work: reaching the daemon from **another machine** (or from a Windows service
listening on the same port) through the WSL IP is not covered; binding `--bind 0.0.0.0` exposes
it inside the WSL network only, and the WSL address changes on restart. If you use mirrored
networking or `localhostForwarding=false` in `.wslconfig`, forwarding may behave differently;
test with `Test-NetConnection` first. Stopping WSL (VM shutdown) stops the daemon; the data dir
survives on the WSL disk.

## Thunderbird

Add a mail account manually (Account Settings, "Set up manually" / "Configure manually").
Use these values. The hostname is `localhost` because the dev certificate is issued for
`localhost`, `127.0.0.1` and `::1`.

| | Incoming (IMAP) | Outgoing (SMTP) |
|---|---|---|
| Server | `localhost` | `localhost` |
| Port | `9993` | `5587` |
| Connection security | SSL/TLS | STARTTLS |
| Authentication | Normal password | Normal password |
| Username | `alice@example.test` (the full address) | `alice@example.test` |

(Alternatives: IMAP `1143` with STARTTLS; SMTP `4465` with SSL/TLS.)

The first connection fails certificate verification because the certificate is self-signed. It is
made once and kept in the data dir (`dev-tls-cert.pem`, key `dev-tls-key.pem` mode 0600; subject the
configured hostname or `localhost`, valid for one year, replaced automatically about a month before it
expires or when the names change). In Thunderbird, when the "Security Exception" / "Add Security
Exception" dialog appears, choose **Confirm Security Exception** - once for IMAP and once for SMTP;
the exception stays valid across restarts of `rusty d --dev` with the same data dir. If the key file
is made readable by others, the start fails instead of silently replacing it. You may also add exceptions in advance under Settings > Privacy & Security > Certificates >
Manage Certificates > Servers.

Mail you send from alice to `bob@example.test` is delivered locally. A recipient outside the
configured domains is queued ("250", Thunderbird shows it as sent) and the outbound worker tries to
deliver it (see above); with `[outbound] enabled = false` it is refused with `451 4.3.2`.

## Stopping and cleaning up

`kill -TERM` the `rusty d` process (it drains and exits), then delete the data dir
(`rm -rf $D`) to start fresh.
