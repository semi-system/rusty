# The Backups page (backup status)

The deployment tool makes backups (`rusty-swarm backup`, `backup verify`, the nightly timer); until now nobody saw that it had stopped. This is the page that shows it.

* **The tool writes, the admin web reads.** After each backup, drill (a dump restored into a scratch database and read) and failure, `rusty-swarm` inserts a row into `backup_runs` (migration 0048: when, kind `backup`, `verify` or
  `restore`, ok, the size of a dump, a line of detail of at most 500 characters) with the database login that stays the operator's. The services never write it; the admin web's role has `SELECT` only and the garbage
  collector removes rows after 400 days. A row that cannot be written (an older schema, the database down) is a warning in the tool's output and never a failed backup.
* **`GET /api/v1/backups`**: the newest 60 runs and the server's clock, for platform administrators (a domain administrator gets 403).
* **The page** (Monitoring, "Backups"): the last good backup and its age (green under 36 hours, amber under three days, red beyond, or when none is on record), whether the newest drill passed and what it read (the
  migration count and the counts of domains, accounts and messages), and the list of runs. An empty history is an invitation to run a backup or install the timer.
* What it does not do: it does not start a backup (the action stays in the tool, which has the database login and the volumes) and it does not say whether a copy left the machine.
