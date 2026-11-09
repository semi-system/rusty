//! Folder commands: SELECT/EXAMINE, CREATE, DELETE, RENAME, SUBSCRIBE, LIST/LSUB, STATUS, CLOSE.
//! Names arrive as modified UTF-7, are resolved through the injected `FolderResolver` against the
//! session's own `Mailbox`, so another account's folder is unreachable by construction.

use crate::command::StatusItem;
use rusty_core::{Error, Flags, FolderId};
use rusty_store::{FolderInfo, MAX_BATCH, MAX_PAGE};

use crate::errors::Op;
use crate::out::{Closed, Out};
use crate::select::Selected;
use crate::session::{Done, Session};
use crate::wire;

/// Longest LIST pattern that is matched (longer ones match nothing).
const MAX_PATTERN_CHARS: usize = 512;

/// `*` matches anything, `%` anything except the delimiter.
// Every index below is bounded by the loop ranges over `n` and `reach` (both `n.len() + 1` long).
#[allow(clippy::indexing_slicing)]
pub(crate) fn wildcard_match(pattern: &str, name: &str, delim: char) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    if p.len() > MAX_PATTERN_CHARS {
        return false;
    }
    // reach[j]: the first i pattern chars can match the first j name chars.
    let mut reach = vec![false; n.len() + 1];
    reach[0] = true;
    for pc in &p {
        let mut next = vec![false; n.len() + 1];
        match pc {
            '*' => {
                let mut any = false;
                for j in 0..=n.len() {
                    any |= reach[j];
                    next[j] = any;
                }
            }
            '%' => {
                let mut any = false;
                for j in 0..=n.len() {
                    any |= reach[j];
                    next[j] = any;
                    if j < n.len() && n[j] == delim {
                        any = false;
                    }
                }
            }
            c => {
                for j in 0..n.len() {
                    if reach[j] && n[j] == *c {
                        next[j + 1] = true;
                    }
                }
            }
        }
        reach = next;
    }
    reach[n.len()]
}

impl Session {
    async fn folders(&self) -> Result<Vec<FolderInfo>, Done> {
        let mb = self.mailbox().ok_or_else(|| Done::bad("Not authenticated"))?;
        mb.list_folders().await.map_err(|e| Done::store(Op::Mailbox, &e))
    }

    fn resolve_name(&self, folders: &[FolderInfo], m: &str) -> Result<Option<FolderId>, Done> {
        Ok(self.resolver.lookup(folders, m))
    }

    pub(crate) async fn cmd_select(&mut self, m: &str, read_only: bool, out: &mut Out<'_>) -> Result<Done, Closed> {
        // A failed SELECT leaves the session with no folder selected (RFC 3501 6.3.1).
        self.sel = None;
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let folders = match self.folders().await {
            Ok(f) => f,
            Err(d) => return Ok(d),
        };
        let id = match self.resolve_name(&folders, m) {
            Ok(Some(id)) => id,
            Ok(None) => return Ok(Done::no("NONEXISTENT", "Mailbox does not exist")),
            Err(d) => return Ok(d),
        };
        let name = m.to_owned();
        let sel = match Selected::load(&mb, id, name, read_only).await {
            Ok(s) => s,
            Err(e) => return Ok(Done::store(Op::Mailbox, &e)),
        };
        out.untagged("FLAGS (\\Seen \\Flagged \\Answered \\Deleted \\Draft)");
        if read_only {
            out.untagged("OK [PERMANENTFLAGS ()] Read-only mailbox");
        } else {
            out.untagged("OK [PERMANENTFLAGS (\\Seen \\Flagged \\Answered \\Deleted \\Draft \\*)] Flags permitted");
        }
        out.untagged(&format!("{} EXISTS", sel.exists()));
        out.untagged("0 RECENT");
        if let Some(n) = sel.first_unseen() {
            out.untagged(&format!("OK [UNSEEN {n}] First unseen message"));
        }
        out.untagged(&format!("OK [UIDVALIDITY {}] UIDs are valid", sel.uid_validity));
        out.untagged(&format!("OK [UIDNEXT {}] Predicted next UID", sel.uid_next));
        self.sel = Some(sel);
        Ok(if read_only {
            Done::ok_code("READ-ONLY", "EXAMINE completed")
        } else {
            Done::ok_code("READ-WRITE", "SELECT completed")
        })
    }

