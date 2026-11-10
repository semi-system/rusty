//! The adapter to the command grammar library (`imap-codec` 1.0.0). This is the ONLY module that
//! names its types (enforced by scripts/check-wrapped-libs.sh): it turns a framed command into
//! our own [`Command`] and offers one response-validation helper for tests. Replacing the library
//! means rewriting this file; the session layer, the writers and the wire-level conformance tests
//! do not change.

use chrono::Datelike;
use imap_codec::decode::{CommandDecodeError, Decoder};
use imap_codec::imap_types::auth::AuthMechanism;
use imap_codec::imap_types::command::CommandBody;
use imap_codec::imap_types::datetime::NaiveDate;
use imap_codec::imap_types::fetch::{MacroOrMessageDataItemNames, MessageDataItemName, Part, Section as WSection};
use imap_codec::imap_types::flag::{Flag, StoreResponse, StoreType};
use imap_codec::imap_types::mailbox::{ListMailbox, Mailbox};
use imap_codec::imap_types::search::SearchKey as WKey;
use imap_codec::imap_types::sequence::{SeqOrUid, Sequence, SequenceSet};
use imap_codec::imap_types::status::StatusDataItemName;
use imap_codec::{CommandCodec, ResponseCodec};

use crate::command::*;
use crate::search::days_from_civil;
use crate::utf7;

/// Parse a framed command; the error carries nothing about the input.
pub fn parse_command(frame: &[u8]) -> Result<Command, ParseError> {
    // The verbs the library lacks are ours (src/ext.rs).
    if let Some(r) = crate::ext::parse(frame) {
        return r;
    }
    match CommandCodec::new().decode(frame) {
        Ok(([], cmd)) => Ok(Command { tag: cmd.tag.as_ref().to_owned(), body: convert(cmd.body)? }),
        Ok(_) | Err(CommandDecodeError::Incomplete | CommandDecodeError::LiteralFound { .. } | CommandDecodeError::Failed) => {
            Err(ParseError::Syntax)
        }
    }
}

/// Does `bytes` (one complete response line, literals inline) decode with the library's response
/// parser? Used by the property tests to check what the writers produce.
pub fn response_decodes(bytes: &[u8]) -> bool {
    ResponseCodec::new().decode(bytes).is_ok()
}

fn name(m: &Mailbox<'_>) -> Result<String, ParseError> {
    match m {
        Mailbox::Inbox => Ok("INBOX".to_owned()),
        Mailbox::Other(o) => {
            let s = std::str::from_utf8(o.as_ref()).map_err(|_| ParseError::BadName)?;
            utf7::decode(s).ok_or(ParseError::BadName)
        }
    }
}

fn list_text(m: &ListMailbox<'_>) -> Result<String, ParseError> {
    let raw: &[u8] = match m {
        ListMailbox::Token(t) => t.as_ref(),
        ListMailbox::String(s) => s.as_ref(),
    };
    let s = std::str::from_utf8(raw).map_err(|_| ParseError::BadName)?;
    utf7::decode(s).ok_or(ParseError::BadName)
}

fn num(v: SeqOrUid) -> SeqNum {
    match v {
        SeqOrUid::Value(n) => SeqNum::Num(n.get()),
        SeqOrUid::Asterisk => SeqNum::Star,
    }
}

fn seq(set: &SequenceSet) -> SeqSet {
    SeqSet(
        set.0
            .as_ref()
            .iter()
            .map(|s| match s {
                Sequence::Single(a) => SeqRange { lo: num(*a), hi: num(*a) },
                Sequence::Range(a, b) => SeqRange { lo: num(*a), hi: num(*b) },
            })
            .collect(),
    )
}

fn path(p: &Part) -> PartPath {
    p.0.as_ref().iter().map(|n| n.get()).collect()
}

fn section(s: &WSection<'_>) -> Section {
    let names = |n: &imap_codec::imap_types::core::NonEmptyVec<imap_codec::imap_types::core::AString<'_>>| -> Vec<Vec<u8>> {
        n.as_ref().iter().map(|a| a.as_ref().to_vec()).collect()
    };
    match s {
        WSection::Part(p) => Section::Part(path(p)),
        WSection::Header(p) => Section::Header(p.as_ref().map(path)),
        WSection::HeaderFields(p, n) => Section::HeaderFields(p.as_ref().map(path), names(n)),
        WSection::HeaderFieldsNot(p, n) => Section::HeaderFieldsNot(p.as_ref().map(path), names(n)),
        WSection::Text(p) => Section::Text(p.as_ref().map(path)),
        WSection::Mime(p) => Section::Mime(path(p)),
    }
}

