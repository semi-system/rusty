# The outgoing queue and the message trace

Built 2026-11-22 (P1 item 7 of `design-admin-parity.md`), first slice.

## Queue manager

* The queue is the `jobs` table (kind `outbound`, one job per destination domain of a message, see `design-fair-claim.md` for how workers take them). The **Queue** tab of a domain lists its jobs newest first (50 a page), with the recipients still to be served, the sender, the number of tries, when the next try is due and why the last one did not succeed. Filter by state (waiting, held, being sent, failed, given up) and search by an address.
* **Actions** (`/api/v1/domains/{d}/queue/{n}/...`): **send now** and **release** (`run_after` = now), **hold** (`run_after` far in the future, year 3000, so no worker claims it and the page shows it as held), **try again** (a given-up job goes back to `queued` with a fresh count of tries), **delete**. Each is one statement limited to the domain and to the states where it is safe: a job a worker holds (`running`) is never changed, and anything else answers `409 wrong_state`. Delete asks first in the web. A held job never expires by age until it is released (the age check happens when a worker takes it).
* Domain administrators manage their own domain's queue; platform administrators every domain (one at a time). No change is made to the schema for this.

## Message trace

* `mail_log` (migration 0036): one row per recipient outcome of a delivery attempt of the outbound worker: `delivered` (with the server that accepted it), `deferred` (with the words of the 4xx), `bounced` (5xx), `expired` (given up after the retry period or the body is gone). Written by the worker through `QueueRepo::log` (best effort: a failure of the log never decides a delivery), read by the trace in the same tab, removed after 30 days by the garbage collector. `rusty_outbound` can only insert, `rusty_admin` only read, `rusty_gc` only delete.
* The trace is searched by sender or recipient and shows when, to whom, the result, and what the other server said. It covers **outgoing** mail.

## Not done

* The **incoming** half of the trace (accepted from whom, delivered to which mailbox, refused by a filter): needs a write on the delivery path of every message, so it wants its own measurement first.
* A queue across all domains for the platform, bulk actions (hold or delete everything for a domain or sender), and a flush of the whole queue.
* The log rows are not linked from a message to its original submission (no queue id shown to the sender).
