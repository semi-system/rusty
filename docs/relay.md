# Sending through an SMTP relay

By default Rusty delivers mail straight to the recipients' mail servers, which means the reputation of the sending IP
address is yours to look after. With `[outbound.relay]` all remote mail goes through a smarthost instead (Amazon SES,
SendGrid, Mailgun, Brevo, a provider's submission server, or a server of your own) and the reputation of the address is the
provider's concern.

```toml
[outbound.relay]
host = "smtp.example.net"
port = 587                       # 587 with starttls, 465 with implicit, 25 only with tls = "none"
tls = "starttls"                 # "starttls" (default) | "implicit" | "none"
username = "mailer"
password_file = "/etc/rusty/relay.pw"   # absolute path, mode 0600, one line; never inline
```

## The three ways to reach a relay

| `tls` | Port (usual) | What happens | Certificate |
|---|---|---|---|
| `starttls` | 587 | plain greeting, `STARTTLS`, then `EHLO` again and the login | chain and **host name verified** |
| `implicit` | 465 ("SSL" in mail clients) | TLS from the first byte, then the greeting | chain and **host name verified** |
| `none` | 25 | no TLS (an internal relay that takes mail from this host); no login is allowed | n/a |

Rules that hold in every mode:

* A configured login **never leaves the host without a verified TLS session**. A relay that does not offer `STARTTLS`,
  refuses it, presents a certificate from an unknown authority or for another host name defers the mail; the password is not
  sent. `tls = "none"` together with a user name is refused when the configuration is read.
* Trust comes from `[outbound] ca_bundle`, or the platform's roots without one. Nothing a remote server says changes it.
* Login mechanisms: `PLAIN` when the relay offers it, otherwise `LOGIN`. A relay that offers neither (only `XOAUTH2` or
  `CRAM-MD5`, say) cannot be used; `relay check` names what it does offer.
* Do not mix up the modes: `implicit` on port 587, or `starttls` on port 465, fails (and `relay check` says it is a TLS problem).

## Check the relay before you send mail

```
rustyctl --config /etc/rusty/rusty.toml relay check
```

It resolves the relay's name, connects, negotiates TLS the way the worker does, logs in and says goodbye. It sends no message
and queues nothing; it opens no database. Exit status 0 when the relay works, 1 when it does not.

```
relay: smtp.example.net:587 (192.0.2.10)
tls: TLSv1_3, certificate verified
login: PLAIN accepted (offered: PLAIN LOGIN)
size limit: 10485760 bytes
no mail was sent
```

A failure is one fixed line, never the relay's own words, the password, the user name or the host:

| Message | Meaning |
|---|---|
| no relay is configured | `[outbound.relay]` is absent |
| the relay password file cannot be used | missing, group/world-readable, or not one line |
| the relay's name does not resolve | DNS (`[outbound] nameserver` when set) |
| cannot connect to the relay | nothing accepted a connection within `connect_timeout_secs` |
| TLS to the relay failed | not offered, refused, handshake failed, certificate or host name does not verify |
| the relay refused the user name or password | the login was rejected |
| the relay offers no login mechanism this server speaks | see "Login mechanisms"; the offered list is printed |
| the relay did not complete the SMTP conversation | no greeting in time, or an invalid or refusing reply |

`--json` prints the same as one JSON object (errors on standard error).

## What the daemon does when the login is wrong

Mail is **deferred, not bounced**: a password that is wrong today is fixed tomorrow, and the sender should not get a bounce
for the operator's mistake. The worker logs a warning for every attempt (`the relay refused the configured credentials`) and
the queue keeps the messages until `max_age_secs`. Run `relay check` after any change of the relay, the password or the
certificate bundle.

## Also needed with a relay

* The relay usually requires the **sending domain to be verified** with the provider, and the envelope sender must belong to it.
* Publish **SPF** with the provider's `include:` and **DKIM** (`rustyctl dkim generate`); Rusty signs before it hands the mail over.
* Not covered by `relay check`: whether the provider accepts a given sender address (that is answered at `MAIL FROM`, which a
  check never sends).

## Not verified

The tests run against loopback fake relays with real TLS (rustls) for STARTTLS, implicit TLS, wrong certificates and host
names, broken handshakes, `PLAIN` and `LOGIN`. They have **not** been run against a real provider (SES, SendGrid, Mailgun):
try `relay check` with your account before relying on it.
