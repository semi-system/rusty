# Rusty Mail Server: documentation

Everything here is written in English. Start with `architecture.md` and `roadmap.md`.

## Overview

| Page | What it covers |
|---|---|
| [architecture.md](architecture.md) | Services, crates, data model rules, consistency, scale targets |
| [roadmap.md](roadmap.md) | Scope levels, milestones and the state of every project |

## Designs

| Page | State |
|---|---|
| [design-per-service-images.md](design-per-service-images.md) | Built: one binary and one image per service |
| [design-admin-web.md](design-admin-web.md) | Built: the admin web (own binary, sessions in the key-value store, embedded assets, message catalogues) |
| [design-s3-per-tenant.md](design-s3-per-tenant.md) | Built: blob stores per tenant |
| [design-filters-and-resolver.md](design-filters-and-resolver.md) | Built: rspamd, ClamAV, a validating resolver |
| [design-authoritative-dns.md](design-authoritative-dns.md) | Designed, review pending |
| [design-webmail.md](design-webmail.md) | Designed, review pending |
| [design-folder-sharing.md](design-folder-sharing.md) | Designed |

## Running and operating

| Page | What it covers |
|---|---|
| [first-mail.md](first-mail.md) | Install the standalone stack and send the first mail |
| [operations.md](operations.md) | Upgrade, rollback and runbooks (drilled on the test server) |
| [docker.md](docker.md) | Images, the published packages, signature verification, the filter profile |
| [postgres-production.md](postgres-production.md), [valkey.md](valkey.md) | The database and the key-value store |
| [blob-stores.md](blob-stores.md), [sealed-secrets.md](sealed-secrets.md) | Message storage and secrets at rest |
| [acme.md](acme.md), [dkim.md](dkim.md), [dns.md](dns.md), [relay.md](relay.md) | Certificates, DKIM, DNS records, outbound relays |
| [backup.md](backup.md) | Backup and restore |
| [admin-deploy.md](admin-deploy.md), [admin-grants.md](admin-grants.md), [admin-web-frontend.md](admin-web-frontend.md) | The admin web |
| [local-run.md](local-run.md) | Running locally |

## Engineering

| Page | What it covers |
|---|---|
| [git-workflow.md](git-workflow.md) | Trunk-based flow, Conventional Commits, required checks, releases |
| [ci.md](ci.md) | The CI design and its gates |
| [buildserver.md](buildserver.md) | The remote build server |
| [clock-policy.md](clock-policy.md) | Which clock may be used where |
| [postgres-mutation-table.md](postgres-mutation-table.md) | Generated table read by a test (do not edit by hand) |
