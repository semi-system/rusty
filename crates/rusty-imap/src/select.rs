//! The selected folder as the session sees it: a snapshot of `(id, uid, flags, size, date)` per
//! message in uid order (the position in the list is the IMAP sequence number), kept in step
//! with the store through the change log. Memory is summaries only, never message bodies.
//!
//! Reading order matters: the change-log cursor is taken BEFORE the snapshot is read, so a change
//! that lands in between is seen again by the next sync, and applying a change twice is harmless
//! (every change is compared with the snapshot, never blindly applied).

use std::collections::{HashMap, HashSet};

use rusty_core::{Error, Flags, FolderId, MessageId, Result};
use rusty_store::{ChangeItem, ChangeOp, Mailbox, MAX_PAGE};

/// More changed messages than this are not looked at one by one: the folder is read again.
const MAX_INCREMENTAL: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub id: MessageId,
    pub uid: u32,
    pub flags: Flags,
    pub size: u64,
    pub date: i64,
}

/// What a sync found, in the order it must be told to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// `* n EXPUNGE` (the number is valid at that point of the sequence of responses).
    Expunge(u32),
    /// `* n EXISTS`
    Exists(u32),
    /// `* n FETCH (FLAGS ..)`: the message with this uid has new flags.
    Flags { uid: u32 },
    /// The selected folder was deleted.
    Gone,
}

pub struct Selected {
    pub folder: FolderId,
    /// The IMAP name the client selected it by.
    pub name: String,
    pub read_only: bool,
    pub uid_validity: u32,
    pub uid_next: u32,
    pub entries: Vec<Entry>,
    cursor: i64,
    /// Keywords of the messages whose flags the client has been told (non-empty sets only): a
    /// change by someone else is detected by comparing with it.
    kw: HashMap<MessageId, Vec<String>>,
}

fn entry_of(s: &rusty_store::MessageSummary) -> Entry {
    Entry {
        id: s.id,
        uid: s.uid,
        flags: s.flags,
        size: u64::try_from(s.size).unwrap_or(0),
        date: s.internal_date,
    }
}

async fn read_entries(mb: &Mailbox, folder: FolderId) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut after = 0u32;
    loop {
        let page = mb.list_folder(folder, after, MAX_PAGE).await?;
        let Some(last) = page.last() else { break };
        after = last.uid;
        entries.extend(page.iter().map(entry_of));
    }
    Ok(entries)
}

impl Selected {
    pub async fn load(mb: &Mailbox, folder: FolderId, name: String, read_only: bool) -> Result<Selected> {
        let cursor = mb.latest_change_seq().await?;
        let info = mb
            .list_folders()
            .await?
            .into_iter()
            .find(|f| f.id == folder)
            .ok_or_else(|| Error::NotFound("folder".into()))?;
        let entries = read_entries(mb, folder).await?;
        Ok(Selected { folder, name, read_only, uid_validity: info.uid_validity, uid_next: info.uid_next, entries, cursor, kw: HashMap::new() })
    }

    /// The client was just told these keywords for `id` (empty or absent means none).
    pub fn remember_keywords(&mut self, id: MessageId, kws: Option<&Vec<String>>) {
        match kws {
            Some(k) if !k.is_empty() => {
                self.kw.insert(id, k.clone());
            }
            _ => {
                self.kw.remove(&id);
            }
        }
    }

    pub fn exists(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }

    pub fn uids(&self) -> Vec<u32> {
        self.entries.iter().map(|e| e.uid).collect()
    }

    /// 1-based sequence number of the first message without `\Seen`.
    pub fn first_unseen(&self) -> Option<u32> {
        self.entries
            .iter()
            .position(|e| !e.flags.contains(Flags::SEEN))
            .and_then(|i| u32::try_from(i + 1).ok())
    }

    pub fn unseen(&self) -> u32 {
        u32::try_from(self.entries.iter().filter(|e| !e.flags.contains(Flags::SEEN)).count()).unwrap_or(u32::MAX)
    }

    pub fn index_of_uid(&self, uid: u32) -> Option<usize> {
        self.entries.binary_search_by_key(&uid, |e| e.uid).ok()
    }

    /// Remove the entries at `indexes` (ascending) and return the `EXPUNGE` numbers to tell.
    pub fn remove(&mut self, indexes: &[usize]) -> Vec<Update> {
        let mut updates = Vec::with_capacity(indexes.len());
        for (k, i) in indexes.iter().enumerate() {
            updates.push(Update::Expunge(u32::try_from(i + 1 - k).unwrap_or(u32::MAX)));
        }
        let drop: HashSet<usize> = indexes.iter().copied().collect();
        let mut i = 0;
        self.entries.retain(|_| {
            let keep = !drop.contains(&i);
            i += 1;
            keep
        });
        updates
    }

