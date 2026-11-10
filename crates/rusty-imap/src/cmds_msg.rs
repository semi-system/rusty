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
    pub(crate) async fn cmd_expunge(&mut self, out: &mut Out<'_>, only_uids: Option<&SeqSet>) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        // Bring the snapshot up to date first: it decides what is flagged Deleted.
        self.send_updates(out, true).await?;
        let Some(sel) = self.sel.as_ref() else { return Ok(Done::bad("No mailbox selected")) };
        if sel.read_only {
            return Ok(Done::no("READ-ONLY", "Mailbox is read-only"));
        }
        // UID EXPUNGE only removes the Deleted messages whose uid is in the set.
        let allowed: Option<std::collections::HashSet<usize>> = only_uids.map(|s| crate::seqset::select_by_uid(s, &sel.uids()).into_iter().collect());
        let doomed: Vec<(usize, MessageId)> = sel
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| e.flags.contains(Flags::DELETED) && allowed.as_ref().is_none_or(|a| a.contains(i)))
            .map(|(i, e)| (i, e.id))
            .collect();
        let ids: Vec<MessageId> = doomed.iter().map(|(_, id)| *id).collect();
        // Conditional in the store: a message another session un-deleted since survives.
        let mut spared: std::collections::HashSet<MessageId> = std::collections::HashSet::new();
        for chunk in ids.chunks(MAX_BATCH) {
            match mb.expunge_deleted_many(chunk).await {
                Ok(o) => spared.extend(o.missing),
                Err(e) => return Ok(Done::store(Op::Message, &e)),
            }
        }
        let indexes: Vec<usize> = doomed.iter().filter(|(_, id)| !spared.contains(id)).map(|(i, _)| *i).collect();
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
        let uid_of: std::collections::HashMap<MessageId, u32> = sel.entries.iter().map(|e| (e.id, e.uid)).collect();
        let (mut src_uids, mut dst_uids): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new());
        for chunk in ids.chunks(MAX_BATCH) {
            let r: Result<Vec<(MessageId, rusty_store::MessageRef)>, Error> = if is_move {
                if same_folder {
                    Ok(Vec::new())
                } else {
                    mb.move_many(chunk, to).await.map(|b| b.moved)
                }
            } else {
                mb.copy_many(chunk, to).await.map(|b| b.copied)
            };
            match r {
                Ok(pairs) => {
                    for (src, new) in pairs {
                        if let Some(u) = uid_of.get(&src) {
                            src_uids.push(*u);
                            dst_uids.push(new.uid);
                        }
                    }
                }
                Err(e) => return Ok(Done::store(Op::Store, &e)),
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
        let uv = folders.iter().find(|f| f.id == to).map_or(0, |f| f.uid_validity);
        if src_uids.is_empty() {
            return Ok(Done::ok(verb));
        }
        Ok(Done::ok_code(&format!("COPYUID {uv} {} {}", uid_set(&src_uids), uid_set(&dst_uids)), verb))
    }

    pub(crate) async fn cmd_append(&mut self, dest: &str, flags: &[FlagArg], date: Option<i64>, data: Vec<u8>) -> Result<Done, Closed> {
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
        let bytes = Bytes::from(data); // moved, not copied
        let stored = match date {
            Some(d) => mb.append_dated(folder, bytes, fs.system, d).await,
            None => mb.append(folder, bytes, fs.system).await,
        };
        let r = match stored {
            Ok(r) => r,
            Err(e) => return Ok(Done::store(Op::Store, &e)),
        };
        for k in &fs.keywords {
            if let Err(e) = mb.add_keyword(r.id, k).await {
                return Ok(Done::store(Op::Store, &e));
            }
        }
        let uv = folders.iter().find(|f| f.id == folder).map_or(0, |f| f.uid_validity);
        Ok(Done::ok_code(&format!("APPENDUID {uv} {}", r.uid), "APPEND completed"))
    }
}

/// `5,7:9`: ascending numbers as a uid set (consecutive runs become ranges), in the given order of
/// first appearance; used by COPYUID where source and destination lists must stay aligned, so
/// only runs that are consecutive in BOTH positions would be mergeable: plain comma lists keep it
/// exact and simple.
fn uid_set(uids: &[u32]) -> String {
    uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}
