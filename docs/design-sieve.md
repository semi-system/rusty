# Sieve filters

Status: **step 1 built** (2026-11-24): `crates/rusty-sieve`, the parser and the interpreter, with no I/O. Nothing else is built.

## What it is for

A person writes rules for their incoming mail in Sieve (RFC 5228): file the list mail into a folder, throw away what a header says is spam, forward some of it, answer with an automatic reply. The mail rules of the admin web
(`design-mail-rules.md`: one forward address, one automatic reply) stay as they are for people who want nothing more; Sieve is for the rest. A person has one active script.

## The engine (built)

* `parse(text) -> Script`: the language of RFC 5228 with the extensions `fileinto`, `reject`, `envelope` and `vacation` (RFC 5230); a script that requires another extension is refused by name. Tests: `true`, `false`, `not`, `allof`,
  `anyof`, `exists`, `size`, `header`, `address`, `envelope`; match types `:is`, `:contains`, `:matches`; address parts `:all`, `:localpart`, `:domain`; comparators `i;ascii-casemap` (default) and `i;octet`. Commands: `if`/`elsif`/`else`,
  `stop`, `keep`, `discard`, `redirect`, `fileinto`, `reject`, `vacation`. Errors carry the line.
* `run(&Script, &Message) -> Outcome`: the actions, in the order of the script, and whether the implicit keep stands. No clock, no database, no network: the same message gives the same answer.
* Limits, all fixed: a script of at most 64 KiB, 1000 commands, blocks nested 16 deep, lists of 100; at run time at most 10 redirects, 20 folders and one automatic reply per message, and the same target twice counts once. `*`/`?` matching is
  iterative (no backtracking explosion). A property test feeds arbitrary text to the parser and runs whatever it accepts.
* Header values are compared as they are written (unfolded); encoded words (`=?utf-8?...?=`) are not decoded yet.

## What is next

1. **Storage**: a table `sieve_scripts (account_id, name, body, active)`, at most 10 scripts of 64 KiB per account, one active (a partial unique index); a repository and the grants (the webmail and the ManageSieve service write, the delivering services read).
2. **Delivery**: in the inbound service next to `apply_rules`, run the active script on the message after it was stored for the account; `fileinto` finds the folder by name (made if missing, never a system folder by a path that escapes it), `redirect` is an outbound
   job like the forward of the mail rules (loop guard `X-Rusty-Forwarded`), `vacation` goes through `vacation_replies` (one reply per sender and the number of days), `reject` becomes a bounce to the envelope sender (never to a null sender), `discard`
   drops the copy. A script that cannot run is logged and the message is kept: a filter never loses mail.
3. **ManageSieve** (RFC 5804) on port 4190: `AUTHENTICATE PLAIN` (account or app password, over TLS), `PUTSCRIPT`/`CHECKSCRIPT` (the parser is the checker), `LISTSCRIPTS`, `GETSCRIPT`, `SETACTIVE`, `DELETESCRIPT`, `HAVESPACE`; a service of its own like the
   others, or a listener of the IMAP service (decided with the owner: one binary fewer).
4. **Webmail**: a script editor in Settings that uses the same `CHECKSCRIPT`.
