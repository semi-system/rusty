//! Calendars and contacts (CalDAV and CardDAV, docs/design-caldav-carddav.md). This crate holds the layers that need no database and no network, so each can be tested and fuzzed alone:
//!
//! * [`content`]: the text of an iCalendar or a vCard object as a bounded tree of components and properties;
//! * [`time`]: dates, date-times and durations as seconds since the epoch;
//! * [`ical`] and [`vcard`]: what the server reads from an object of a calendar or of an address book (UID, kind, bounds in time, names and addresses).
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

pub mod content;
pub mod fuzz_checks;
pub mod paths;
pub mod server;
pub mod ical;
pub mod time;
pub mod vcard;
pub mod xml;
