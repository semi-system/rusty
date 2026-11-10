# Running Rusty locally (development profile)

> **DEV ONLY. Do not put real mail in this.** `rusty d --dev` keeps everything in a SQLite file
> and a blob directory under a data dir, uses an in-memory self-signed certificate, and binds
> high ports. The production backend will be Postgres (not wired yet) and production runs with
> `--config`. Treat the dev data dir as disposable.

## What works and what does not

Works: SMTP inbound (port 25 equivalent), submission with STARTTLS and implicit TLS, IMAP with
STARTTLS and implicit TLS, `rustyctl` (domains, accounts, aliases), local delivery between local
accounts.

Does **not** work yet:

- **Remote delivery.** The outbound queue (U5) is not merged. Mail to an address outside your
  local domains is not delivered anywhere. Nothing reaches the Internet, and nothing is signed
  with DKIM (`rustyctl dkim generate` is a stub).
- **Postgres / Valkey.** Production storage wiring (U11) is not done; the dev profile uses SQLite.
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

It sends a mail to alice through 2525, reads it over IMAPS 9993 (LOGIN, LIST, SELECT, FETCH,
STORE, EXPUNGE), sends alice -> bob through 5587 with STARTTLS + AUTH PLAIN, and checks bob
receives it. It needs only `bash`, `openssl`, `base64` and `timeout`.

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

The first connection fails certificate verification because the certificate is self-signed and
regenerated at every start. In Thunderbird, when the "Security Exception" / "Add Security
Exception" dialog appears, choose **Confirm Security Exception**. You will have to do this again
after every restart of `rusty d --dev` (new certificate each time) - once for IMAP and once for
SMTP. You may also add exceptions in advance under Settings > Privacy & Security > Certificates >
Manage Certificates > Servers.

Mail you send from alice to `bob@example.test` is delivered locally. A recipient outside the
configured domains is accepted by the submission server ("250 queued", Thunderbird shows it as sent) but nothing leaves
this machine (outbound is not implemented).

## Stopping and cleaning up

`kill -TERM` the `rusty d` process (it drains and exits), then delete the data dir
(`rm -rf $D`) to start fresh.
