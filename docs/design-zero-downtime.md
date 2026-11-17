# Updates without downtime

Decided 2026-11-17 (owner): after this, a new release must be installable without stopping mail and without anyone having to change the architecture first. This is what makes that true, and the rules the project now keeps (some by tests).

## What a routine update does

`rusty-swarm upgrade` takes a backup, makes the secrets and configs that changed (configs are named by the hash of their content, so an unchanged one is not touched), runs the **migration job**, then `docker stack deploy`. Swarm replaces only the services whose specification changed, one replica at a time (`parallelism: 1`, `order: start-first`, a health check, a pause between replicas, rollback when a task does not come up). Every mail service runs **two replicas** by default, so while one is replaced the other answers; the edge keeps the connections of everyone else because it is not in the update at all unless `haproxy.cfg` or its image changed.

## The part that used to be a convention: the schema

During an update the old and the new release run side by side on the same database, and an old task that restarts in that window must still start. Before, a binary refused any schema that was not exactly its own. Now (migration 0033, `schema_compat`):

* every migration from 0034 on writes one row `(version, min_binary)`: the schema version of the **oldest binary that keeps working** once the migration is applied. A binary starts on a schema that is **newer** than its own when every newer migration has a row with `min_binary` at or below the binary's version (it logs a warning: an update is in progress). A migration with no row, or one that asks for a newer binary, stops it, as before. A schema that is **older** than the binary is still refused: the migration job runs first.
* `crates/rusty-store/tests/migration_rules.rs` enforces the **expand and contract** discipline in CI: a migration that says an older binary keeps working may only add (no drop, no rename, no type change, no new `NOT NULL` without a default, no deletion). Anything else is a **breaking** migration: its first line is `-- rusty:breaking <why>` and its row says `min_binary` = its own number. An index on a big table (`messages`, `jobs`, `change_log`, `deliveries`, `blobs`, `blob_locations`, `keywords`, `folders`) must be `CREATE INDEX CONCURRENTLY` in a file that starts with `-- no-transaction`, because a plain build blocks every delivery while it runs.
* The way to remove or change something is two releases: release N adds the new shape and the code writes both and reads the new one; release N+1 (once N is everywhere) has a breaking migration that drops the old shape. Nothing breaking is ever released together with the code that needs it.
* The migrator gives up on a lock after 5 seconds and tries again (12 times, 5 seconds apart) instead of waiting: a DDL statement that waits for a lock makes every query on that table queue behind it, which is an outage however short the DDL is.
* **What crosses releases besides the schema** keeps the same rule. Job payloads: the decoders ignore fields they do not know, and `crates/rusty-smtp/tests/payload_compat.rs` freezes the format of today (add a new frozen sample when the format grows, never edit one). The admin API and the web are one service. Configuration files are strict (`deny_unknown_fields`) and travel with their service in the same swarm update, so an old task never reads a new file.

## What can still interrupt, and why

* **The edge** restarts (a few seconds with no connections on every port; senders retry, clients reconnect) only when `haproxy.cfg` changes (a new switch, a new service) or the HAProxy image is bumped; not for a routine release. One edge per host owns the host ports. Zero interruption there needs two edges on two nodes behind a floating address, which is the multi-node layout (`design-swarm.md`), not a code change.
* **The admin web** has one replica unless `REPLICAS_ADMIN` is set with Valkey for the sessions; mail does not pass through it.
* **The database and the object store** are infrastructure: a Postgres major upgrade or a primary failover is the operator's, with the database's own tools.
* A **breaking migration** cannot be rolled in: the deploy stops the old tasks first. It is always the second release of a two-release change, so by then no old task depends on what it drops.

## A release author's checklist

1. A migration only adds, and writes its `schema_compat` row with `min_binary` = the schema version of the release before it.
2. A new field in a job payload or any stored JSON is optional on read.
3. A new listener or switch must be off or harmless until the whole fleet runs the new release (a protocol switch in the database is: the old tasks do not read it).
4. Something to remove: this release stops using it, the next one drops it.
