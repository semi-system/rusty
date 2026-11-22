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

## External list members (built 2026-11-22, same branch family)

* **Adding:** the admin API takes an address outside this server as a member (stored lower-cased); an address of a hosted domain that has no such account is a typo and is refused. A list may forward to at most 100 outside members.
* **Delivery:** the local members get a copy as before. The outside members become the recipients of ONE outbound job (per destination domain, as any queued mail), written after every local copy is stored. On port 25 the job is made by the SMTP service itself (`InboundHandler::with_forwarder`, on when `[outbound]` is enabled; without it the outside members are skipped as before). On submission it goes through the user's own queue accounting.
* **Sender of the forwarded copy:** on port 25 the envelope sender is `<list>-bounces@<domain>` (a name that exists nowhere, so what bounces is refused and never reaches the original sender) and the job is counted to the list's domain, with no account behind it (`account` 0 skips the per-account caps; the domain's caps still apply). On submission the sender is the user, as for any message.
* **Who may write:** on port 25 only a member of every list of the message with outside members (an outside member by address, a local one by account): an outsider could otherwise use the list to mail its members. Anyone signed in on submission may. The null sender may not.
* **Loops:** every copy carries `X-Rusty-List: <list address>`. A message that arrives with the header of a list it is addressed to, or with more than 25 `Received` fields, is refused with `554 5.4.6` (permanent, so the sender does not retry). Two lists that forward into each other, or an outside member that forwards back, end at the second hop.
* **Not done:** moderation, a reply-to policy, unsubscribing by mail, and per-list posting rules. The sender who is also an outside member receives their own message back.
