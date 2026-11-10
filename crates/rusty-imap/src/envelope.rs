//! ENVELOPE (RFC 3501 section 7.4.2) from a header block: date, subject, the six address lists,
//! in-reply-to and message-id, written as they appear in the message (encoded words are not
//! decoded, folding is removed).

use crate::address::{self, Addr};
use crate::headers::{self, find};
use crate::wire;

fn write_addresses(out: &mut Vec<u8>, value: Option<&[u8]>) {
    let list = value.map(address::parse).unwrap_or_default();
    if list.is_empty() {
        out.extend_from_slice(b"NIL");
        return;
    }
    out.push(b'(');
    for a in &list {
        match a {
            Addr::Mailbox { name, mailbox, host } => {
                out.push(b'(');
                wire::ntext(out, name.as_deref());
                out.extend_from_slice(b" NIL ");
                wire::text(out, mailbox);
                out.push(b' ');
                wire::ntext(out, host.as_deref());
                out.push(b')');
            }
            Addr::GroupStart(name) => {
                out.extend_from_slice(b"(NIL NIL ");
                wire::text(out, name);
                out.extend_from_slice(b" NIL)");
            }
            Addr::GroupEnd => out.extend_from_slice(b"(NIL NIL NIL NIL)"),
        }
    }
    out.push(b')');
}

/// Append the ENVELOPE of the message whose header block is `header`.
pub fn write(out: &mut Vec<u8>, header: &[u8]) {
    let fields = headers::fields(header);
    let get = |name: &str| find(&fields, name).map(|f| f.value.as_slice());
    out.push(b'(');
    wire::ntext(out, get("date"));
    out.push(b' ');
    wire::ntext(out, get("subject"));
    let from = get("from");
    out.push(b' ');
    write_addresses(out, from);
    out.push(b' ');
    write_addresses(out, get("sender").or(from));
    out.push(b' ');
    write_addresses(out, get("reply-to").or(from));
    for name in ["to", "cc", "bcc"] {
        out.push(b' ');
        write_addresses(out, get(name));
    }
    out.push(b' ');
    wire::ntext(out, get("in-reply-to"));
    out.push(b' ');
    wire::ntext(out, get("message-id"));
    out.push(b')');
}
