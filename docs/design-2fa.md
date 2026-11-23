# Two-factor sign-in for the webmail (TOTP)

Status: **design** (2026-11-24), not built.

## What

A person can ask for a second factor at sign-in to the webmail: a six-digit code from an authenticator app (TOTP, RFC 6238: HMAC-SHA1, 30 seconds, six digits). The mail protocols cannot ask for a code, so an account with the second
factor turned on uses **app passwords** (docs/design-app-passwords.md) in its mail programs: its own password is then refused on IMAP, POP3 and SMTP submission.

## Where the secret lives

The code is checked with the shared secret in the clear, so something must be able to read it. The webmail holds **no** key of the outbound service (docs/design-webmail.md section 3), and must not get one. It gets a key of its
own, used for this and nothing else:

* `[totp] key_file` in the webmail's configuration: an HPKE key pair in the format of `rusty-seal` (`rusty-seal generate`), separate from the sealing key of the outbound service. Without the section the feature is off and the
  Settings page does not offer it.
* The secret is sealed to that key with the account as the context (`kind = "account"`, `purpose = "totp"`), so a stored blob cannot be moved to another account. The database holds only the sealed blob.
* What a compromise of the webmail gives: the second-factor secrets of the people who turned it on (weakening that factor), never DKIM keys, relay passwords or the mail of any other service. That is the reason for the separate key.

## Flow

* `POST /api/v1/totp/begin` (needs the account's password again): makes 20 random bytes, seals and stores them as **unconfirmed**, answers the base32 text and the `otpauth://` URI (the front end draws no QR code from a library; it
  shows the text and the link, and a QR code is a later step).
* `POST /api/v1/totp/confirm` `{code}`: the secret becomes **confirmed** when the person proves they can make a code; from then on the second factor is required. Ten single-use recovery codes are made at this moment, shown once, and
  stored as hashes (Argon2id, like app passwords).
* Sign-in: `POST /api/v1/session` takes an optional `code` (or a `recovery` code). A right password with no or a wrong code answers `401` with the fixed code `second_factor` (only someone who knows the password learns that a code is
  needed); every failure counts against the same throttle as a wrong password. A code is accepted for the current step and one step either side (clock drift), and **each step is accepted once** (`last_step` in the row), so a code
  seen over a shoulder cannot be reused.
* `POST /api/v1/totp/disable` needs the password and a code (or a recovery code).
* A password change by an administrator or a reset ends the sessions as now; it does not turn the factor off (an administrator can: `rustyctl totp-reset ACCOUNT`, audited).

## Tables and roles

* `totp (account_id PK, sealed bytea, confirmed_at, last_step bigint)` and `recovery_codes (account_id, hash)`; written by the webmail role only. The protocol services read `totp.confirmed_at` (a column grant) to refuse the
  account's own password; the admin role may delete a row (the reset).

## Not in this design

Hardware keys (WebAuthn), SMS, push approval, remembering a browser for thirty days. The admin web has its own sign-in and its own 2FA question, later.
