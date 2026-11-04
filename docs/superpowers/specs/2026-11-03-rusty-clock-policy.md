# Rusty clock policy (decision D2)

Status: decided (PG3). Applies to both backends. Tests: `crates/rusty-store/tests/clock_policy.rs`.

## Rule

1. **The database clock is authoritative for every correctness-relevant timestamp**: creation
   times, `internal_date` defaults, lease checks (`lease_expires_at`, `run_after` comparisons),
   `zero_since`, the GC grace cutoff, capability validity. SQLite spells it `unixepoch()`,
   Postgres `rs_now()` (`statement_timestamp()`); shared statement text uses the `{NOW}` token.
   A multi-statement transaction reads the clock once when it needs one value for several
   columns (Postgres `statement_timestamp()` differs per statement).
2. **The Rust clock is display-only** (log lines, metrics, UI hints). `SystemTime::now` and
   `Instant::now` do not appear in `rusty-store/src` or `rusty-directory/src` except where the
   operating system stamps a value itself (below). A test scans the source and fails otherwise.
3. **Caller-supplied values are data, not clock decisions** and are stored verbatim:
   - the IMAP APPEND date / `INTERNALDATE` a client supplies, and the message `Date:` header
     (protocol-defined data; no code path derives a correctness decision from them);
   - the `_at(now)` test twins (`deliver_at`, `claim_at`, `issue_capability_at`, ...): they bind
     the caller's value in place of `{NOW}`. They exist for tests only.
4. **File modification times** (blob files, temp files, quarantine copies) are stamped by the
   operating system and compared with the OS clock (`blob.rs`, `BlobStore::fs_now`). Two clocks
   take part in one GC run, the database's (rows: `zero_since`) and the blob volume's (files:
   mtime), and they can be different machines' clocks. The rules (review PG5 I2):
   - rows are judged by the database cutoff `db_now - grace`;
   - files are judged by `min(db_cutoff, fs_now - grace)`: a file is never deleted on the
     strength of a database clock that runs ahead of the volume clock;
   - `collect_blobs` is REFUSED (a storage error, nothing is touched) when
     `|db_now - fs_now| > grace / 4`. The minimum grace of one hour no longer "bounds any skew":
     it bounds only the tolerated skew (15 minutes at the minimum grace, 6 hours at the default
     24 h);
   - tests: `gc_conformance::gc_refuses_to_run_when_database_and_file_clocks_disagree` (the
     reproduction: database +2 h, one-hour grace, a fresh in-flight file) and
     `a_file_is_judged_by_the_file_system_clock_when_the_database_runs_ahead`.

Why: a correctness decision (is this lease still mine, is this blob past grace) taken against a
clock other than the one that granted the value can hand one lease to two workers or delete a
blob that is about to be referenced. Several stores and workers share one database; they do not
share a Rust clock.

## Audit (SQLite backend as of PG3; Postgres columns identical)

| Column / path | Source before PG3 | Policy | Fix / test |
|---|---|---|---|
| `domains.created_at` | Rust clock (`rusty-directory`) | DB clock | `{NOW}` in `DirectoryRepo::create_domain`; `directory_created_at_and_uid_validity_follow_the_database_clock` |
| `accounts.created_at` | Rust clock | DB clock | `{NOW}` in `create_account`; same test |
| `mailboxes.uid_validity` seed, system `folders.uid_validity` | Rust clock (`uid_validity_base`) | DB clock | `unixepoch()` / `rs_now()` in `create_account`; same test |
| `mailboxes.uid_validity` high-water on `create_folder` | Rust clock (`mailbox.rs now()`) | DB clock | `max(DB_NOW, uid_validity + 1)`; `mailbox_expunge_and_folder_uid_validity_use_the_database_clock` |
| `messages.internal_date` (`append`, `deliver`) | Rust clock | DB clock by default; `deliver_at(.., now)` stores `now` verbatim | `COALESCE(?, DB_NOW)`; `mailbox_append_internal_date_is_database_clock_and_at_twin_is_verbatim`, `spike_append_internal_date_and_delivery_created_at_follow_the_database_clock` |
| `messages.internal_date` (`deliver_many`) | DB clock (`COALESCE(?2, DB_NOW)`) | DB clock / caller `_at` | none needed; `mailbox_conformance` (PG4) |
| client IMAP APPEND date, `Date:` header | not stored as a clock | caller data, verbatim | no code path; documented here (rule 3) |
| `messages.deleted_at` (`expunge`) | Rust clock | DB clock | `DB_NOW` in `expunge`; same test as folder uid_validity. `expunge_many[_at]` was already DB clock / `_at` |
| `deliveries.created_at` | DB clock / `_at` | DB clock / caller `_at` | none needed; asserted in the spike test and in `clock_policy` |
| `jobs.created_at`, `updated_at`, `run_after` default | DB clock inside the transaction | DB clock | none needed; `job_lease_and_schedule_use_the_database_clock` (SQLite; PG5 repeats it on Postgres) |
| `jobs.lease_expires_at` (claim, heartbeat, fence, complete) | DB clock inside the transaction | DB clock | same test; stale-lease behaviour is in `tests/jobs.rs` |
| `jobs` retention purge | DB clock | DB clock | none needed |
| `blobs.zero_since` | trigger `unixepoch()` | DB clock (`rs_now()` trigger on Postgres) | `blob_zero_since_is_set_from_the_database_clock` |
| GC grace cutoff (`collect_blobs`) | `SystemTime::now()` | DB clock | cutoff read with `SELECT unixepoch()`; `gc_grace_cutoff_comes_from_the_database_clock`, source scan |
| GC `purge_deliveries_default`, `purge_capabilities_default` | DB clock | DB clock | none needed |
| `capabilities.issued_at`, `expires_at`, `revoked_at`, validity checks | DB clock / `_at` | DB clock / caller `_at` | `DirectoryRepo` capability methods take `now: Option<i64>`; `directory_conformance` |
| `change_log` | no timestamp column | n/a | n/a |
| Blob file / temp / quarantine mtimes | OS clock (`SystemTime::now` in `blob.rs`) | OS clock (rule 4) | allow-listed in the source scan |

Open items: GC on Postgres and `jobs` on Postgres arrive with PG5; their clock tests reuse the
bodies above through `for_each_backend`.

Caller-supplied timestamps (both backends, `mailbox_conformance::caller_supplied_dates_are_verbatim_and_bounded`): the `_at` twins (`Mailbox::deliver_at`, `deliver_many_at`,
`expunge_many_at`, the capability `*_at` methods) are `#[doc(hidden)]`, store the value verbatim
(rule 3) and reject anything outside `0..=MAX_SUPPLIED_TIME` (2100-01-01) with `Error::Invalid`.
A future client IMAP APPEND date must go through the same bound. `collect_blobs_with_db_clock`
exists so a test can skew the database clock and prove the GC cutoff follows it
(`gc_cutoff_follows_a_skewed_database_clock`).

Error-class note (decision pending, unchanged): Postgres 57P01/57P02/57P03 (admin shutdown,
crash recovery), 25P03 (idle-in-transaction timeout) and 08xxx map to `Error::Storage`, not
`Busy`, because D1 does not list them. For failover, `Busy` is probably the better class for
57P0x; revisit in PG8. Capability eviction at the cap orders by `issued_at`, then `token_hash`,
on both backends.
