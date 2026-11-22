# Mail footer

A domain's footer text (`footer_text` in the domain's configuration, set in the domain's Settings page by a platform or a domain administrator, up to 2000 characters) is added to the mail the domain sends.

* **Where.** In the outbound worker, in `signed()`, after the stored copy is made ready for sending and **before** DKIM signing, so the signature covers the footer. Only mail whose `From` domain is the sending tenant's gets it; mail a server forwards for a list under another From does not.
* **Which messages.** A single `text/plain` body (the footer follows a `-- ` separator) or a single `text/html` body (a rule and a paragraph before `</body>`, the text escaped). Charset must be UTF-8 or ASCII, the transfer encoding 7bit or 8bit. Multipart messages, attachments, quoted-printable or base64 bodies and other charsets go out unchanged: a footer that could damage a message is worse than none. (Most clients send multipart/alternative; adding to those is the next step.)
* **The Sent copy** the client keeps has no footer: it is the message as the person wrote it.
* **A footer that cannot be read** (database error) is logged and the mail goes without it; it never holds mail back.