    pub(crate) async fn cmd_create(&mut self, m: &str) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let name = m.to_owned();
        // `CREATE foo/` asks for a hierarchy node only; here it is the same folder.
        let name = name.strip_suffix('/').unwrap_or(&name).to_owned();
        let folders = match self.folders().await {
            Ok(f) => f,
            Err(d) => return Ok(d),
        };
        let plan = match self.resolver.plan_create(&folders, &name) {
            Ok(p) => p,
            Err(_) => return Ok(Done::no("CANNOT", "Invalid mailbox name")),
        };
        if plan.missing.is_empty() {
            return Ok(Done::no("ALREADYEXISTS", "Mailbox already exists"));
        }
        let mut parent = plan.parent;
        for part in &plan.missing {
            match mb.create_folder(parent, part).await {
                Ok(id) => parent = Some(id),
                Err(e) => return Ok(Done::store(Op::Create, &e)),
            }
        }
        Ok(Done::ok("CREATE completed"))
    }

    pub(crate) async fn cmd_delete(&mut self, m: &str) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let folders = match self.folders().await {
            Ok(f) => f,
            Err(d) => return Ok(d),
        };
        let id = match self.resolve_name(&folders, m) {
            Ok(Some(id)) => id,
            Ok(None) => return Ok(Done::no("NONEXISTENT", "Mailbox does not exist")),
            Err(d) => return Ok(d),
        };
        let Some(info) = folders.iter().find(|f| f.id == id) else { return Ok(Done::no("NONEXISTENT", "Mailbox does not exist")) };
        if info.role != "custom" {
            return Ok(Done::no("CANNOT", "System mailboxes cannot be deleted"));
        }
        if folders.iter().any(|f| f.parent == Some(id)) {
            return Ok(Done::no("HASCHILDREN", "Mailbox has subfolders"));
        }
        // RFC 3501: deleting a mailbox removes its messages. The store wants an empty folder, so
        // expunge them first (soft delete; the cleanup job purges them).
        let mut after = 0u32;
        loop {
            let page = match mb.list_folder(id, after, MAX_PAGE).await {
                Ok(p) => p,
                Err(e) => return Ok(Done::store(Op::Mailbox, &e)),
            };
            let Some(last) = page.last() else { break };
            after = last.uid;
            let ids: Vec<_> = page.iter().map(|s| s.id).collect();
            for chunk in ids.chunks(MAX_BATCH) {
                if let Err(e) = mb.expunge_many(chunk).await {
                    return Ok(Done::store(Op::Mailbox, &e));
                }
            }
        }
        if self.sel.as_ref().is_some_and(|s| s.folder == id) {
            self.sel = None;
        }
        match mb.delete_folder(id).await {
            Ok(()) => Ok(Done::ok("DELETE completed")),
            Err(Error::Conflict(_)) => Ok(Done::no("INUSE", "Mailbox is in use, try again")),
            Err(e) => Ok(Done::store(Op::Mailbox, &e)),
        }
    }

    pub(crate) async fn cmd_rename(&mut self, from: &str, to: &str) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let to_name = to.to_owned();
        let folders = match self.folders().await {
            Ok(f) => f,
            Err(d) => return Ok(d),
        };
        let id = match self.resolve_name(&folders, from) {
            Ok(Some(id)) => id,
            Ok(None) => return Ok(Done::no("NONEXISTENT", "Mailbox does not exist")),
            Err(d) => return Ok(d),
        };
        if folders.iter().find(|f| f.id == id).is_some_and(|f| f.role != "custom") {
            return Ok(Done::no("CANNOT", "System mailboxes cannot be renamed"));
        }
        let plan = match self.resolver.plan_create(&folders, &to_name) {
            Ok(p) => p,
            Err(_) => return Ok(Done::no("CANNOT", "Invalid mailbox name")),
        };
        let Some((leaf, intermediates)) = plan.missing.split_last() else {
            return Ok(Done::no("ALREADYEXISTS", "Mailbox already exists"));
        };
        let mut parent = plan.parent;
        for part in intermediates {
            match mb.create_folder(parent, part).await {
                Ok(p) => parent = Some(p),
                Err(e) => return Ok(Done::store(Op::Create, &e)),
            }
        }
        let current = folders.iter().find(|f| f.id == id);
        if current.is_some_and(|f| f.parent != parent) {
            if let Err(e) = mb.move_folder(id, parent).await {
                return Ok(Done::store(Op::Create, &e));
            }
        }
        if current.is_some_and(|f| f.name != *leaf) {
            if let Err(e) = mb.rename_folder(id, leaf).await {
                return Ok(Done::store(Op::Create, &e));
            }
        }
        if let Some(s) = self.sel.as_mut() {
            if s.folder == id {
                s.name = to_name;
            }
        }
        Ok(Done::ok("RENAME completed"))
    }

    pub(crate) async fn cmd_subscribe(&mut self, m: &str, on: bool) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let folders = match self.folders().await {
            Ok(f) => f,
            Err(d) => return Ok(d),
        };
        let id = match self.resolve_name(&folders, m) {
            Ok(Some(id)) => id,
            // Unsubscribing a name that does not exist is harmless (RFC 3501 6.3.7).
            Ok(None) if !on => return Ok(Done::ok("UNSUBSCRIBE completed")),
            Ok(None) => return Ok(Done::no("NONEXISTENT", "Mailbox does not exist")),
            Err(d) => return Ok(d),
        };
        match mb.set_subscribed(id, on).await {
            Ok(()) => Ok(Done::ok(if on { "SUBSCRIBE completed" } else { "UNSUBSCRIBE completed" })),
            Err(e) => Ok(Done::store(Op::Mailbox, &e)),
        }
    }

    pub(crate) async fn cmd_list(
        &mut self,
        reference: &str,
        pattern: &str,
        lsub: bool,
        out: &mut Out<'_>,
    ) -> Result<Done, Closed> {
        let verb = if lsub { "LSUB" } else { "LIST" };
        self.list_matches(verb, reference, &[pattern.to_owned()], lsub, false, out).await
    }

    /// Extended LIST (RFC 5258): several patterns, selection by subscription, `\\Subscribed`.
    pub(crate) async fn cmd_list_ext(
        &mut self,
        reference: &str,
        patterns: &[String],
        subscribed_only: bool,
        return_subscribed: bool,
        out: &mut Out<'_>,
    ) -> Result<Done, Closed> {
        self.list_matches("LIST", reference, patterns, subscribed_only, return_subscribed || subscribed_only, out).await
    }

    async fn list_matches(
        &mut self,
        verb: &str,
        reference: &str,
        patterns: &[String],
        subscribed_only: bool,
        mark_subscribed: bool,
        out: &mut Out<'_>,
    ) -> Result<Done, Closed> {
        let folders = match self.folders().await {
            Ok(f) => f,
            Err(d) => return Ok(d),
        };
        let delim = self.resolver.delimiter();
        let names = self.resolver.names(&folders);
        let mut shown = std::collections::HashSet::new();
        for pattern in patterns {
            // An empty pattern asks for the hierarchy delimiter.
            if pattern.is_empty() {
                out.untagged(&format!("{verb} (\\Noselect) \"{delim}\" \"\""));
                continue;
            }
            // "INBOX" is case-insensitive in the first component.
            let joined = format!("{reference}{pattern}");
            let full = match joined.split_once(delim) {
                Some((h, t)) if h.eq_ignore_ascii_case("INBOX") => format!("INBOX{delim}{t}"),
                None if joined.eq_ignore_ascii_case("INBOX") => "INBOX".to_owned(),
                _ => joined,
            };
            for (f, name) in folders.iter().zip(names.iter()) {
                if (subscribed_only && !f.subscribed) || !wildcard_match(&full, name, delim) || !shown.insert(f.id) {
                    continue;
                }
                let mut attrs: Vec<&str> = Vec::new();
                if mark_subscribed && f.subscribed {
                    attrs.push("\\Subscribed");
                }
                attrs.push(if folders.iter().any(|c| c.parent == Some(f.id)) { "\\HasChildren" } else { "\\HasNoChildren" });
                match f.role.as_str() {
                    "sent" => attrs.push("\\Sent"),
                    "drafts" => attrs.push("\\Drafts"),
                    "trash" => attrs.push("\\Trash"),
                    "spam" => attrs.push("\\Junk"),
                    "archive" => attrs.push("\\Archive"),
                    _ => {}
                }
                let mut line = format!("* {verb} ({}) \"{delim}\" ", attrs.join(" ")).into_bytes();
                wire::mailbox(&mut line, name);
                line.extend_from_slice(b"\r\n");
                out.raw(&line);
                out.flush_if_big().await?;
            }
        }
        Ok(Done::ok(&format!("{verb} completed")))
    }

    pub(crate) async fn cmd_status(
        &mut self,
        m: &str,
        items: &[StatusItem],
        out: &mut Out<'_>,
    ) -> Result<Done, Closed> {
        let Some(mb) = self.mailbox() else { return Ok(Done::bad("Not authenticated")) };
        let folders = match self.folders().await {
            Ok(f) => f,
            Err(d) => return Ok(d),
        };
        let id = match self.resolve_name(&folders, m) {
            Ok(Some(id)) => id,
            Ok(None) => return Ok(Done::no("NONEXISTENT", "Mailbox does not exist")),
            Err(d) => return Ok(d),
        };
        let counts = match mb.folder_counts(id).await {
            Ok(c) => c,
            Err(e) => return Ok(Done::store(Op::Mailbox, &e)),
        };
        let Some(info) = folders.iter().find(|f| f.id == id) else { return Ok(Done::no("NONEXISTENT", "Mailbox does not exist")) };
        let mut parts: Vec<String> = Vec::new();
        for item in items {
            match item {
                StatusItem::Messages => parts.push(format!("MESSAGES {}", counts.messages)),
                StatusItem::Recent => parts.push("RECENT 0".to_owned()),
                StatusItem::UidNext => parts.push(format!("UIDNEXT {}", info.uid_next)),
                StatusItem::UidValidity => parts.push(format!("UIDVALIDITY {}", info.uid_validity)),
                StatusItem::Unseen => parts.push(format!("UNSEEN {}", counts.unseen)),
                // DELETED and DELETED-STORAGE are not offered (no capability advertises them).
                _ => {}
            }
        }
        let name = self.resolver.names(&folders).into_iter().zip(folders.iter()).find(|(_, f)| f.id == id).map(|(n, _)| n).unwrap_or_default();
        let mut line = b"* STATUS ".to_vec();
        wire::mailbox(&mut line, &name);
        line.extend_from_slice(format!(" ({})\r\n", parts.join(" ")).as_bytes());
        out.raw(&line);
        Ok(Done::ok("STATUS completed"))
    }

    pub(crate) async fn cmd_close(&mut self) -> Done {
        if let (Some(mb), Some(sel)) = (self.mailbox(), self.sel.as_ref()) {
            if !sel.read_only {
                let ids: Vec<_> = sel.entries.iter().filter(|e| e.flags.contains(Flags::DELETED)).map(|e| e.id).collect();
                for chunk in ids.chunks(MAX_BATCH) {
                    if let Err(e) = mb.expunge_many(chunk).await {
                        return Done::store(Op::Message, &e);
                    }
                }
            }
        }
        self.sel = None;
        Done::ok("CLOSE completed")
    }
}

#[cfg(test)]
mod tests {
    use super::wildcard_match as m;

    #[test]
    fn wildcards() {
        assert!(m("*", "a/b/c", '/'));
        assert!(m("%", "abc", '/'));
        assert!(!m("%", "a/b", '/'));
        assert!(m("a/%", "a/b", '/'));
        assert!(!m("a/%", "a/b/c", '/'));
        assert!(m("a/*", "a/b/c", '/'));
        assert!(m("INBOX", "INBOX", '/'));
        assert!(!m("INBOX", "INBOX/x", '/'));
        assert!(m("*b*", "abc", '/'));
        assert!(m("", "", '/'));
        assert!(!m("a", "", '/'));
        assert!(m("%/%", "a/b", '/'));
        assert!(!m(&"*".repeat(600), "a", '/'), "overlong patterns match nothing");
    }
}
