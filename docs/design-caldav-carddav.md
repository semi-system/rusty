# Calendars and contacts (CalDAV and CardDAV)

Status: **steps 1 to 4 built** (2026-11-24): storage (migration 0058), the readers, the server (`crates/rusty-dav`), and the service (`crates/rusty-davd`, role `dav`, image `rusty-dav`, Swarm `DAV=1`, the routes at the HAProxy and Prost edges, the heartbeat, the tombstone collector). Step 5 (the composer of the webmail suggests the contacts of the person: `GET /api/v1/contacts?q=`, read only through the role of the webmail) is built too. The Apple profile (`/mail/config.mobileconfig`) adds the CalDAV and CardDAV accounts when the service has a heartbeat (built 2026-11-24). The sign-in counters of the DAV service are shared in Valkey with VALKEY=1 (built 2026-11-24; the service then takes the default replicas). The webmail exports the contacts as one .vcf (`GET /api/v1/contacts/export`, a link in Settings; built 2026-11-25). The webmail shows an agenda of the next two weeks (`GET /api/v1/events`, read only; `rusty_dav::recur` expands the common repeat rules and says when it cannot; times with a zone are shown as written, UTC ones in the reader's zone; built 2026-11-25). A single event can be added from the webmail (`POST /api/v1/events`, a form in the agenda; built 2026-11-25). Open: changing and removing events there (it needs DELETE on the objects). (vCard import is built, 2026-11-25: `POST /api/v1/contacts/import` writes through `DavRepo::put` with narrow grants for the webmail role: INSERT/UPDATE on `dav_objects`, UPDATE of `revision` and `updated_at` on `dav_collections`, SELECT/DELETE on `dav_tombstones`; it never creates an address book, that stays the DAV service's), the Autodiscover entries of Outlook (it has no CalDAV), scheduling and sharing (v2). Decision D1 of `design-admin-parity.md`: groupware is built as a service of its own. This page is that design.

## What it is for

A person keeps a calendar and an address book on the server and reaches them from the phone and the desktop (iOS and macOS Calendar and Contacts, Android with DAVx5, Thunderbird, Evolution, GNOME). The protocols are CalDAV
(RFC 4791) for calendars and CardDAV (RFC 6352) for contacts, both on top of WebDAV (RFC 4918). The webmail gets contact completion later from the same tables.

Not in v1: scheduling (iTIP: invitations by mail, free/busy, RFC 6638), sharing a calendar with another person (it follows the folder-sharing table of `design-folder-sharing.md`, which already reserves a `kind`), resources
(rooms, equipment), tasks (VTODO is stored and returned, but there is no task UI), push (APNs/WebDAV-Push), attachments by reference, a calendar or contacts UI in the webmail.

## The service

* **One role, `dav`**, one binary entry (`rusty --role dav`), one image (`rusty-dav`), one Swarm service like the webmail: HTTP behind the edge, PROXY protocol v2 for the client address, its own Postgres role `rusty_dav`
  with only the tables below and the directory lookups it needs, no blob store, no sealed secrets.
* **The crate `rusty-dav`** has three layers kept apart so each is tested and fuzzed alone: `xml` (the request bodies of WebDAV: PROPFIND, PROPPATCH, REPORT; a strict reader with limits on size, depth and names, built on a
  maintained XML library, never entities or external references), `ical` and `vcard` (a *light* reader of the two text formats: it finds the component, `UID`, `DTSTART`, `DTEND`, `DURATION`, `RRULE`, `RECURRENCE-ID`, `FN`, `EMAIL`,
  `TEL` and checks the structure; it does not edit anything: the object is stored and returned byte for byte), and `server` (the methods and reports over a repository trait).
* **Where clients find it.** The edge routes `/dav/` and `/.well-known/caldav` and `/.well-known/carddav` (redirects to `/dav/`) of the mail host to the service: no new name, no new certificate. The autoconfig and
  Autodiscover answers of the admin service gain the CalDAV and CardDAV entries when the service runs. DNS SRV and TXT records (`_caldavs._tcp`, `_carddavs._tcp`) are added to the DNS check and to the hosted zones.
* **Authentication.** HTTP Basic inside TLS only, with the own password of the account or an app password (`design-app-passwords.md`), through the same check, throttle and plan rules as IMAP (the plan must allow `imap`; a
  protocol key `dav` of its own would need a migration and is left out). An account with the second factor on signs in with an app password. A wrong password is `401` with `WWW-Authenticate: Basic realm="Rusty"`.

## Paths

```
/dav/                                        the root: current-user-principal -> /dav/principals/{account}/
/dav/principals/{account}/                   the principal: calendar-home-set, addressbook-home-set, displayname
/dav/calendars/{account}/                    the calendar home (Depth 1: its calendars)
/dav/calendars/{account}/{name}/             a calendar collection
/dav/calendars/{account}/{name}/{object}.ics an event
/dav/addressbooks/{account}/                 the address book home
/dav/addressbooks/{account}/{name}/          an address book
/dav/addressbooks/{account}/{name}/{object}.vcf a contact
```

`{account}` is the address of the account (percent-encoded); a person reaches their own tree only (`403` elsewhere, until sharing). A new account has one calendar (`default`) and one address book (`default`), made at the first
request. `{name}` and `{object}` are 1 to 128 characters of `A-Za-z0-9._-` (anything else is refused, which also closes the path-escape questions).

## Storage (migration 0058, role `rusty_dav`)

