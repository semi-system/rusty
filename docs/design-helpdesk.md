# The helpdesk role

A third kind of administrator, for the person who answers "I forgot my password": a domain administrator (migration 0041 adds `admin_grants.helpdesk`, valid only with a domain) that may

* sign in, see the domain list and the accounts of its domain,
* set an account's password (also as a temporary one) and lock or unlock the account.

Everything else is `403 forbidden`: creating or deleting accounts, aliases, lists, settings, plans, queue, DKIM, DNS, storage, administrators and the audit log. The rule is one function, `Principal::may(method, path)`, called from `AdminApp::require`, which every handler passes through, so a new route is closed to the role until it is added to that list.

A platform administrator grants it (`POST /api/v1/admins` with `"role": "helpdesk"` and a domain; the Administrators page has a checkbox); granting again replaces the grant and so drops the role.

Upgrade note: grant the role only after every admin instance runs the new version; an older instance does not know the flag and would treat the grant as a full domain administrator.

Not done yet: the web console does not yet hide the pages a helpdesk administrator cannot use (they answer 403 with the usual message).