    /// Bring the snapshot up to date with the store. With `allow_expunge == false` (during a
    /// non-UID FETCH/STORE/SEARCH, RFC 3501 section 7.4.1) a sync that would have to report an
    /// expunge changes nothing and leaves everything for the next one.
    pub async fn sync(&mut self, mb: &Mailbox, allow_expunge: bool) -> Result<Vec<Update>> {
        let mut changed: Vec<MessageId> = Vec::new();
        let mut seen: HashSet<MessageId> = HashSet::new();
        let mut last = self.cursor;
        let mut gone = false;
        let mut too_many = false;
        loop {
            let page = mb.changes_since(last, MAX_PAGE).await?;
            for c in &page.changes {
                last = c.seq;
                match c.item {
                    ChangeItem::Message(id) => {
                        if seen.insert(id) {
                            changed.push(id);
                        }
                    }
                    ChangeItem::Folder(f) if f == self.folder && c.op == ChangeOp::Destroy => gone = true,
                    ChangeItem::Folder(_) => {}
                }
            }
            if changed.len() > MAX_INCREMENTAL {
                too_many = true;
                break;
            }
            if !page.more || page.changes.is_empty() {
                break;
            }
        }
        if gone {
            return Ok(vec![Update::Gone]);
        }
        if too_many {
            return self.resync(mb, allow_expunge).await;
        }
        if changed.is_empty() {
            self.cursor = last;
            return Ok(Vec::new());
        }

        let by_id: HashMap<MessageId, usize> = self.entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        let mut removals: Vec<usize> = Vec::new();
        let mut adds: Vec<Entry> = Vec::new();
        let mut flag_changes: Vec<(u32, Flags)> = Vec::new();
        let mut maybe_keywords: Vec<(MessageId, u32, Flags)> = Vec::new();
        for id in changed {
            let now = match mb.get_message(id).await {
                Ok(d) if !d.deleted && d.folder == self.folder => Some(entry_of(&d.summary)),
                Ok(_) | Err(Error::NotFound(_)) => None,
                Err(e) => return Err(e),
            };
            match (by_id.get(&id), now) {
                (Some(i), None) => removals.push(*i),
                (Some(i), Some(n)) => {
                    if let Some(old) = self.entries.get(*i) {
                        if old.flags != n.flags {
                            flag_changes.push((old.uid, n.flags));
                        } else {
                            // Same flag bits: a keyword may still have changed.
                            maybe_keywords.push((old.id, old.uid, n.flags));
                        }
                    }
                }
                (None, Some(n)) => adds.push(n),
                (None, None) => {}
            }
        }
        // Keyword changes: compare the stored keywords of the otherwise unchanged messages with what
        // the client last heard.
        let ids: Vec<MessageId> = maybe_keywords.iter().map(|m| m.0).collect();
        let mut now: HashMap<MessageId, Vec<String>> = HashMap::new();
        for chunk in ids.chunks(rusty_store::MAX_BATCH) {
            now.extend(mb.keywords_many(chunk).await?);
        }
        for (id, uid, flags) in maybe_keywords {
            if now.get(&id) != self.kw.get(&id) {
                flag_changes.push((uid, flags));
            }
        }
        if !allow_expunge && !removals.is_empty() {
            return Ok(Vec::new());
        }
        self.cursor = last;
        removals.sort_unstable();
        let mut updates = self.remove(&removals);
        if !adds.is_empty() {
            adds.sort_by_key(|e| e.uid);
            for a in adds {
                let at = self.entries.partition_point(|e| e.uid < a.uid);
                self.entries.insert(at, a);
            }
            self.uid_next = self.uid_next.max(self.entries.last().map_or(1, |e| e.uid.saturating_add(1)));
            updates.push(Update::Exists(self.exists()));
        }
        flag_changes.sort_unstable_by_key(|c| c.0);
        for (uid, flags) in flag_changes {
            if let Some(i) = self.index_of_uid(uid) {
                if let Some(e) = self.entries.get_mut(i) {
                    e.flags = flags;
                    updates.push(Update::Flags { uid });
                }
            }
        }
        Ok(updates)
    }

    /// Read the folder again and diff against the snapshot (used when too much changed to follow
    /// message by message).
    async fn resync(&mut self, mb: &Mailbox, allow_expunge: bool) -> Result<Vec<Update>> {
        let cursor = mb.latest_change_seq().await?;
        let fresh = read_entries(mb, self.folder).await?;
        let by_id: HashMap<MessageId, &Entry> = fresh.iter().map(|e| (e.id, e)).collect();
        let removals: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !by_id.contains_key(&e.id))
            .map(|(i, _)| i)
            .collect();
        if !allow_expunge && !removals.is_empty() {
            return Ok(Vec::new());
        }
        let mut updates = self.remove(&removals);
        let known: HashSet<MessageId> = self.entries.iter().map(|e| e.id).collect();
        let grew = fresh.iter().any(|e| !known.contains(&e.id));
        let old: HashMap<MessageId, Flags> = self.entries.iter().map(|e| (e.id, e.flags)).collect();
        let changed_uids: Vec<u32> = fresh
            .iter()
            .filter(|e| old.get(&e.id).is_some_and(|f| *f != e.flags))
            .map(|e| e.uid)
            .collect();
        self.entries = fresh;
        self.cursor = cursor;
        if grew {
            self.uid_next = self.uid_next.max(self.entries.last().map_or(1, |e| e.uid.saturating_add(1)));
            updates.push(Update::Exists(self.exists()));
        }
        updates.extend(changed_uids.into_iter().map(|uid| Update::Flags { uid }));
        Ok(updates)
    }
}
