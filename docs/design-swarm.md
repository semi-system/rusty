# Design: deploying on a Docker Swarm cluster

Status: **plan for the owner's review** (2026-11-16, updated the same day: local cluster first, no Prometheus in the cluster). Nothing here is built. The goal the owner set: first build one Docker Swarm cluster, then deploy the platform onto it "by the model" (the five service images, the edge, the
filters, the replicas, the limits). This page is the deploy flow, step by step, and what has to change in the project to make it possible.

## 1. Decisions already taken (owner, 2026-11-16)

| Question | Answer |
|---|---|
| Where the cluster runs | **First a local test cluster** (section 3b: Docker-in-Docker nodes on the owner's machine); the real machines or VPS are looked for **only when the work is finished and passes there**. |
| Where the public ports are open | **Only on the Manager node.** The edge (HAProxy) runs there and nowhere else. |
| Message blobs | **An external S3 service**, always (no S3 server inside the cluster). |
| The database | **An external, dedicated PostgreSQL** ("as separate as the S3"): it is not a swarm service. |

Consequences, in short: the swarm holds only stateless services plus a few single-instance helpers; everything with data (PostgreSQL, S3) is outside and has its own backup; the Manager is the one public entrance and
therefore the one place that must be protected and monitored most closely (section 9 says what that costs).

## 2. The topology

```
internet ──► DNS A/AAAA (mail.example.org, admin.example.org, ...) ──► the Manager node
                                                                         │ host-mode ports 25 80 143 443 465 587 993
                                                                         ▼
  ┌────────────── Docker Swarm (overlay networks, encrypted) ───────────────────────────────────┐
  │ Manager:  edge (HAProxy, ACME)          [label edge=true is the manager itself]              │
  │ Workers:  smtp-in x N, submission x N, imap x N, admin x N, gc x 1, outbound x N (labelled)   │
  │           filter helpers: clamd + freshclam (one node, label av), rspamd, resolver            │
  │           valkey x 1                                                                          │
  └──────────────┬──────────────────────────────┬────────────────────────────────────────────────┘
                 ▼                              ▼
     external PostgreSQL (TLS, verify-full)   external S3 (HTTPS)       + the operator's machine (deploy tool, backups)
```

Nodes: one Manager (the entrance and the control plane) and at least two workers. A single Manager is a single point of failure for both mail ingress and swarm control: the running tasks keep running if it is
down, but nothing enters and nothing can be changed. Three managers with the edge on one of them is the next step, not part of this plan (it needs a floating address, section 12).

## 3. What changes compared with the standalone stack

| Standalone (`deploy/standalone`) | On the swarm |
|---|---|
| `docker compose`, one host | `docker stack deploy`; the stack file is **generated** by the deploy tool from a template and a parameter file |
| bind mounts of the state directory (`etc`, `keys`, `tls`, `secrets`) | **Docker configs** (the rendered `*.toml`, versioned by content hash) and **Docker secrets** (passwords, keys, the sealing key, the TLS key); no host path is shared between nodes |
| PostgreSQL and its roles in the stack, `pg_hba` by container name | PostgreSQL **outside**: `pg_hba` allows the **swarm nodes' addresses** with the per-service **role and password** (a task's outgoing traffic leaves with its node's address; reverse-DNS names of tasks are not usable) |
| `migrate`, `db-init` as one-shot services with `depends_on` | **replicated-job** services run by the deploy tool and waited for **before** the stack is updated (swarm has no `depends_on`) |
| blob volume `mail-data` | none: the platform store is `[blob_s3]` on the external S3 |
| fixed addresses for the edge and the resolver | none: swarm does not support them. The services accept the PROXY header from the **subnet of the `app` overlay** (section 9) and rspamd reaches Unbound through its **service name** |
| the edge finds backends by service name (VIP) | `tasks.<service>` (the DNS name that returns every task), so HAProxy sees each replica and its health |
| `cert-sync` reads the edge's HAProxy socket and writes files for STARTTLS | the certificate has to travel to **other nodes**: the deploy tool copies it into **versioned Docker secrets** and rolls the three TLS services (section 7) |
| ports published to the host | the edge only, in **host mode** (the client address is kept, which PROXY v2, SPF checks and the per-address limits need); nothing else publishes a port |
| `restart: unless-stopped` | `deploy.restart_policy`, `update_config`, `rollback_config` |

## 3b. The local test cluster (the first target)

Until the work is complete the cluster lives on the owner's own machine, as **Docker-in-Docker nodes**, with the two external dependencies as plain containers outside it, so the flow of section 6 runs unchanged
against it:

* **Nodes**: `rusty-m1` (the Manager) and `rusty-w1`, `rusty-w2` (workers), each a privileged `docker:dind` container on one Docker network (`rusty-cluster`); labels `outbound=true` on `w1` and `av=true` on `w2` stand
  in for the real roles. The machine to use is the WSL2 Ubuntu of the owner's PC (16 CPUs, 16 GB, 100 GB limit set earlier), which has room for three nodes plus the helpers; the LAN test server (4 CPUs, 7 GB) is
  the second choice.
* **External PostgreSQL** and **external S3** (RustFS, the S3 server of the test suite) are ordinary containers on a second network (`rusty-ext`) that the nodes can reach, with their own certificates from a throw-away CA;
  the database allows the nodes' addresses with the service roles exactly as it will on the real machine.
* **The edge** is a host-mode task on the Manager: its ports exist inside `rusty-m1`, which publishes them to the machine on high ports (`10025`, `10587`, `10993`, `10443`, `10080`, ...); a test client uses those ports.
  Certificates are self-signed (ACME cannot reach a local machine), so the certificate-sync of section 7 is exercised with that certificate.
* **Mail to the outside** is a fake remote MTA container on `rusty-ext` that the `outbound` node reaches, with the DNS names the flow needs on a local resolver; real delivery, reverse DNS, IP reputation and port 25 are
  what a real node adds and what the final acceptance run on the VPS is for.
* `deploy/swarm/local-cluster.sh up | down | status | node-stop NAME | node-start NAME` creates, removes and shows it and stops a node (a node loss test). It is the only script that knows about the dind layout; every
  other subcommand of `rusty-swarm` talks to a Manager over SSH or `docker -H`, so the same tool is used against the real cluster later.
* **What a local cluster cannot prove**, to be re-checked on the real one: the encrypted overlay (it needs IPsec in the kernel, which the WSL2 kernel may lack: the local run then goes without it and a check on the real
  nodes confirms it), the time and cost of the network between real machines, the real firewall rules, performance, and an outage of a whole machine.

## 4. The pieces, where they run and how many

| Service | Replicas | Placement | Notes |
|---|---|---|---|
| `edge` | 1 | the Manager (`node.role == manager`) | host-mode ports; `update_config.order: stop-first` (two tasks cannot hold the same host port); ACME state in a local volume on the Manager |
| `smtp-in`, `submission`, `imap` | 2+ each | workers, spread over nodes | `max_replicas_per_node` so two replicas do not land on one node |
| `outbound` | 1+ | workers **with the label `outbound=true`** | the node's public address is the sending address: its reverse DNS, SPF and `public_ips` must match; port 25 outbound must be open at that provider |
| `gc` | 1 | any worker | leases make a second one harmless, one is enough |
| `admin` | 2 | workers | reached only through the edge; its `allowed_peers` is the overlay subnet |
| `clamd` + `freshclam` | 1 + 1 | one worker with the label `av=true` (they share a local volume) | signatures are re-downloaded when the node changes |
| `rspamd`, `resolver` | 1 each | workers | the resolver is reached as `resolver` by rspamd and `[resolver] nameserver` of smtp-in (its address, resolved at start) |
| `valkey` | 1 | worker | no persistence; TLS and ACL users as in the standalone profile |
| (later) `webmail`, `rusty-dns` | N | workers; `rusty-dns` publishes 53/udp+tcp on the nodes that own the NS addresses | designed, not built |

Everything uses the published images (`ghcr.io/semi-system/...:TAG`, pinned), with `docker stack deploy --with-registry-auth` when the registry is private.

## 5. The inputs: `swarm.env`

One parameter file the deploy tool reads (it replaces the arguments of `setup.sh`). It is kept by the operator, never in the repository:

* the nodes: name, `user@host` for SSH, role (`manager` or `worker`), labels, the node's public address (for `outbound` and the edge);
* the public names: the mail host, the admin host, the platform's own domain, the contact address for ACME, the admin allow-list, the public IP(s) mail leaves from;
* the external database: host, port, database name, the CA that signs its certificate (a file), a superuser or owner login **for the one-time preparation only** (roles and migration), never stored in the cluster;
* the external S3: endpoint, region, bucket, prefix, and the access and secret keys (stored as swarm secrets);
* the profiles (`filter`, `valkey`, `monitoring`), the replicas per service, the image tag, the limits percentage (section 10);
* the sealing key pair: generated by the tool the first time and printed once for a separate backup; the unsealing key goes to the swarm as a secret for the services that open blobs.

## 6. The deploy flow, in order

Every step is a subcommand of one tool, `deploy/swarm/rusty-swarm` (a bash script, like `setup.sh`), run from the operator's machine over SSH to the Manager. Each step can be repeated.

**Phase A: the cluster (`rusty-swarm cluster ...`)**
1. `cluster check`: for every node in the inventory, over SSH: Docker (a version that supports `replicated-job`, `--cap-drop` on services and encrypted overlays), kernel modules for overlay encryption (IPsec/ESP) and IPVS,
   time synchronisation (NTP, because certificates and the database clock logic care), free disk, the **firewall rules between nodes** (`2377/tcp`, `7946/tcp+udp`, `4789/udp`, protocol `50` ESP) and from outside to the
   Manager (`25 80 143 443 465 587 993`). It prints what is wrong and changes nothing.
2. `cluster init`: `docker swarm init --advertise-addr <manager>` with an explicit data-path address, then `--autolock` (the key that unlocks the encrypted Raft store at a manager restart is printed once: the operator stores
   it apart); `cluster join` joins every worker with the worker token over SSH; `cluster label` sets `outbound=true`, `av=true`, `monitor=true`.
3. `cluster verify`: `docker node ls` all Ready and Active; a throw-away overlay with one task per node, a ping and a DNS lookup between them (the data path really works, encrypted).

**Phase B: the inputs of the cluster (`rusty-swarm prepare`)**
4. `prepare db`: connects to the external PostgreSQL with the one-time login, creates the roles of the services (`deploy/db/roles.sql`) and their passwords, and prints the `pg_hba` lines that allow the swarm nodes' addresses with
   those roles (the operator, or the tool over SSH, installs them on the database host). Everything is idempotent.
5. `prepare secrets`: creates the swarm secrets with **versioned names** (`pg_imap_v1`, ...; a secret cannot be changed in place): the service passwords, the Valkey passwords, the unsealing key, the S3 keys, the TLS key and
   certificate (self-signed at first, or the edge's ACME certificate once it exists, section 7). A rotation makes `_v2` and a rolling update moves the service to it.
6. `prepare configs`: renders the per-service TOML files from `deploy/standalone/templates/*.toml.in` (the same templates; the database host, the S3 section and the overlay subnet are the swarm's values), the HAProxy
   configuration, and creates them as **configs named by their content hash**; a change in content is a new config and so a rolling update.
7. `prepare images`: pulls the images by tag on every node that will run them (so the first start is not a download), verifies the signatures (`cosign verify`, as `docker.md`).

**Phase C: the database schema (`rusty-swarm migrate`)**
8. Runs the `migrate` **replicated-job** (`rustyctl db migrate` as the schema owner, which also applies the privileges) and waits for its result before anything else changes. This is the step that makes a rolling update safe:
   it runs once, on the new image, **before** any service moves.

**Phase D: the stack (`rusty-swarm deploy`)**
9. Generates `stack.yaml` from the template and the parameters (networks, placement, replicas, limits, healthchecks, update and rollback policy, logging limits) and runs
   `docker stack deploy -c stack.yaml --with-registry-auth --prune rusty`. Order is not needed: services that cannot reach what they need restart until it is there.
10. Waits until every service has converged (`docker service ls` replicas `N/N`, no task `Failed` or `Rejected`) or the timeout passes; prints the first service logs of the ones that did not.
11. **First start only:** the admin web prints the setup token in its log; `rusty-swarm logs admin` shows it, the operator opens `https://admin.<domain>/` from an allowed address.

**Phase E: acceptance (`rusty-swarm verify`)**
12. From outside: the edge answers on every public port; the TLS certificates are the expected ones; a message goes in through 25 and out through 587/465 and is read through 993; the filters refuse the standard test
    messages (GTUBE, EICAR); the monitor page shows every service and replica and `/metrics` answers on each task from the `metrics` network; a node is drained and the tasks move.

## 7. TLS certificates for STARTTLS

The edge obtains the certificate (ACME HTTP-01, on the Manager) and ends TLS for 465, 993 and the admin host. The ports 25, 587 and 143 are passed through: the **services** hold the key and do STARTTLS themselves, so the
mail host's certificate and key must reach the nodes where `smtp-in`, `submission` and `imap` run. The way chosen:

* the mail services read the files `/run/secrets/tls_cert` and `/run/secrets/tls_key` (the paths never change, only the secret behind them);
* `rusty-swarm certs sync` (run by a systemd timer or cron on the **Manager**, and by hand after the first start) asks the edge's HAProxy admin socket for the current certificate (the logic of `cert-sync.sh`), and **only if
  it changed** creates `tls_cert_vN` and `tls_key_vN` and runs `docker service update --secret-rm <old> --secret-add source=<new>,target=tls_cert ...` for the three services; the rolling update restarts them one replica at a time;
* the private key therefore also lives in the swarm's encrypted Raft store (with autolock) and in the memory of the tasks that use it, which is the same trust as today's `state/tls` directory. It is not given to any
  service that does not do STARTTLS.

Why a timer on the host and not a service: a service that could create secrets and update services would need the Docker socket, which is root on the Manager; a script the operator controls does not.

## 8. Rolling updates, rollback and the schema

* Every service has `update_config: { parallelism: 1, delay: 15s, order: start-first, monitor: 30s, failure_action: pause }` (the edge: `order: stop-first`) and a healthcheck, so a new task must be healthy before the next
  replica moves.
* **`failure_action: pause`, not `rollback`**, after a migration: the services refuse to start on a schema that is newer than their binary (`operations.md`), so swarm's automatic rollback to the old image cannot work
  after a migration. If a task of the new release never becomes healthy, the update stops with the others still on the old release (which keep running, they check the schema only at start).
* The rule of `operations.md` becomes load-bearing here: **a migration must keep the previous release working** (add first, remove in a later release), because during an update both releases run side by side on the
  same schema, and an old task that restarts in that window must still start. Today the code refuses an old binary on a newer schema outright; a **schema compatibility range** (each migration states the oldest release
  that may keep running on it, and a binary accepts a schema inside its range) is the follow-up that makes this safe instead of only a convention. It is listed in section 11.
* The way back after a bad migration is the same as in `operations.md`: restore the pre-upgrade backup of the external database (section 10) and deploy the old tag. `rusty-swarm upgrade --image-tag X` does: backup, `prepare
  images`, `migrate`, `deploy`, the convergence wait and the schema check, and says where the backup is.

## 9. Security model

* **Secrets** are Docker secrets (encrypted in the Raft store, **autolock on**, delivered as in-memory files only to the tasks that declare them). Each service gets only its own database password, its own Valkey
  password, and the unsealing key only if it opens blobs. No secret is in an environment variable, an image or the stack file.
* **The overlay networks are encrypted** (`driver_opts: encrypted: "true"`) because plain mail content crosses nodes after the edge ends TLS for 465, 993 and the admin host. The networks that need no way out are
  `internal` (the same split as today: `app`, `filter`, `kv`, `metrics`); the services that need the internet (`outbound`, `freshclam`, the edge, the resolver) also join an `egress` network.
* **The database allow-list** is now by **address and role**: the PostgreSQL host allows the swarm nodes' addresses, each role has its own password, and the host's firewall allows nothing else. The by-name list of the
  standalone stack cannot be used for an external server. A compromised service can still only do what its role can (`pg_service_roles.rs` is the contract).
* **The PROXY header** is accepted from the `app` overlay's subnet because the edge has no fixed address on a swarm. That widens "only the edge may say who the client is" to "any task on that network may". The
  hardening that closes it is a shared-secret TLV in the PROXY v2 header (HAProxy writes it, the services check it); it is a small code change and is listed in section 11. Until then the `app` network holds only the
  edge and the services that need it.
* **The Manager** holds the Docker API, every secret and the only public entrance: keep it patched, SSH by key only, the Docker API not exposed, and use `ufw`/nftables as `cluster check` verifies.
* The task hardening of the standalone stack (`read_only`, `cap_drop: ALL`, non-root users, tmpfs) carries over where `docker stack deploy` supports it. **`no-new-privileges` (`security_opt`) is not supported for swarm
  services in the versions the plan assumes: to be verified on the cluster** (the baseless images carry no setuid binary, which makes this a defence in depth rather than the only line).

## 10. Limits, logs, observability, backups

* **Limits**: `deploy/standalone/limits.sh` already reads the swarm (the sum of the Ready and Active nodes) and divides 80 % of it over the containers; the deploy tool turns that into `deploy.resources.limits` and
  `reservations` (reservations matter on a swarm: they are what the scheduler places by). The listener memory budget (`[limits] inflight_message_mib`) is set under each inbound service's limit.
* **Logs**: `json-file` with `max-size` and `max-file` on every service; a log shipper is out of scope.
* **Metrics**: the `[metrics]` listener of every service on an attachable `metrics` overlay. **No Prometheus runs in the cluster** (owner's decision: the endpoints are enough); an operator's Prometheus that joins
  the overlay scrapes `tasks.<service>:9100` and can use the sample rules (`design-metrics.md`). The admin web's Monitor page needs nothing new: the heartbeats come from every task's own database login.
* **Backups** live where the data is: the external PostgreSQL is backed up with its own tools (`pg_dump`/`pgBackRest` on the database host or by `rusty-swarm backup`, which runs `pg_dump -Fc` from a one-shot job over
  TLS) and the S3 bucket by its versioning or replication. The sealing key pair and the swarm's autolock key are the two things that exist nowhere else: back them up apart. `docs/backup.md` is extended.

## 11. What has to change in the project

| # | Change | Why |
|---|---|---|
| 1 | `deploy/swarm/`: `rusty-swarm` (cluster, prepare, migrate, deploy, verify, upgrade, certs, logs, ctl wrapper), `stack.yaml.in`, the parameter file's example | the deploy flow itself |
| 2 | `rustyctl` and `migrate` through **replicated-job** services (`rusty-swarm ctl ...` runs one and prints its log), because a swarm has no `docker compose run` and the overlay is not attachable by default | operations |
| 3 | HAProxy backends using `tasks.<service>`, and the sample scrape configuration likewise | replicas visible one by one |
| 4 | A check that the platform store can be **S3 only** for every role (no `blob_dir` on the nodes) and that the configuration validator says so clearly | the external S3 decision |
| 5 | PROXY v2 **authentication TLV** (HAProxy and the three services) | closes the widened PROXY trust of section 9 |
| 6 | **Schema compatibility range** in the migration metadata and the schema check | safe rolling updates and a meaningful swarm rollback |
| 7 | `limits.sh` writing the swarm form (`deploy.resources` with reservations) and reading the labelled nodes | already half there |
| 8 | An end-to-end test in CI on a **single-node swarm** with PostgreSQL and an S3 server as plain containers outside the stack: deploy, send mail, update, scale, drain, node-down is out | proves the deploy flow without a cluster |
| 9 | `docs/swarm.md` (the operator's guide) and updates of `operations.md` | |

## 12. What is not in this plan

A highly available database (PostgreSQL replication and failover is the database operator's job here), three managers with a floating address for the edge (keepalived or a provider's address), a log shipper, an S3
server in the cluster, and the webmail and `rusty-dns` services (they join the same stack later: the first needs one more edge host name, the second needs published UDP and TCP port 53 on the nodes that own the NS
addresses, which also have to be public).

## 13. Order of work

0. **The local test cluster** (section 3b) and the external PostgreSQL and S3 containers: `local-cluster.sh up` gives a Manager and two workers, healthy, with a working overlay.
1. `cluster` subcommands and `cluster check`/`verify`, tested against the local cluster (nothing of the platform yet): **the first result**.
2. The stack template with only `edge`, `smtp-in` and a configuration, a secrets and configs pipeline, the external-database preparation and the `migrate` job: the smallest thing that receives a message.
3. The remaining services, the filters and Valkey, then the limits and the logging limits; the certificate sync.
4. Monitoring (`tasks.<service>`), the Monitor page, `verify`.
5. `upgrade`, rolling updates and a rollback drill on the cluster; the CI single-node swarm test; `docs/swarm.md`.
6. The hardening items 5 and 6 of section 11 (small, independent PRs).

Each step is a pull request with tests, and each phase ends with a run on the **local cluster**. When the whole flow passes there (including the failure drills: a node stopped, a task killed, an update, a
rollback by restore), the owner finds the machines and the same flow is run on them as the acceptance of what a local cluster cannot prove.

## 14. What the plan needs from the owner (at the end, for the real cluster; nothing is needed now)

1. The machines: how many, their addresses, the SSH access (the same kind as the test server), which one is the Manager, which has the public address and open ports, which may send mail (port 25 outbound open,
   reverse DNS settable) and which has the most disk and RAM (it will hold ClamAV).
2. The external PostgreSQL: where it is, who runs it, a version (17 or 18 as the images assume), and a login that may create roles for the one-time preparation; whether the database host's firewall can be opened to the nodes.
3. The external S3: endpoint, bucket, region, and keys.
4. The public host names and the DNS control for them (including the platform domain's `autoconfig` and the ACME `HTTP-01` reachability on port 80 of the Manager).
5. The decision on a single Manager as the single entrance (this plan) or a floating address later.
