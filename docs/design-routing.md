# Where mail goes besides accounts: lists, catch-all, domain aliases

Built 2026-11-22 (P1 item 6 of `design-admin-parity.md`), first slice.

* **Distribution lists** (tables `lists` and `list_members`, which existed): the admin web makes and deletes a list and adds or removes members on the new **Routing** tab of a domain. Mail to a list address is delivered to each member account (the SMTP service already did this, with a cap of members and one copy per account). A member is an account of any hosted domain. A list is the way to **forward to several accounts**: an alias still names one account.
* **Catch-all** (`domains.config` key `catch_all`, a local part): the account, alias or list of the domain that takes the mail of names that do not exist. It is only consulted when nothing matched, so a real address always goes to its owner. Setting one checks that it names something that exists. Without one, unknown names are refused as before.
* **Domain aliases** (migration 0035, `domain_aliases`): `name@alias.test` is delivered as `name@real.test`, including the catch-all of `real.test`. A platform administrator adds them (an alias claims a name on the internet); the alias must not be a hosted domain or another alias. The address resolution (`DirectoryRepo::resolve`) does the work, after the ordinary lookup found nothing, so a normal delivery costs no extra query. One hop: an alias of an alias is not followed.
* **Grants:** the services that resolve addresses read `domain_aliases`; only the admin web writes it.

## Not done (next)

* **External members.** A list can only hold local accounts: the admin API answers `400 invalid` for an outside address, and the SMTP service still refuses mail to a list that has an external member. Forwarding needs the outbound queue and loop protection (a hop count on `Received` and a `X-Rusty-List` marker, with a sender that is the list's bounce address); it is its own change.
* A list that only some senders may write to, moderation, and a list's own reply-to.
* Domain aliases in the generated DNS records (the alias needs its own MX record; the page says so, the DNS page does not list it) and in the certificate of the mail host.
* For mail to an unknown name at an alias domain the SMTP service answers `554 relay access denied` and not `550 no such user`: it does not recognise the alias as one of its domains at that point.
