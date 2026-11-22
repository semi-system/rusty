# DKIM rotation

Every key of a domain signs (the outbound service signs with each selector it finds), so a rotation needs no switch:

1. **Rotate** makes a new key under a selector named after the day (`r20261122`), the DNS page then shows the record to publish. Both keys sign from now on.
2. After the new record has been published for a few days (receivers cache the old one, and mail in transit is checked later), **retire** the old key: `DELETE /api/v1/domains/{d}/dkim/{k}` removes its public row (the signer stops using it at once) and then its sealed private half.
3. Remove the old record from DNS.

The last key cannot be retired (`409 wrong_state`): the domain would send unsigned mail. A key shows when it was made (`created_at`), so the page tells which one is old.

Not done: an automatic rotation on a schedule (it needs the DNS to be published by the operator or by a DNS server of ours, which is the separate DNS project).
