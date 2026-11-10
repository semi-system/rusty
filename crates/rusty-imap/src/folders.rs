//! `FolderResolver`: the seam between IMAP mailbox names and the folder tree of a `Mailbox`.
//! The personal namespace implemented here is the only one today (no shared folders, so no
//! cross-account name can resolve); an ACL or shared-folder namespace is another implementation
//! and the session code does not change.

use std::collections::HashMap;

use rusty_core::FolderId;
use rusty_store::{validate_folder_name, FolderInfo, MAX_FOLDER_DEPTH};

/// Where a new name goes: below an existing folder (`None`: top level), after creating the
/// `missing` intermediate folders and finally the leaf (the last entry of `missing`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePlan {
    pub parent: Option<FolderId>,
    pub missing: Vec<String>,
}

/// The name is not a legal mailbox name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadName;

pub trait FolderResolver: Send + Sync {
    /// The hierarchy delimiter of the namespace.
    fn delimiter(&self) -> char;
    /// The full IMAP name (UTF-8) of every folder, in the order of `folders`.
    fn names(&self, folders: &[FolderInfo]) -> Vec<String>;
    /// The folder a (UTF-8) name denotes.
    fn lookup(&self, folders: &[FolderInfo], name: &str) -> Option<FolderId>;
    /// Split a name that does not exist yet into its existing parent and the folders to create.
    fn plan_create(&self, folders: &[FolderInfo], name: &str) -> Result<CreatePlan, BadName>;
}

/// One namespace per account: names are paths of folder names joined with `/`, the inbox is
/// `INBOX` (case-insensitive as the first component).
#[derive(Debug, Default, Clone, Copy)]
pub struct PersonalNamespace;

fn is_inbox(f: &FolderInfo) -> bool {
    f.role == "inbox"
}

impl PersonalNamespace {
    fn child<'a>(folders: &'a [FolderInfo], parent: Option<FolderId>, name: &str, first: bool) -> Option<&'a FolderInfo> {
        folders.iter().find(|f| {
            f.parent == parent
                && if first && parent.is_none() && name.eq_ignore_ascii_case("INBOX") { is_inbox(f) } else { f.name == name && !(is_inbox(f) && first) }
        })
    }
}

impl FolderResolver for PersonalNamespace {
    fn delimiter(&self) -> char {
        '/'
    }

    fn names(&self, folders: &[FolderInfo]) -> Vec<String> {
        let by_id: HashMap<FolderId, &FolderInfo> = folders.iter().map(|f| (f.id, f)).collect();
        folders
            .iter()
            .map(|f| {
                let mut parts: Vec<&str> = Vec::new();
                let mut cur = Some(f);
                let mut depth = 0;
                while let Some(c) = cur {
                    parts.push(if is_inbox(c) { "INBOX" } else { c.name.as_str() });
                    depth += 1;
                    if depth > MAX_FOLDER_DEPTH {
                        break;
                    }
                    cur = c.parent.and_then(|p| by_id.get(&p).copied());
                }
                parts.reverse();
                parts.join("/")
            })
            .collect()
    }

    fn lookup(&self, folders: &[FolderInfo], name: &str) -> Option<FolderId> {
        let mut parent = None;
        let mut found = None;
        for (i, comp) in name.split('/').enumerate() {
            if comp.is_empty() {
                return None;
            }
            let f = Self::child(folders, parent, comp, i == 0)?;
            parent = Some(f.id);
            found = Some(f.id);
        }
        found
    }