fn item(i: &MessageDataItemName<'_>) -> FetchItem {
    match i {
        MessageDataItemName::Flags => FetchItem::Flags,
        MessageDataItemName::Uid => FetchItem::Uid,
        MessageDataItemName::InternalDate => FetchItem::InternalDate,
        MessageDataItemName::Rfc822Size => FetchItem::Rfc822Size,
        MessageDataItemName::Envelope => FetchItem::Envelope,
        MessageDataItemName::Body => FetchItem::Body,
        MessageDataItemName::BodyStructure => FetchItem::BodyStructure,
        MessageDataItemName::Rfc822 => FetchItem::Rfc822,
        MessageDataItemName::Rfc822Header => FetchItem::Rfc822Header,
        MessageDataItemName::Rfc822Text => FetchItem::Rfc822Text,
        MessageDataItemName::BodyExt { section: s, partial, peek } => FetchItem::BodyExt {
            section: s.as_ref().map(section),
            partial: partial.map(|(o, l)| (o, l.get())),
            peek: *peek,
        },
    }
}

fn flag(f: &Flag<'_>) -> FlagArg {
    match f {
        Flag::Seen => FlagArg::Seen,
        Flag::Answered => FlagArg::Answered,
        Flag::Flagged => FlagArg::Flagged,
        Flag::Deleted => FlagArg::Deleted,
        Flag::Draft => FlagArg::Draft,
        Flag::Extension(_) => FlagArg::Ignored,
        Flag::Keyword(k) => FlagArg::Keyword(k.as_ref().to_owned()),
    }
}

fn day(d: &NaiveDate) -> i64 {
    let d: &chrono::NaiveDate = d.as_ref();
    days_from_civil(i64::from(d.year()), i64::from(d.month()), i64::from(d.day()))
}

fn bytes(a: &imap_codec::imap_types::core::AString<'_>) -> Vec<u8> {
    a.as_ref().to_vec()
}

fn key(k: &WKey<'_>) -> SearchKey {
    match k {
        WKey::And(v) => SearchKey::And(v.as_ref().iter().map(key).collect()),
        WKey::SequenceSet(s) => SearchKey::Seq(seq(s)),
        WKey::Uid(s) => SearchKey::Uid(seq(s)),
        WKey::All => SearchKey::All,
        WKey::Answered => SearchKey::Answered,
        WKey::Unanswered => SearchKey::Unanswered,
        WKey::Deleted => SearchKey::Deleted,
        WKey::Undeleted => SearchKey::Undeleted,
        WKey::Draft => SearchKey::Draft,
        WKey::Undraft => SearchKey::Undraft,
        WKey::Flagged => SearchKey::Flagged,
        WKey::Unflagged => SearchKey::Unflagged,
        WKey::Seen => SearchKey::Seen,
        WKey::Unseen => SearchKey::Unseen,
        WKey::New => SearchKey::New,
        WKey::Old => SearchKey::Old,
        WKey::Recent => SearchKey::Recent,
        WKey::Keyword(a) => SearchKey::Keyword(a.as_ref().to_owned()),
        WKey::Unkeyword(a) => SearchKey::Unkeyword(a.as_ref().to_owned()),
        WKey::Larger(n) => SearchKey::Larger(*n),
        WKey::Smaller(n) => SearchKey::Smaller(*n),
        WKey::Before(d) => SearchKey::Before(day(d)),
        WKey::On(d) => SearchKey::On(day(d)),
        WKey::Since(d) => SearchKey::Since(day(d)),
        WKey::SentBefore(d) => SearchKey::SentBefore(day(d)),
        WKey::SentOn(d) => SearchKey::SentOn(day(d)),
        WKey::SentSince(d) => SearchKey::SentSince(day(d)),
        WKey::Bcc(a) => SearchKey::Bcc(bytes(a)),
        WKey::Body(a) => SearchKey::Body(bytes(a)),
        WKey::Cc(a) => SearchKey::Cc(bytes(a)),
        WKey::From(a) => SearchKey::From(bytes(a)),
        WKey::Subject(a) => SearchKey::Subject(bytes(a)),
        WKey::Text(a) => SearchKey::Text(bytes(a)),
        WKey::To(a) => SearchKey::To(bytes(a)),
        WKey::Header(n, v) => SearchKey::Header(bytes(n), bytes(v)),
        WKey::Not(k) => SearchKey::Not(Box::new(key(k))),
        WKey::Or(a, b) => SearchKey::Or(Box::new(key(a)), Box::new(key(b))),
    }
}

