# Mail rules: forwarding and the automatic reply

Two server features every account may have, kept as one JSON object, the key `mail_rules` of the **account's own** configuration (not inherited from a plan or a domain):

```json
{ "forward": { "to": ["me@other.example"], "keep": true },
  "vacation": { "enabled": true, "subject": "Away", "body": "Back on Monday.", "from": 1700000000, "until": 1700500000 } }
```

The administrator sets it with `GET`/`PUT /api/v1/accounts/{a}/mail-rules` (the Rules button of the Accounts page); the webmail will give the person the same two calls for their own account (design-webmail.md). An empty object removes the rules.

**Where they act.** In the inbound service, on port 25, right after a copy is stored for the account (and only when it was stored now, not found again by a retry): `apply_rules`. A rule that cannot run is logged and never fails the delivery.

* **Forwarding.** One outbound job to the listed addresses (at most ten), from the address the mail was sent to as envelope sender, with a header `X-Rusty-Forwarded: <that address>` in front. The copy in the mailbox is kept (`keep: false` is stored but version 1 always keeps it). The guard against loops: a message that already carries three such headers is not forwarded, and an address that already forwarded it is skipped, as is the account itself.
* **Automatic reply.** While `enabled` and inside `from`..`until`. One reply per sender and week: `vacation_replies` (migration 0043) is claimed in one statement (`INSERT ... ON CONFLICT DO UPDATE WHERE older than a week`), so two deliveries at once cannot both answer. The reply is sent from the address the mail was sent to, to the envelope sender, with the null sender, `Auto-Submitted: auto-replied`, `Precedence: bulk` and `In-Reply-To`. Never answered, whatever the rules say (RFC 3834): the null sender, `mailer-daemon`/`postmaster`/`no-reply` and `-bounces`/`-request` senders, anything with `Auto-Submitted` (other than `no`), `Precedence: bulk|junk|list`, `List-Id`, `List-Unsubscribe` or `X-Auto-Response-Suppress`, and the account writing to itself.
* **Retention.** The vacation book-keeping is purged after 60 days.

Not done: rules on mail an account of this server sends to another account of this server (submission delivers locally without this step); the web form does not set the start and end dates of the reply (the API does and the form keeps them); sorting and filing rules and ManageSieve (the next step, with the webmail's filter page); forwarding without a kept copy.
