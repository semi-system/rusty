# Domain settings and temporary passwords

A platform administrator sets, per domain, the defaults and limits its accounts follow; they are keys of the domain's configuration layer, so a change reaches the next delivery without a restart.

* `default_plan`: the plan a new account (created or imported) is put on when none is named.
* `max_accounts`: how many accounts the domain may have; creation and import beyond it answer `409 limit_reached`.
* `quota_bytes`, `max_message_bytes`: limits for accounts whose own plan and configuration set none.
* `password_min_length`, `password_classes`: rules for a password an administrator sets. A domain administrator may change only these two; everything else is `403`.

API: `GET`/`PUT /api/v1/domains/{d}/settings`. Page: the Settings tab of a domain.

## Plan keys

A plan also carries `messages_per_hour` (submission limit per account, default from the server setting) and `protocols` (`imap`, `pop3`, `submission`; absent means all). A protocol a plan leaves out refuses the sign-in of its accounts like a wrong password, without counting as a failed attempt.

## Temporary password

`POST /api/v1/accounts/{a}/password` takes `must_change: true` (migration 0039). IMAP, POP3 and submission refuse such an account with a message that says why, until another password is set without the flag.

## Not done

Showing and clearing a sign-in lockout needs the throttle state shared between services (it is in memory or Valkey, keyed by address and identity); deferred.
