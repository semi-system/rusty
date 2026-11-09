//! FETCH, STORE and the unsolicited updates that keep a client's view in step.

use std::collections::{BTreeMap, HashMap};

use imap_codec::imap_types::fetch::{MacroOrMessageDataItemNames, MessageDataItemName, Section};
use imap_codec::imap_types::flag::{Flag, StoreResponse, StoreType};
use imap_codec::imap_types::sequence::SequenceSet;
use rusty_core::{Error, Flags, MessageId};
use rusty_store::{normalize_keyword, MAX_BATCH};

use crate::errors::Op;
use crate::out::{Closed, Out};
use crate::select::Update;
use crate::seqset;
use crate::session::{Done, Session};
use crate::structure::{self, Mime};
use crate::wire;

/// Messages whose keywords are read in one store call while FETCHing.
const KEYWORD_CHUNK: usize = 200;

/// A command's flag arguments split into the system flags and the keywords.
pub(crate) struct FlagSet {
    pub system: Flags,
    pub keywords: Vec<String>,
}

pub(crate) fn split_flags(flags: &[Flag<'_>]) -> Result<FlagSet, ()> {
    let mut system = Flags::empty();
    let mut keywords = Vec::new();
    for f in flags {
        match f {
            Flag::Seen => system |= Flags::SEEN,
            Flag::Answered => system |= Flags::ANSWERED,
            Flag::Flagged => system |= Flags::FLAGGED,
            Flag::Deleted => system |= Flags::DELETED,
            Flag::Draft => system |= Flags::DRAFT,
            // \Recent and unknown system flags cannot be stored; they are ignored.
            Flag::Extension(_) => {}
            Flag::Keyword(k) => {
                let kw = normalize_keyword(k.as_ref()).map_err(|_| ())?;
                if !keywords.contains(&kw) {
                    keywords.push(kw);
                }
            }
        }
    }
    Ok(FlagSet { system, keywords })
}

/// The text of a section specification as it appears inside `BODY[...]`.
fn write_section(out: &mut Vec<u8>, s: &Option<Section<'_>>) {
    let part = |out: &mut Vec<u8>, p: &imap_codec::imap_types::fetch::Part| {
        let nums: Vec<String> = p.0.as_ref().iter().map(|n| n.get().to_string()).collect();
        out.extend_from_slice(nums.join(".").as_bytes());
    };
    let dot = |out: &mut Vec<u8>, p: &Option<imap_codec::imap_types::fetch::Part>| {
        if let Some(p) = p {
            part(out, p);
            out.push(b'.');
        }
    };
    let names = |out: &mut Vec<u8>, n: &imap_codec::imap_types::core::NonEmptyVec<imap_codec::imap_types::core::AString<'_>>| {
        out.extend_from_slice(b" (");
        for (i, a) in n.as_ref().iter().enumerate() {
            if i > 0 {
                out.push(b' ');
            }
            wire::astring(out, a.as_ref());
        }
        out.push(b')');
    };
    match s {
        None => {}
        Some(Section::Part(p)) => part(out, p),
        Some(Section::Header(p)) => {
            dot(out, p);
            out.extend_from_slice(b"HEADER");
        }
        Some(Section::HeaderFields(p, n)) => {
            dot(out, p);
            out.extend_from_slice(b"HEADER.FIELDS");
            names(out, n);
        }
        Some(Section::HeaderFieldsNot(p, n)) => {
            dot(out, p);
            out.extend_from_slice(b"HEADER.FIELDS.NOT");
            names(out, n);
        }
        Some(Section::Text(p)) => {
            dot(out, p);
            out.extend_from_slice(b"TEXT");
        }
        Some(Section::Mime(p)) => {
            part(out, p);
            out.extend_from_slice(b".MIME");
        }
    }
}

impl Session {
    pub(crate) fn pick(&self, set: &SequenceSet, uid: bool) -> Vec<usize> {
        let Some(sel) = &self.sel else { return Vec::new() };
        if uid {
            seqset::select_by_uid(set, &sel.uids())
        } else {
            seqset::select_by_seq(set, sel.entries.len())
        }
    }