* `dav_collections (id, account_id, kind 'calendar'|'addressbook', name, display_name, description, color, components text[], revision bigint, created_at, updated_at)`, unique `(account_id, kind, name)`. `revision` is the
  collection's counter: every change (put, delete) adds one; it is the `getctag` and the base of the sync token.
* `dav_objects (collection_id, name, uid, etag, body text, size, component, starts_at, ends_at, recurring boolean, revision, updated_at)`, primary key `(collection_id, name)`, unique `(collection_id, uid)`. `starts_at` and
  `ends_at` are the outer bounds of the event (the last one is the end of the last occurrence when the rule has an end, and null when it has none) and are what `time-range` filters on. `body` is the object as uploaded.
* `dav_tombstones (collection_id, name, revision)`: what was deleted at which revision, for `sync-collection`; the garbage collector drops the ones older than 30 days (a client whose token is older gets `valid-sync-token`
  failing and does a full sync).
* Limits, checked in the repository and in the service: 1 MiB per object, 10 000 objects per collection, 50 collections per account, 100 000 objects per account. The write is one transaction that checks the quota, writes the object and
  adds the revision, so two clients never leave a collection half changed.

## The protocol (what v1 answers)

* **WebDAV:** `OPTIONS` (the `DAV:` header: `1, calendar-access, addressbook`), `PROPFIND` (Depth 0 and 1; `Depth: infinity` is refused with `403` and the `propfind-finite-depth` precondition), `PROPPATCH` (display name, description
  and color of a collection only), `GET`/`HEAD` (an object, with its `ETag`), `PUT` (`If-Match`, `If-None-Match: *`; `412` on a mismatch; `409` and `no-uid-conflict` for a second object with the same UID), `DELETE` (`If-Match`),
  `MKCALENDAR` and `MKCOL` with a `resourcetype` of an address book, `COPY` and `MOVE` of an object within the account.
* **CalDAV reports:** `calendar-query` (component filter, `time-range`, `prop-filter` and `text-match` on UID, SUMMARY and the like), `calendar-multiget`, `free-busy-query` is not answered (scheduling is not in v1). A
  recurring event is matched by its outer bounds (a superset: the client filters the occurrences, as the clients do anyway); an event with a rule that has no end matches every range that starts after its first occurrence.
* **CardDAV reports:** `addressbook-query` (`prop-filter` and `text-match` on FN, EMAIL, TEL, ORG; `limit`), `addressbook-multiget`.
* **Both:** `sync-collection` (RFC 6578) with a sync token `{collection-id}:{revision}`; `expand-property` is not answered; `ACL` methods are not (the properties `current-user-privilege-set` and `owner` are, read-only).
* **Properties:** `resourcetype`, `displayname`, `getetag`, `getcontenttype`, `getcontentlength`, `getlastmodified`, `creationdate`, `getctag` (the Apple one), `sync-token`, `supported-report-set`,
  `supported-calendar-component-set`, `supported-calendar-data`, `supported-address-data`, `max-resource-size`, `calendar-description`, `calendar-color` (Apple), `current-user-principal`, `principal-URL`,
  `calendar-home-set`, `addressbook-home-set`. Another property asked for is answered `404 Not Found` in its own `propstat`, never an error for the whole request.
* **Validity:** a `PUT` to a calendar must be one `VCALENDAR` of version 2.0 with one UID and components of one kind (the rest is the client's business); to an address book, one `VCARD` of version 3.0 or 4.0 with an `FN`. Anything
  else is `403` with the `valid-calendar-object-resource` or `valid-address-data` precondition. Lines are unfolded with a bound, and a body that is not UTF-8 is refused.

## Safety

* Every request has a body limit (1 MiB; 64 KiB for XML), a time limit and a limit of reports per second per account; a report returns at most 1000 objects, and `sync-collection` pages with the standard
  `507` truncation answer when there are more.
* The XML reader accepts no DTD, no entity and no namespace it does not know; an element deeper than 16 levels is refused. The text of an object is never put into a header or a log line.
* An object is data: it is stored and returned as bytes with `Content-Type: text/calendar; charset=utf-8` (or `text/vcard`), `X-Content-Type-Options: nosniff` and `Content-Security-Policy: sandbox`, so a browser that opens it
  runs nothing.
* The service writes an audit line for a change (account, collection, object name, size), not for a read.

## Steps

1. **Storage:** migration 0058, the repository trait and its Postgres implementation, the grants of `rusty_dav` (the matrix test), the quotas, the tombstone collection by the garbage collector.
2. **The formats:** `ical` and `vcard` readers with the outer-bounds computation (recurrence rules of `FREQ`, `INTERVAL`, `COUNT`, `UNTIL`, `BYDAY`), property and fuzz tests (a fuzz target for each reader and for the XML reader).
3. **The server:** methods, properties and reports over the repository trait, tested with recorded request and response pairs of real clients (iOS, DAVx5, Thunderbird) and with a litmus-style suite for the WebDAV rules.
4. **The service:** role, config `[dav]`, binary entry, image stage, Swarm service (replicas like the webmail), edge routes for HAProxy and Prost, well-known redirects, autoconfig and Autodiscover entries, SRV records, the admin
   Monitor row, the metrics.
5. **Webmail:** contact completion in the composer from the address books (`GET /api/v1/contacts?q=`), and import and export of vCard; a calendar view later.

Decisions taken here and open to the owner: scheduling (invitations by mail) and sharing are v2; push is not planned; the protocol key `dav` stays out of the Protocols page unless the owner wants a switch.
