# The certificates page

The edge (HAProxy with ACME) holds the certificates; the admin service cannot read them. What matters to an operator is what a client is **handed**, so the page asks the way a client does: `GET /api/v1/certificates` (platform administrators) connects to the public names, completes a TLS handshake without judging the chain, and reads the leaf certificate: who it was issued to and by, the validity, the days left.

* **Which names.** The host of the admin web's public origin (its port, 443 by default) and, when the mail host is known, the mail host's implicit-TLS ports 993 (IMAP), 995 (POP3) and 465 (submission). They are probed in parallel with a 4 second limit each, so a silent host costs one timeout.
* **A failed probe** shows its reason (`cannot connect`, `TLS failed`, `timed out`) in the row instead of hiding the row.
* **No new dependency.** The four fields come from a small DER reader (`certs::parse_cert`), tested against certificates made by rcgen; the handshake uses rustls with a verifier that accepts what is presented.
* **Under 14 days** the row is a warning, expired or unread is a failure.

Not done: the next renewal time (ACME state lives in the edge), a manual upload for a deployment without ACME (dropped for now, see the ACME decision), and certificates per tenant (they arrive with per-domain ACME at the edge).
