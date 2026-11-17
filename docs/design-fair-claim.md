# Fair claiming of jobs between tenants

Decided 2026-11-17 (owner): the outbound worker takes one mail per tenant per round, not the oldest mail of the whole queue.

## Problem

`JobRepo::claim` returned the oldest eligible job of the queue. A tenant that queues 10,000 mails delays every mail of every other tenant queued after it until its backlog is sent.

## Rule

A tenant is a domain; jobs with no domain (platform jobs) are one more tenant. The claim order is:

1. a running job whose lease expired (a worker died), oldest first: recovery goes before everything;
2. queued jobs by their **place in their own tenant's line** (the oldest job of every tenant, then the second oldest of every tenant, ...);
3. inside a place, the tenant **served least recently** first (a tenant never served first), then the older job.

So with tenant A holding 20 mails and B and C one each, the claims go A, B, C, A, A, A, ... Nothing is reserved for a tenant that has nothing queued, and a tenant alone gets the whole worker.

## How

* `job_turns (scope, kind, last_claim)` (migration 0031): the number of the last claim of a tenant for a kind of job, from the sequence `job_turn_seq` (no clock ties). Written in the claim transaction, after the job is marked running.
* The window of 32 candidates (`CLAIM_PEEK`) finds the tenants that have queued jobs by a skip scan on `jobs_queued_scope`, takes up to 8 jobs per tenant, and orders them as above. The lock step takes the first one of the window that no other claimer holds (`SKIP LOCKED`, in window order), so concurrent workers spread over tenants and, when a tenant is alone, over its own jobs.
* A round whose window is entirely held by other claimers is followed by one that starts 8 jobs deeper in every line, up to 16 rounds (the old cap of 512 rows read).
* A claim costs one more statement (the turn). A claim reads one index probe per tenant that has queued jobs: thousands of tenants cost thousands of small probes, tens of thousands of tenants would need a different shape (a table of tenants with work).

## Not done

* Weights (a paying tenant served more often) and a cap of mails in flight per tenant: the order is equal turns only. Per-tenant sending limits are the plans/quota work.
* Fairness between the accounts of one tenant.
