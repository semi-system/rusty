//! EXPUNGE, SEARCH, COPY/MOVE and APPEND.

use std::collections::HashMap;

use bytes::Bytes;
use crate::command::{FlagArg, SearchKey, SeqSet};
use rusty_core::{Error, Flags, MessageId};
use rusty_store::MAX_BATCH;

use crate::cmds_fetch::split_flags;
use crate::errors::Op;
use crate::out::{Closed, Out};
use crate::search::{eval, needs_content, needs_keywords, Bounds, Content, Facts, Tri};
use crate::session::{Done, Session};

impl Session {
    pub(crate) async fn cmd_expunge(&mut self, out: &mut Out<'_>) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let Some(sel) = self.sel.as_ref() else { return Ok(Done::bad("No mailbox selected")) };
        if sel.read_only {
            return Ok(Done::no("READ-ONLY", "Mailbox is read-only"));
        }
        let doomed: Vec<(usize, MessageId)> =
            sel.entries.iter().enumerate().filter(|(_, e)| e.flags.contains(Flags::DELETED)).map(|(i, e)| (i, e.id)).collect();
        let ids: Vec<MessageId> = doomed.iter().map(|(_, id)| *id).collect();
        for chunk in ids.chunks(MAX_BATCH) {
            if let Err(e) = mb.expunge_many(chunk).await {
                return Ok(Done::store(Op::Message, &e));
            }
        }
        let indexes: Vec<usize> = doomed.iter().map(|(i, _)| *i).collect();
        if let Some(sel) = self.sel.as_mut() {
            for u in sel.remove(&indexes) {
                if let crate::select::Update::Expunge(n) = u {
                    out.untagged(&format!("{n} EXPUNGE"));
                }
            }
        }
        Ok(Done::ok("EXPUNGE completed"))
    }

