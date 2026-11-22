# Storage capacity: the check also judges room

**The problem.** A quota is a promise about one mailbox. Nothing relates the sum of the quotas to the room a tenant's S3 store really has, and the S3 protocol cannot say how much room is left. If the quotas add up to more than the store, the store fills first and writes fail (a delivery is answered `4xx` and retried, an IMAP append fails); the quota does not stop that.

**What the check does now.** The operator declares the room of a tenant's store (`PUT /api/v1/domains/{d}/storage/capacity`, platform administrators; the Storage page, in gigabytes). Every `storage.check` job, after its probe of the bucket, also reads what the tenant holds and has been promised and records a verdict on the store row (migration 0042):

| Figure | Meaning |
|---|---|
| `used_bytes` | what the mailboxes of the domain hold |
| `promised_bytes` | the sum of the quotas of its accounts (account, plan, domain, then the global layer) |
| `unlimited_accounts` | accounts with no quota at all |

Verdict `capacity_status`: `unset` (no room declared), `full` (held is at least the room), `nearly_full` (90 % or more), `overcommitted` (promised is more than the room, or any account has no quota), otherwise `ok`. Saving the room queues a check so the verdict follows at once. A domain administrator sees the verdict and the figures, never changes the room.

**Why declared and not asked.** An S3 server only says "yes" or "no" to a write; free space is a property of the disks behind it, reported (differently) by the admin APIs of RustFS or MinIO. The declared figure is the operator's contract with the store, and the check is what keeps the promises made in quotas honest against it. Reading the real free space from RustFS or MinIO is the next step.

**Roles.** The check runs in the outbound service's role: it gets `SELECT` on `plans` and `UPDATE` on the four capacity columns only.

Not done: a warning in the monitor page and in the swarm `verify` when a store is `nearly_full` or `full`; the platform store (its room is the disks of the deployment).
