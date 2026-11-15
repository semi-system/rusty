# Who may use the admin web: administrator grants

An administrator is an **ordinary account** (e-mail and password, Argon2 as for IMAP) that has a row in `admin_grants` (migration 0021). Nothing in the
table is a credential; signing in will be the account's own password, checked by the directory.

| Grant | Meaning |
|---|---|
| platform (`domain_id` NULL) | every domain; may create domains |
| domain (`domain_id` = d) | the administrator of that one domain only; the account must belong to that domain |

One grant per account: granting again widens or narrows it. The database enforces "a domain administrator is an account of that domain" for any
writer (trigger, SQLSTATE `RS002`), a grant goes with its account (`ON DELETE CASCADE`), and a missing account or domain is an error, not a row.

## `rustyctl admin`

```
rustyctl admin grant ops@example.org            # platform administrator
rustyctl admin grant boss@client.net client.net # administrator of client.net only
rustyctl admin revoke boss@client.net           # the account stays; it is no longer an administrator
rustyctl admin list
```

* The account must exist and have a password (otherwise it could never sign in).
* This is also the **recovery path**: if no administrator can sign in, grant one from the shell.
* The first platform administrator is normally made on the web with the setup token (design section 14.3); that page exists only while
  `AdminStore::has_platform_admin()` is false, a database fact read each time.

## Not in this change

Sessions, sign-in, CSRF and the `rusty-admin` binary come next; until then a grant changes nothing a user can do.