    pub(crate) async fn cmd_search(
        &mut self,
        charset: Option<&str>,
        criteria: &SearchKey,
        uid_mode: bool,
        out: &mut Out<'_>,
    ) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        if let Some(cs) = charset {
            let name = cs.to_ascii_uppercase();
            if name != "UTF-8" && name != "US-ASCII" {
                return Ok(Done::no("BADCHARSET (UTF-8 US-ASCII)", "Charset not supported"));
            }
        }
        let Some(sel) = self.sel.as_ref() else { return Ok(Done::bad("No mailbox selected")) };
        let entries = sel.entries.clone();
        let bounds = Bounds { max_seq: u32::try_from(entries.len()).unwrap_or(u32::MAX), max_uid: entries.last().map_or(0, |e| e.uid) };
        // Keywords are read once for the whole folder only if a key asks for them.
        let mut keywords: HashMap<MessageId, Vec<String>> = HashMap::new();
        if needs_keywords(criteria) {
            let ids: Vec<MessageId> = entries.iter().map(|e| e.id).collect();
            for chunk in ids.chunks(MAX_BATCH) {
                match mb.keywords_many(chunk).await {
                    Ok(m) => keywords.extend(m),
                    Err(e) => return Ok(Done::store(Op::Message, &e)),
                }
            }
        }
        let content_needed = needs_content(criteria);
        let (mut scanned_msgs, mut scanned_bytes) = (0usize, 0u64);
        let mut hits: Vec<u32> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            let seq = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let kw: &[String] = keywords.get(&e.id).map_or(&[][..], Vec::as_slice);
            let facts = Facts { seq, uid: e.uid, flags: e.flags, keywords: kw, size: e.size, internal_date: e.date };
            let verdict = match eval(criteria, &facts, bounds, None) {
                Tri::Needs if content_needed => {
                    scanned_msgs += 1;
                    scanned_bytes = scanned_bytes.saturating_add(e.size);
                    if scanned_msgs > self.limits.search_scan_messages || scanned_bytes > self.limits.search_scan_bytes {
                        // Fail closed: never a partial answer.
                        return Ok(Done::no("LIMIT", "Search is too expensive; narrow it down"));
                    }
                    let raw = match mb.get_raw(e.id).await {
                        Ok(b) => b,
                        Err(Error::NotFound(_)) => continue,
                        Err(err) => return Ok(Done::store(Op::Message, &err)),
                    };
                    eval(criteria, &facts, bounds, Some(&Content::new(&raw)))
                }
                v => v,
            };
            if verdict == Tri::Yes {
                hits.push(if uid_mode { e.uid } else { seq });
            }
        }
        let mut line = String::from("* SEARCH");
        for h in &hits {
            line.push(' ');
            line.push_str(&h.to_string());
        }
        line.push_str("\r\n");
        out.raw(line.as_bytes());
        Ok(Done::ok(if uid_mode { "UID SEARCH completed" } else { "SEARCH completed" }))
    }

    pub(crate) async fn cmd_copy(
        &mut self,
        set: &SeqSet,
        dest: &str,
        uid_mode: bool,
        is_move: bool,
        out: &mut Out<'_>,
    ) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let name = dest;
        let folders = match mb.list_folders().await {
            Ok(f) => f,
            Err(e) => return Ok(Done::store(Op::Mailbox, &e)),
        };
        let Some(to) = self.resolver.lookup(&folders, name) else {
            return Ok(Done::no("TRYCREATE", "Mailbox does not exist"));
        };
        let picked = self.pick(set, uid_mode);
        let Some(sel) = self.sel.as_ref() else { return Ok(Done::bad("No mailbox selected")) };
        if is_move && sel.read_only {
            return Ok(Done::no("READ-ONLY", "Mailbox is read-only"));
        }
        let ids: Vec<MessageId> = picked.iter().filter_map(|i| sel.entries.get(*i)).map(|e| e.id).collect();
        let same_folder = sel.folder == to;
        for chunk in ids.chunks(MAX_BATCH) {
            let r = if is_move {
                if same_folder {
                    Ok(())
                } else {
                    mb.move_many(chunk, to).await.map(|_| ())
                }
            } else {
                mb.copy_many(chunk, to).await.map(|_| ())
            };
            if let Err(e) = r {
                return Ok(Done::store(Op::Store, &e));
            }
        }
        if is_move && !same_folder {
            // The moved messages leave the source folder: tell the client now (MOVE is allowed to
            // report expunges even though a plain COPY is not).
            if let Some(sel) = self.sel.as_mut() {
                for u in sel.remove(&picked) {
                    if let crate::select::Update::Expunge(n) = u {
                        out.untagged(&format!("{n} EXPUNGE"));
                    }
                }
            }
        }
        let verb = match (is_move, uid_mode) {
            (true, true) => "UID MOVE completed",
            (true, false) => "MOVE completed",
            (false, true) => "UID COPY completed",
            (false, false) => "COPY completed",
        };
        Ok(Done::ok(verb))
    }

    pub(crate) async fn cmd_append(&mut self, dest: &str, flags: &[FlagArg], data: &[u8]) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let name = dest;
        let Ok(fs) = split_flags(flags) else { return Ok(Done::no("CANNOT", "Invalid keyword")) };
        let folders = match mb.list_folders().await {
            Ok(f) => f,
            Err(e) => return Ok(Done::store(Op::Mailbox, &e)),
        };
        let Some(folder) = self.resolver.lookup(&folders, name) else {
            return Ok(Done::no("TRYCREATE", "Mailbox does not exist"));
        };
        // The internal date the client may give is not honoured yet (the message gets the server
        // time); see the deferred list.
        let r = match mb.append(folder, Bytes::copy_from_slice(data), fs.system).await {
            Ok(r) => r,
            Err(e) => return Ok(Done::store(Op::Store, &e)),
        };
        for k in &fs.keywords {
            if let Err(e) = mb.add_keyword(r.id, k).await {
                return Ok(Done::store(Op::Store, &e));
            }
        }
        Ok(Done::ok("APPEND completed"))
    }
}
