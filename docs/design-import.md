# Making accounts from a CSV

Built 2026-11-22 (P1 item 5 of `design-admin-parity.md`, the import half).

* `POST /api/v1/domains/{d}/accounts/import` with `{ "csv": "...", "dry_run": false }`. One line per account: the **name or the address** (an address must be in this domain), then the **password** (optional: an account with none cannot sign in until an administrator sets one), then the **plan** (optional, by name). Commas separate the fields; a field may be in double quotes (`""` is a quote). A header line (`address,password,plan`) and blank lines are skipped. At most **500 lines** a request (the body is limited to 64 KiB); the web page sends a longer file in requests of 400 lines.
* **Each line is judged alone** with the rules of the single "new account" form: a valid name, a password of at least 8 characters, a plan that exists, an address that is not listed twice in the file. A line that is wrong or already exists is **reported and skipped**; the others are made. The answer lists the wrong lines (and in a dry run what would be made), with the reason in plain words, and counts of made, existing and wrong.
* **Dry run** (`dry_run: true`, the **Check** button) goes through every rule and writes nothing: use it first.
* The first account made in an empty domain gets the role addresses `postmaster` and `abuse`, as in the single form. A plan is applied to each account right after it is made.
* Domain administrators import into their own domain, platform administrators into any. Every account made is announced to the open pages as usual.

## Not done

* Import of aliases, lists and their members, or of mail (IMAP migration is its own item), and export of the accounts as CSV.
* Sending each new user their password (the file holds the passwords in clear text: delete it afterwards, or leave the column out and set them one by one).