    fn plan_create(&self, folders: &[FolderInfo], name: &str) -> Result<CreatePlan, BadName> {
        let comps: Vec<&str> = name.split('/').collect();
        if comps.len() > MAX_FOLDER_DEPTH {
            return Err(BadName);
        }
        for c in &comps {
            validate_folder_name(c).map_err(|_| BadName)?;
        }
        let mut parent = None;
        let mut idx = 0;
        for (i, comp) in comps.iter().enumerate() {
            match Self::child(folders, parent, comp, i == 0) {
                Some(f) => {
                    parent = Some(f.id);
                    idx = i + 1;
                }
                None => break,
            }
        }
        Ok(CreatePlan { parent, missing: comps.iter().skip(idx).map(|s| (*s).to_owned()).collect() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(id: i64, parent: Option<i64>, name: &str, role: &str) -> FolderInfo {
        FolderInfo {
            id: FolderId(id),
            parent: parent.map(FolderId),
            name: name.into(),
            role: role.into(),
            uid_validity: 1,
            uid_next: 1,
            subscribed: true,
        }
    }

    fn tree() -> Vec<FolderInfo> {
        vec![
            f(1, None, "Inbox", "inbox"),
            f(2, None, "Sent", "sent"),
            f(6, None, "Projects", "custom"),
            f(7, Some(6), "2026", "custom"),
            f(8, Some(1), "Work", "custom"),
        ]
    }

    #[test]
    fn names_are_paths_and_the_inbox_is_upper_case() {
        let n = PersonalNamespace.names(&tree());
        assert_eq!(n, ["INBOX", "Sent", "Projects", "Projects/2026", "INBOX/Work"]);
    }

    #[test]
    fn lookup_is_exact_except_for_the_inbox() {
        let t = tree();
        let r = PersonalNamespace;
        assert_eq!(r.lookup(&t, "inbox"), Some(FolderId(1)));
        assert_eq!(r.lookup(&t, "InBoX/Work"), Some(FolderId(8)));
        assert_eq!(r.lookup(&t, "Projects/2026"), Some(FolderId(7)));
        assert_eq!(r.lookup(&t, "projects"), None, "other names are case-sensitive");
        assert_eq!(r.lookup(&t, "Projects/"), None);
        assert_eq!(r.lookup(&t, "/Projects"), None);
        assert_eq!(r.lookup(&t, "Projects//2026"), None);
        assert_eq!(r.lookup(&t, "Inbox2"), None);
        assert_eq!(r.lookup(&t, "2026"), None, "a child is not reachable without its parent");
        assert_eq!(r.lookup(&t, ""), None);
        // A custom top-level folder called "inbox" cannot exist (the inbox is system folder 1),
        // and a child of Projects named like the inbox is just a name.
        let mut t2 = t.clone();
        t2.push(f(9, Some(6), "inbox", "custom"));
        assert_eq!(r.lookup(&t2, "Projects/inbox"), Some(FolderId(9)));
    }

    #[test]
    fn create_plans_find_the_existing_prefix() {
        let t = tree();
        let r = PersonalNamespace;
        assert_eq!(r.plan_create(&t, "New").unwrap(), CreatePlan { parent: None, missing: vec!["New".into()] });
        assert_eq!(
            r.plan_create(&t, "Projects/2027/Q1").unwrap(),
            CreatePlan { parent: Some(FolderId(6)), missing: vec!["2027".into(), "Q1".into()] }
        );
        assert_eq!(r.plan_create(&t, "Projects/2026").unwrap().missing, Vec::<String>::new(), "already there");
        assert_eq!(r.plan_create(&t, "INBOX/Sub").unwrap().parent, Some(FolderId(1)));
        for bad in ["", "a//b", "/a", "a/", "a/..", ".", "a\u{7}b"] {
            assert_eq!(r.plan_create(&t, bad), Err(BadName), "{bad:?}");
        }
        let deep = vec!["d"; 70].join("/");
        assert_eq!(r.plan_create(&t, &deep), Err(BadName));
    }

    #[test]
    fn a_corrupt_cycle_does_not_hang() {
        let t = vec![f(1, Some(2), "a", "custom"), f(2, Some(1), "b", "custom")];
        let n = PersonalNamespace.names(&t);
        assert_eq!(n.len(), 2);
    }
}
