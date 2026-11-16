# Design: folder sharing and ACLs

Status: design, not built. It gives a mailbox owner the equivalent of Zimbra folder grants (share a folder to read, write or manage, delegate, share with a group) on top of the standard model, IMAP ACL
(RFC 4314), so IMAP, JMAP and later CalDAV/CardDAV share one permission table.

## Constraints

* A domain (tenant) is a hard security boundary. Sharing stays inside one tenant in the first version; the database enforces it.
* Every check runs in the store layer, from the caller's capability, never trusted from a protocol service.
* Without the `lookup` right a folder answers `NotFound`, identical to a folder that does not exist.
* A domain administrator can see and revoke grants in their tenant, but still cannot read mail.
* Out of scope: cross-tenant sharing, public links, guest shares, send-as / send-on-behalf (a separate design), per-user read state on shared folders, negative rights.

## Grantees

| Kind | Meaning |
|---|---|
| `account` | one account of the same tenant |
| `list` | every account member of a distribution list of the same tenant (external members are ignored); membership changes apply at once |
| `domain` | every signed-in account of the tenant (company-wide shared folders) |

The owner always has every right on their own mailbox, with no grant.

## Rights

A bitmask following RFC 4314: `l` lookup, `r` read, `s` keep seen state, `w` write flags, `i` insert, `p` post, `k` create child folder, `x` delete or rename folder, `t` mark messages deleted,
`e` expunge, `a` administer the ACL. UI presets are only names for masks: viewer `lrs`, contributor `lrsip`, editor `lrswipkte`, manager everything but `a`, admin everything.

Rules: no right is effective without `lookup`; rights only add up (no deny); inheritance can be cut with a per-folder flag.

## Data

A `folder_grants` table keyed by (mailbox, folder, grantee) with the rights mask, the tenant (enforced by composite foreign keys, so a grant to another tenant is impossible), who granted it and when;
`folders.inherit_grants` (default true: a child inherits its ancestors' grants, cut with false); an `acl_version` counter per mailbox bumped on every change (cache key); an append-only `grant_log`
(triggers refuse edits and deletes). Limits: 200 grants per folder, 5,000 per mailbox.

## Effective rights

For caller `U` on folder `F` of mailbox `M`: the owner has everything; otherwise the OR of the `account` grant for `U`, the `list` grants of lists containing `U`, and the `domain` grant, on `F` and, while
`inherit_grants` holds, on each ancestor. One recursive query (depth limit 64), with `U`'s list memberships cached briefly by `acl_version`. No `lookup` means `NotFound` for everything.

The shared `\Seen` flag is the owner's in the first version.

## Enforcement

* Every operation takes an `Actor` derived from the capability and the target mailbox, and goes through a `ScopedMailbox` that checks the needed right in the same transaction as the change. Right per
  operation: list folders `l`; list or read messages `r`; flags `w` (seen needs `s`); add or copy in `i`; create child `k`; delete or rename folder `x`; mark deleted `t`; expunge `e`; see or change the
  ACL `a`; a move needs `t` at the source and `i` at the target.
* A locked owner or locked grantee makes the grant unusable. Revocation applies at once; an open session is refused on its next call. Quota counts against the owner, including mail others add.
* No escalation: someone with `a` can grant at most the rights they hold, and cannot change the owner's grants.

## Protocol effects

* IMAP: shared folders appear under the "Other Users" namespace (RFC 2342); `SETACL`, `GETACL`, `LISTRIGHTS`, `MYRIGHTS`, `DELETEACL` map directly; `LIST` returns only folders with `lookup`.
* JMAP: access another account through `accountId` with `myRights` (RFC 8621).
* Change feed and search are filtered by the folders the caller can read; creating or deleting a grant writes a change record for the grantee. Cache keys include `acl_version`.
* Calendars and contacts reuse the same table through a folder `kind`.

## Admin and audit

Owners manage the grants of their folders; holders of `a` within the escalation rule. Domain administrators list and revoke any grant in the tenant (`rustyctl acl ...` and the admin web); everything
is written to the admin audit log and the grant log. Reports: who can reach this folder, and which folders are shared out.

## Tests

A permission-by-operation matrix with a reference model (property tests); inheritance and its cut; list membership changes; negative tests for cross-tenant foreign keys; `NotFound` indistinguishable from
non-existence; revocation in the middle of a multi-step operation and concurrent grant, revoke and use; no escalation; search and change feed always filtered; the grant limits and the depth bound; effective
rights computed in one query (checked with a statement counter).

## Order of work

1. Migrations (`UNIQUE (id, domain_id)` where missing, `folder_grants`, `acl_version`, `grant_log`, `inherit_grants`) with negative tests.
2. The effective-rights query, the reference model and property tests.
3. `ScopedMailbox` and the per-operation table; the current `Mailbox` becomes the owner-only path.
4. Grant management (set, change, revoke, list) with the escalation rule, the log and the version cache.
5. Store API, `rustyctl acl`, filtering of the change feed and search.
6. IMAP ACL and JMAP sharing when those protocols exist.

## Risks

ACL code is where privilege escalation bugs live: a reference model, property tests and a separate security review are mandatory. The check must sit on every read path (search, sync, export, mailbox
moves, caches); keep a checklist and cross tests. Shared read state may surprise users; document it.