fn convert(body: CommandBody<'_>) -> Result<Body, ParseError> {
    use CommandBody as C;
    let s = |a: &imap_codec::imap_types::core::AString<'_>| String::from_utf8_lossy(a.as_ref()).into_owned();
    Ok(match body {
        C::Capability => Body::Capability,
        C::Noop => Body::Noop,
        C::Logout => Body::Logout,
        C::StartTLS => Body::StartTls,
        C::Login { username, password } => Body::Login { user: s(&username), password: s(password.declassify()) },
        C::Authenticate { mechanism, initial_response } => Body::Authenticate {
            mechanism: if mechanism == AuthMechanism::Plain { "PLAIN".to_owned() } else { "OTHER".to_owned() },
            initial: initial_response.map(|i| i.declassify().to_vec()),
        },
        C::Select { mailbox } => Body::Select { name: name(&mailbox)? },
        C::Examine { mailbox } => Body::Examine { name: name(&mailbox)? },
        C::Create { mailbox } => Body::Create { name: name(&mailbox)? },
        C::Delete { mailbox } => Body::Delete { name: name(&mailbox)? },
        C::Rename { from, to } => Body::Rename { from: name(&from)?, to: name(&to)? },
        C::Subscribe { mailbox } => Body::Subscribe { name: name(&mailbox)? },
        C::Unsubscribe { mailbox } => Body::Unsubscribe { name: name(&mailbox)? },
        C::List { reference, mailbox_wildcard } => Body::List { reference: name(&reference)?, pattern: list_text(&mailbox_wildcard)? },
        C::Lsub { reference, mailbox_wildcard } => Body::Lsub { reference: name(&reference)?, pattern: list_text(&mailbox_wildcard)? },
        C::Status { mailbox, item_names } => Body::Status {
            name: name(&mailbox)?,
            items: item_names
                .iter()
                .map(|i| match i {
                    StatusDataItemName::Messages => StatusItem::Messages,
                    StatusDataItemName::Recent => StatusItem::Recent,
                    StatusDataItemName::UidNext => StatusItem::UidNext,
                    StatusDataItemName::UidValidity => StatusItem::UidValidity,
                    StatusDataItemName::Unseen => StatusItem::Unseen,
                    _ => StatusItem::Unsupported,
                })
                .collect(),
        },
        C::Append { mailbox, flags, date, message } => {
            let date = date.map(|d| {
                let c: &chrono::DateTime<chrono::FixedOffset> = d.as_ref();
                c.timestamp()
            });
            Body::Append { name: name(&mailbox)?, flags: flags.iter().map(flag).collect(), date, data: message.as_ref().to_vec() }
        }
        C::Check => Body::Check,
        C::Close => Body::Close,
        C::Unselect => Body::Unselect,
        C::Expunge => Body::Expunge,
        C::Search { charset, criteria, uid } => {
            Body::Search { charset: charset.map(|c| c.as_ref().to_owned()), key: key(&criteria), uid }
        }
        C::Fetch { sequence_set, macro_or_item_names, uid } => {
            let items = match &macro_or_item_names {
                MacroOrMessageDataItemNames::Macro(m) => m.expand().iter().map(item).collect(),
                MacroOrMessageDataItemNames::MessageDataItemNames(v) => v.iter().map(item).collect(),
            };
            Body::Fetch { set: seq(&sequence_set), items, uid }
        }
        C::Store { sequence_set, kind, response, flags, uid } => Body::Store {
            set: seq(&sequence_set),
            kind: match kind {
                StoreType::Replace => StoreKind::Replace,
                StoreType::Add => StoreKind::Add,
                StoreType::Remove => StoreKind::Remove,
            },
            silent: response == StoreResponse::Silent,
            flags: flags.iter().map(flag).collect(),
            uid,
        },
        C::Copy { sequence_set, mailbox, uid } => Body::Copy { set: seq(&sequence_set), name: name(&mailbox)?, uid },
        C::Move { sequence_set, mailbox, uid } => Body::Move { set: seq(&sequence_set), name: name(&mailbox)?, uid },
        C::Idle => Body::Idle,
        C::Enable { .. } => Body::Enable,
        _ => return Err(ParseError::Syntax),
    })
}
