# Protocol switches

Decided 2026-11-17 (owner): every protocol the product has is **on by default**; a protocol that can be turned off is turned off in the **database** (the admin web), never in the environment or in a configuration file.

## What is a switch

* The listeners are always there. POP3 listens on `110` (`STLS`) and `995` (TLS ended at the edge) in every installation: the edge publishes the ports, the IMAP service binds `1110` and `1995` behind it. Nothing in `swarm.env`, `setup.sh` or `[admin]` selects them (the `POP3=1`, `--pop3` and `[admin] pop3` of the first version are gone). An operator who wants no listener at all names an empty address in `[imap]` (`listen_110 = ""`).
* Whether a protocol **answers** is a row of `platform_settings` (migration 0030): `protocol_pop3` with `enabled`. No row means on, so a fresh installation needs nothing. The key list is closed by a table check; a new optional protocol adds its key in a migration.
* Services read the table through `rusty_svc::ProtocolSwitches`: a copy renewed at most every 5 seconds, so a connection costs no query and a change reaches every replica within 5 s without a restart. If the database cannot be read the last known answer stands (on, before any read): a database hiccup must not close a protocol that was open. A protocol that is off answers each new connection with its own error (`-ERR POP3 is turned off on this server`) and closes it; connections already open finish.
* The admin web reads and writes it: `GET /api/v1/protocols` and `PUT /api/v1/protocols` (`{ "protocol": "pop3", "enabled": false }`), platform administrators only; the page is **Protocols** under Monitoring. The auto-setup answers (Thunderbird, Outlook, Apple) list a protocol only while it is on; they are cached for 5 minutes by clients.

## Roles

`rusty_admin` has `SELECT, INSERT, UPDATE` on the table; every service role has `SELECT`. No service can write it.

## Not done

* Valkey is not used: a five-second copy per process is enough for a value that changes by hand. If a switch ever needs to act within a second, the table already has `NOTIFY` triggers available (migration 0028 shows how).
* Per-domain protocol switches (POP3 for one tenant only) are not built; the table's key would gain a domain column.
