# Audit log

Every change an administrator makes in the admin web is written to `audit_log` (migration 0040): when, who (the address it signed in with), from which address (the client address the edge reports), and what (HTTP method, the registered route and the path as sent). It is kept 365 days (the garbage collector removes older rows) and shown on the Audit page.

* **One wrapper, not one call per handler.** `Router::wrap_routes` wraps every route; `audit::record_changes` records a `POST`, `PUT` or `DELETE` that answered below 400. A new route is audited without its author remembering. Sign-in and sign-out (`/api/v1/session`) are not recorded: a sign-in has no actor yet.
* **What was set is kept, secrets are not** (migration 0047, `detail`). The path names what changed; the fields of a JSON request body are kept as a flat object of short text values (at most 30 fields, 120 characters each, nested objects flattened one level as `a.b`). A value whose field name holds `pass`, `secret`, `token`, `private`, `pem`, `code`, `totp`, `cred`, `seal` or `key` is written as `(hidden)` (the log shows the field was sent, never what it held); booleans are always shown, and `password_min_length` and `password_classes` are settings, not secrets. A body that is not an object is not kept, nor are the pieces of a mailbox transfer. The Audit page lists the fields under the request. What it does not show is the value *before* the change; for that the log of the previous row of the same path is the record.
* **The domain** of a row comes from the `{d}` or `{a}` parameter of the route; a change with neither belongs to the domain of a domain administrator, and to no domain for a platform administrator.
* **Who sees what.** `GET /api/v1/audit` (`before`, `domain`): a platform administrator sees every row and may filter by domain; a domain administrator sees only its own domain, and asking for another is `404`.
* **A failed write** is logged and does not undo the change (it has happened).

Not done: a row for refused changes and for sign-ins (planned with the lockout page), and a per-action description (the route and path are shown as they are).
