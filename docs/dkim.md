# DKIM keys

Mail that leaves Rusty is signed with a DKIM key per domain. This page says where the keys live, what is stable and how to
rotate.

## Where the two halves are

| Half | Where | Who reads it |
|---|---|---|
| **Private key** | a file `<domain>.<selector>.pem` (RSA) or `<domain>.<selector>-ed25519.pem` (ed25519) in `[outbound] dkim_dir`, mode 0600 | only the outbound service, which signs |
| **Public key** | the table `domain_dkim_keys` (migration 0018) | the admin web and `rustyctl check`, to show the record to publish and to check it |

The admin web therefore never touches a private key: a compromised admin cannot leak a signing key. Making a key is an
operator action (`rustyctl`), not a button.

## Making a key

```
rustyctl dkim generate example.org [--selector S]     # an ed25519 key (selector S, published as S-ed25519)
rustyctl dkim import example.org key.pem [--selector S]   # an RSA key (2048 to 4096 bits) or an ed25519 key you made
```

RSA keys are imported, not generated, because no acceptable RSA key generator is in the dependency tree:
`openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out key.pem`, then import it and delete the file. Both commands
write the private key (mode 0600, never overwriting), keep the public half in the database and print the TXT record to publish.
A domain can have an ed25519 and an RSA key side by side (two selectors).

## It is stable

* **A key is made once and never replaced.** `generate` refuses to overwrite the file; run again with the same selector it
  **succeeds, changes nothing and prints the record of the key that is there** (`created: false` in `--json`), so a lost record can
  be read again without a new key.
* The record is derived from the key, so the same key always gives the same record; the DNS check does not report drift for it.
* The database refuses to change a stored public key at all (a trigger), whoever writes: a different key is a different selector.
* An older key file made before the database held public halves is recorded the next time `dkim generate` is run for it (RSA keys:
  import the same file again after moving the old one away, or make a new selector).

## Rotating

Make a key under a **new selector**, publish its record, wait for DNS to carry it (hours to a day), then point signing at it and
remove the old key and record after the longest message lifetime you care about. Two keys can be published at the same time;
a domain publishes at most four.

## The record

`v=DKIM1; k=ed25519; p=<base64 of the raw 32-byte key>` or `v=DKIM1; k=rsa; p=<base64 SubjectPublicKeyInfo>`. The `k=` tag follows
the algorithm of the key (an earlier version printed `k=rsa` for every key, which was wrong for the ed25519 keys that
`dkim generate` makes).