    /// Tell the client what changed in the folder since the last command.
    pub(crate) async fn send_updates(&mut self, out: &mut Out<'_>, allow_expunge: bool) -> Result<(), Closed> {
        let (Some(mb), Some(sel)) = (self.mailbox(), self.sel.as_mut()) else { return Ok(()) };
        let updates = match sel.sync(&mb, allow_expunge).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "mailbox sync failed");
                return Ok(());
            }
        };
        if updates.is_empty() {
            return Ok(());
        }
        let flag_ids: Vec<MessageId> = updates
            .iter()
            .filter_map(|u| match u {
                Update::Flags { uid } => sel.index_of_uid(*uid).and_then(|i| sel.entries.get(i)).map(|e| e.id),
                _ => None,
            })
            .collect();
        let mut keywords: HashMap<MessageId, Vec<String>> = HashMap::new();
        for chunk in flag_ids.chunks(MAX_BATCH) {
            if let Ok(m) = mb.keywords_many(chunk).await {
                keywords.extend(m);
            }
        }
        let mut gone = false;
        for u in updates {
            match u {
                Update::Expunge(n) => out.untagged(&format!("{n} EXPUNGE")),
                Update::Exists(n) => out.untagged(&format!("{n} EXISTS")),
                Update::Flags { uid } => {
                    if let Some(i) = sel.index_of_uid(uid) {
                        if let Some(e) = sel.entries.get(i) {
                            let mut b = format!("* {} FETCH (UID {uid} FLAGS ", i + 1).into_bytes();
                            wire::flag_list(&mut b, e.flags, keywords.get(&e.id).map_or(&[][..], Vec::as_slice));
                            b.extend_from_slice(b")\r\n");
                            out.raw(&b);
                        }
                    }
                }
                Update::Gone => {
                    out.untagged("BYE Selected mailbox was deleted");
                    gone = true;
                }
            }
        }
        if gone {
            self.sel = None;
        }
        out.flush_if_big().await
    }

    pub(crate) async fn cmd_fetch(
        &mut self,
        set: &SequenceSet,
        items: &MacroOrMessageDataItemNames<'_>,
        uid_mode: bool,
        out: &mut Out<'_>,
    ) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let mut wanted: Vec<MessageDataItemName<'_>> = match items {
            MacroOrMessageDataItemNames::Macro(m) => m.expand(),
            MacroOrMessageDataItemNames::MessageDataItemNames(v) => v.clone(),
        };
        if uid_mode && !wanted.iter().any(|i| matches!(i, MessageDataItemName::Uid)) {
            wanted.insert(0, MessageDataItemName::Uid);
        }
        let wants_flags = wanted.iter().any(|i| matches!(i, MessageDataItemName::Flags));
        let needs_raw = wanted.iter().any(|i| {
            matches!(
                i,
                MessageDataItemName::Envelope
                    | MessageDataItemName::Body
                    | MessageDataItemName::BodyStructure
                    | MessageDataItemName::BodyExt { .. }
                    | MessageDataItemName::Rfc822
                    | MessageDataItemName::Rfc822Header
                    | MessageDataItemName::Rfc822Text
            )
        });
        let sets_seen = wanted.iter().any(|i| {
            matches!(i, MessageDataItemName::BodyExt { peek: false, .. } | MessageDataItemName::Rfc822 | MessageDataItemName::Rfc822Text)
        });
        let read_only = self.sel.as_ref().is_some_and(|s| s.read_only);
        let picked: Vec<usize> = self.pick(set, uid_mode);
        for chunk in picked.chunks(KEYWORD_CHUNK) {
            // Entries of this chunk (positions are stable: nothing is expunged during a FETCH).
            let entries: Vec<(usize, crate::select::Entry)> = match &self.sel {
                Some(s) => chunk.iter().filter_map(|i| s.entries.get(*i).map(|e| (*i, *e))).collect(),
                None => break,
            };
            let ids: Vec<MessageId> = entries.iter().map(|(_, e)| e.id).collect();
            let keywords = if wants_flags || (sets_seen && !read_only) {
                match mb.keywords_many(&ids).await {
                    Ok(m) => m,
                    Err(e) => return Ok(Done::store(Op::Message, &e)),
                }
            } else {
                HashMap::new()
            };
            for (idx, entry) in entries {
                let raw = if needs_raw {
                    match mb.get_raw(entry.id).await {
                        Ok(b) => Some(b),
                        // Expunged by someone else meanwhile: the next sync tells the client.
                        Err(Error::NotFound(_)) => continue,
                        Err(e) => return Ok(Done::store(Op::Message, &e)),
                    }
                } else {
                    None
                };
                let mut flags = entry.flags;
                let mut flags_changed = false;
                if sets_seen && !read_only && !flags.contains(Flags::SEEN) {
                    let new = flags | Flags::SEEN;
                    match mb.set_flags(entry.id, new).await {
                        Ok(()) => {
                            flags = new;
                            flags_changed = true;
                            if let Some(e) = self.sel.as_mut().and_then(|s| s.entries.get_mut(idx)) {
                                e.flags = new;
                            }
                        }
                        Err(Error::NotFound(_)) => {}
                        Err(e) => return Ok(Done::store(Op::Message, &e)),
                    }
                }
                let kw: &[String] = keywords.get(&entry.id).map_or(&[][..], Vec::as_slice);
                let buf = out.buf();
                buf.extend_from_slice(format!("* {} FETCH (", idx + 1).as_bytes());
                let mime = raw.as_deref().map(structure::parse);
                let mut first = true;
                for item in &wanted {
                    if !first {
                        buf.push(b' ');
                    }
                    first = false;
                    write_item(buf, item, &entry, flags, kw, raw.as_deref(), mime.as_ref());
                }
                if flags_changed && !wants_flags {
                    buf.push(b' ');
                    buf.extend_from_slice(b"FLAGS ");
                    wire::flag_list(buf, flags, kw);
                }
                buf.extend_from_slice(b")\r\n");
                out.flush_if_big().await?;
            }
        }
        Ok(Done::ok(if uid_mode { "UID FETCH completed" } else { "FETCH completed" }))
    }

    pub(crate) async fn cmd_store(
        &mut self,
        set: &SequenceSet,
        kind: StoreType,
        response: StoreResponse,
        flags: &[Flag<'_>],
        uid_mode: bool,
        out: &mut Out<'_>,
    ) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        if self.sel.as_ref().is_some_and(|s| s.read_only) {
            return Ok(Done::no("READ-ONLY", "Mailbox is read-only"));
        }
        let Ok(fs) = split_flags(flags) else { return Ok(Done::no("CANNOT", "Invalid keyword")) };
        let picked = self.pick(set, uid_mode);
        let entries: Vec<(usize, crate::select::Entry)> = match &self.sel {
            Some(s) => picked.iter().filter_map(|i| s.entries.get(*i).map(|e| (*i, *e))).collect(),
            None => return Ok(Done::bad("No mailbox selected")),
        };
        // System flags: group the messages by their resulting flag set, one batch per group.
        let mut groups: BTreeMap<u8, Vec<MessageId>> = BTreeMap::new();
        let mut result: HashMap<MessageId, Flags> = HashMap::new();
        for (_, e) in &entries {
            let new = match kind {
                StoreType::Replace => fs.system,
                StoreType::Add => e.flags | fs.system,
                StoreType::Remove => Flags::from_bits(e.flags.bits() & !fs.system.bits()).unwrap_or(e.flags),
            };
            result.insert(e.id, new);
            if new != e.flags {
                groups.entry(new.bits()).or_default().push(e.id);
            }
        }
        let mut missing: Vec<MessageId> = Vec::new();
        for (bits, ids) in &groups {
            let Some(new) = Flags::from_bits(*bits) else { continue };
            for chunk in ids.chunks(MAX_BATCH) {
                match mb.set_flags_many(chunk, new).await {
                    Ok(o) => missing.extend(o.missing),
                    Err(e) => return Ok(Done::store(Op::Message, &e)),
                }
            }
        }
        // Keywords.
        let ids: Vec<MessageId> = entries.iter().map(|(_, e)| e.id).filter(|id| !missing.contains(id)).collect();
        if kind == StoreType::Replace || !fs.keywords.is_empty() {
            let existing = if kind == StoreType::Replace {
                let mut m: HashMap<MessageId, Vec<String>> = HashMap::new();
                for chunk in ids.chunks(MAX_BATCH) {
                    match mb.keywords_many(chunk).await {
                        Ok(x) => m.extend(x),
                        Err(e) => return Ok(Done::store(Op::Message, &e)),
                    }
                }
                m
            } else {
                HashMap::new()
            };
            for id in &ids {
                let res = self.apply_keywords(&mb, *id, kind, &fs.keywords, existing.get(id)).await;
                match res {
                    Ok(()) | Err(Error::NotFound(_)) => {}
                    Err(e) => return Ok(Done::store(Op::Message, &e)),
                }
            }
        }
        // The snapshot follows what was stored.
        if let Some(s) = self.sel.as_mut() {
            for (idx, e) in &entries {
                if let (Some(slot), Some(f)) = (s.entries.get_mut(*idx), result.get(&e.id)) {
                    if !missing.contains(&e.id) {
                        slot.flags = *f;
                    }
                }
            }
        }
        if response == StoreResponse::Answer {
            let mut keywords: HashMap<MessageId, Vec<String>> = HashMap::new();
            for chunk in ids.chunks(MAX_BATCH) {
                if let Ok(m) = mb.keywords_many(chunk).await {
                    keywords.extend(m);
                }
            }
            for (idx, e) in &entries {
                if missing.contains(&e.id) {
                    continue;
                }
                let flags = result.get(&e.id).copied().unwrap_or(e.flags);
                let buf = out.buf();
                buf.extend_from_slice(format!("* {} FETCH (", idx + 1).as_bytes());
                if uid_mode {
                    buf.extend_from_slice(format!("UID {} ", e.uid).as_bytes());
                }
                buf.extend_from_slice(b"FLAGS ");
                wire::flag_list(buf, flags, keywords.get(&e.id).map_or(&[][..], Vec::as_slice));
                buf.extend_from_slice(b")\r\n");
                out.flush_if_big().await?;
            }
        }
        Ok(Done::ok(if uid_mode { "UID STORE completed" } else { "STORE completed" }))
    }

    async fn apply_keywords(
        &self,
        mb: &rusty_store::Mailbox,
        id: MessageId,
        kind: StoreType,
        wanted: &[String],
        existing: Option<&Vec<String>>,
    ) -> Result<(), Error> {
        match kind {
            StoreType::Add => {
                for k in wanted {
                    mb.add_keyword(id, k).await?;
                }
            }
            StoreType::Remove => {
                for k in wanted {
                    mb.remove_keyword(id, k).await?;
                }
            }
            StoreType::Replace => {
                for k in existing.map(Vec::as_slice).unwrap_or(&[]) {
                    if !wanted.contains(k) {
                        mb.remove_keyword(id, k).await?;
                    }
                }
                for k in wanted {
                    if !existing.is_some_and(|e| e.contains(k)) {
                        mb.add_keyword(id, k).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// One `item value` pair of a FETCH response.
fn write_item(
    out: &mut Vec<u8>,
    item: &MessageDataItemName<'_>,
    entry: &crate::select::Entry,
    flags: Flags,
    keywords: &[String],
    raw: Option<&[u8]>,
    mime: Option<&Mime>,
) {
    let raw = raw.unwrap_or(&[]);
    match item {
        MessageDataItemName::Flags => {
            out.extend_from_slice(b"FLAGS ");
            wire::flag_list(out, flags, keywords);
        }
        MessageDataItemName::Uid => out.extend_from_slice(format!("UID {}", entry.uid).as_bytes()),
        MessageDataItemName::InternalDate => {
            out.extend_from_slice(b"INTERNALDATE \"");
            out.extend_from_slice(wire::date_time(entry.date).as_bytes());
            out.push(b'"');
        }
        MessageDataItemName::Rfc822Size => out.extend_from_slice(format!("RFC822.SIZE {}", entry.size).as_bytes()),
        MessageDataItemName::Envelope => {
            out.extend_from_slice(b"ENVELOPE ");
            if let Some(m) = mime {
                out.extend_from_slice(&m.envelope(raw));
            } else {
                out.extend_from_slice(b"NIL");
            }
        }
        MessageDataItemName::Body => {
            out.extend_from_slice(b"BODY ");
            if let Some(m) = mime {
                out.extend_from_slice(&m.body_structure(raw, false));
            }
        }
        MessageDataItemName::BodyStructure => {
            out.extend_from_slice(b"BODYSTRUCTURE ");
            if let Some(m) = mime {
                out.extend_from_slice(&m.body_structure(raw, true));
            }
        }
        MessageDataItemName::Rfc822 => {
            out.extend_from_slice(b"RFC822 ");
            wire::string(out, raw);
        }
        MessageDataItemName::Rfc822Header => {
            out.extend_from_slice(b"RFC822.HEADER ");
            let part = mime.and_then(|m| m.section(raw, &Section::Header(None)));
            wire::string(out, part.as_deref().unwrap_or(&[]));
        }
        MessageDataItemName::Rfc822Text => {
            out.extend_from_slice(b"RFC822.TEXT ");
            let part = mime.and_then(|m| m.section(raw, &Section::Text(None)));
            wire::string(out, part.as_deref().unwrap_or(&[]));
        }
        MessageDataItemName::BodyExt { section, partial, .. } => {
            out.extend_from_slice(b"BODY[");
            write_section(out, section);
            out.push(b']');
            let data: Option<Vec<u8>> = match section {
                None => Some(raw.to_vec()),
                Some(s) => mime.and_then(|m| m.section(raw, s)),
            };
            match (partial, data) {
                (Some((offset, len)), Some(d)) => {
                    out.extend_from_slice(format!("<{offset}> ").as_bytes());
                    let start = usize::try_from(*offset).unwrap_or(usize::MAX).min(d.len());
                    let end = start.saturating_add(usize::try_from(len.get()).unwrap_or(usize::MAX)).min(d.len());
                    wire::string(out, d.get(start..end).unwrap_or(&[]));
                }
                (None, Some(d)) => {
                    out.push(b' ');
                    wire::string(out, &d);
                }
                (Some((offset, _)), None) => out.extend_from_slice(format!("<{offset}> NIL").as_bytes()),
                (None, None) => out.extend_from_slice(b" NIL"),
            }
        }
    }
}
