use std::time::{SystemTime, UNIX_EPOCH};

use rusty_core::{AccountId, DomainId, Error, FolderRole, ListId, MailboxId, PlanId, Result};
use rusty_store::{map_db_error, SqliteStore};

use crate::auth::hash_password;
use crate::config::ensure_object;

/// Most members a mailing list may have, and the most one `resolve` expands.
pub const MAX_LIST_MEMBERS: usize = 10_000;

/// Result of resolving an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Account(AccountId),
    External(String),
    List(Vec<Resolved>),
}

#[derive(Debug, Clone)]
pub struct Directory {
    store: SqliteStore,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Number of system folders created with every mailbox (folder ids 1..=SYSTEM_FOLDERS).
const SYSTEM_FOLDERS: i64 = 5;

/// UIDVALIDITY seed for a new mailbox (spec §4, RFC 3501/9051 §2.3.1.1): the clock in
/// seconds, clamped so that `base + SYSTEM_FOLDERS` still fits in 1..=u32::MAX. System
/// folder `id` gets `base + id`; the mailbox high-water mark gets `base + SYSTEM_FOLDERS`,
/// the largest value issued, so later folders (`Mailbox::create_folder`) are all greater.
fn uid_validity_base() -> i64 {
    now().clamp(1, i64::from(u32::MAX) - SYSTEM_FOLDERS)
}

fn has_bad_chars(s: &str) -> bool {
    s.chars()
        .any(|c| c == '@' || c.is_whitespace() || c.is_control())
}

/// Normalize a domain: trim, strip one trailing dot, IDNA to ASCII (strict, with DNS length
/// verification). The result is the lowercase A-label form.
pub(crate) fn normalize_domain(domain: &str) -> Result<String> {
    let d = domain.trim();
    let d = d.strip_suffix('.').unwrap_or(d);
    if d.is_empty() {
        return Err(Error::Invalid("empty domain".into()));
    }
    if has_bad_chars(d) {
        return Err(Error::Invalid("invalid character in domain".into()));
    }
    let ascii = idna::domain_to_ascii_strict(d)
        .map_err(|_| Error::Invalid("invalid domain".into()))?;
    if ascii.is_empty()
        || ascii.len() > 253
        || ascii.split('.').any(|l| l.is_empty() || l.len() > 63)
    {
        return Err(Error::Invalid("domain label or length out of range".into()));
    }
    Ok(ascii.to_lowercase())
}

/// Which non-ASCII scripts local parts may use. Only Latin today. Other scripts will be
/// opt-in per tenant later and must follow a single-script rule (no mixing within one local
/// part); there is deliberately no per-tenant plumbing yet.
struct LocalPartPolicy {
    latin: bool,
}

const POLICY: LocalPartPolicy = LocalPartPolicy { latin: true };

/// Latin blocks accepted for non-ASCII lowercase letters (covers Vietnamese and western and
/// central European names). Each range is checked together with General_Category Ll.
const LATIN_RANGES: &[std::ops::RangeInclusive<u32>] = &[
    0x00DF..=0x00FF, // Latin-1 lowercase letters (U+00F7 DIVISION SIGN is not Ll)
    0x0100..=0x017F, // Latin Extended-A
    0x0180..=0x024F, // Latin Extended-B
    0x1E00..=0x1EFF, // Latin Extended Additional
];

/// Allow list for one character of an already NFC+lowercased local part (dots are handled
/// separately by the caller). ASCII: a-z, 0-9 and the RFC 5322 atext specials. Non-ASCII:
/// only Ll letters inside [`LATIN_RANGES`] that are NFKC-stable. Everything else (combining marks, other scripts,
/// digits, symbols, emoji, spaces, controls, format characters, private use, unassigned,
/// compatibility forms) is rejected. The Ll test uses `unicode-general-category` because
/// `char::is_lowercase` also accepts Other_Lowercase characters that are not Ll.
pub fn allowed_local_char(c: char) -> bool {
    use unicode_general_category::{get_general_category, GeneralCategory};
    if c.is_ascii() {
        return c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || c == '.'
            || "!#$%&'*+-/=?^_`{|}~".contains(c);
    }
    use unicode_normalization::UnicodeNormalization;
    POLICY.latin
        && LATIN_RANGES.iter().any(|r| r.contains(&(c as u32)))
        && get_general_category(c) == GeneralCategory::LowercaseLetter
        // Compatibility characters (ligatures like U+0133, long s U+017F, digraphs U+01C6, ...)
        // are rejected: a character must be stable under NFKC.
        && std::iter::once(c).nfkc().eq(std::iter::once(c))
}

/// Normalize a local part: NFC, lowercase, NFC; then every character must pass
/// [`allowed_local_char`]. Does not trim (whitespace is invalid). At most 64 octets and 64
/// characters; '.' may not lead, trail or repeat. Quoted forms and comments are not supported.
pub(crate) fn normalize_local(local: &str) -> Result<String> {
    use unicode_normalization::UnicodeNormalization;
    let l: String = local
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .nfc()
        .collect();
    if l.is_empty() {
        return Err(Error::Invalid("empty local part".into()));
    }
    if l.len() > 64 || l.chars().count() > 64 {
        return Err(Error::Invalid("local part longer than 64 octets".into()));
    }
    if !l.chars().all(allowed_local_char) {
        return Err(Error::Invalid("invalid character in local part".into()));
    }
    if l.starts_with('.') || l.ends_with('.') || l.contains("..") {
        return Err(Error::Invalid("misplaced '.' in local part".into()));
    }
    Ok(l)
}

/// Shared normalization for every address-bearing entry point.
pub fn normalize_address(local: &str, domain: &str) -> Result<(String, String)> {
    Ok((normalize_local(local)?, normalize_domain(domain)?))
}

/// Split on the last '@' and normalize both halves.
pub fn parse_address(addr: &str) -> Result<(String, String)> {
    let (l, d) = addr
        .trim()
        .rsplit_once('@')
        .ok_or_else(|| Error::Invalid("address must contain '@'".into()))?;
    normalize_address(l, d)
}

/// Convert a stored (punycode) domain back to Unicode for display.
pub fn display_domain(punycode: &str) -> String {
    idna::domain_to_unicode(punycode).0
}

impl Directory {
    pub fn new(store: SqliteStore) -> Directory {
        Directory { store }
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub async fn create_domain(&self, name: &str) -> Result<DomainId> {
        let name = normalize_domain(name)?;
        let id: i64 =
            sqlx::query_scalar("INSERT INTO domains (name, created_at) VALUES (?, ?) RETURNING id")
                .bind(name)
                .bind(now())
                .fetch_one(self.store.pool())
                .await
                .map_err(map_db_error)?;
        Ok(DomainId(id))
    }

    pub async fn create_plan(&self, name: &str, config: serde_json::Value) -> Result<PlanId> {
        ensure_object(&config)?;
        let name = name.trim();
        if name == crate::config::GLOBAL_PLAN {
            return Err(Error::Invalid("reserved plan name".into()));
        }
        let id: i64 =
            sqlx::query_scalar("INSERT INTO plans (name, config) VALUES (?, ?) RETURNING id")
                .bind(name)
                .bind(config.to_string())
                .fetch_one(self.store.pool())
                .await
                .map_err(map_db_error)?;
        Ok(PlanId(id))
    }

    /// Inserts the account's `mailboxes` row. Every code path that inserts a `mailboxes` row
    /// must seed `next_folder_id` (see `rusty_store::Mailbox`).
    pub async fn create_account(
        &self,
        local: &str,
        domain: DomainId,
        password: Option<&str>,
        plan: Option<PlanId>,
    ) -> Result<AccountId> {
        let local = normalize_local(local)?;
        let hash = match password {
            Some(p) => Some(hash_password(p).await?),
            None => None,
        };
        // BEGIN IMMEDIATE: the write lock is taken up front. Every multi-statement directory
        // write uses `begin_write`, never a deferred `pool.begin()`.
        let mut tx = self.store.begin_write().await?;
        let acc: i64 = sqlx::query_scalar(
            "INSERT INTO accounts (local_part, domain_id, plan_id, password_hash, status, created_at) \
             VALUES (?, ?, ?, ?, 'active', ?) RETURNING id",
        )
        .bind(&local)
        .bind(domain.0)
        .bind(plan.map(|p| p.0))
        .bind(hash)
        .bind(now())
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        let uv_base = uid_validity_base();
        // next_folder_id (migration 0003): custom folders start after the system folders.
        let mb: i64 = sqlx::query_scalar(
            "INSERT INTO mailboxes (account_id, uid_validity, next_folder_id) VALUES (?, ?, ?) \
             RETURNING id",
        )
        .bind(acc)
        .bind(uv_base + SYSTEM_FOLDERS)
        .bind(SYSTEM_FOLDERS + 1)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        let system = [
            (1i64, "Inbox", FolderRole::Inbox),
            (2, "Sent", FolderRole::Sent),
            (3, "Drafts", FolderRole::Drafts),
            (4, "Trash", FolderRole::Trash),
            (5, "Spam", FolderRole::Spam),
        ];
        for (id, name, role) in system {
            sqlx::query(
                "INSERT INTO folders (mailbox_id, id, parent_id, name, role, uid_validity) \
                 VALUES (?, ?, NULL, ?, ?, ?)",
            )
            .bind(mb)
            .bind(id)
            .bind(name)
            .bind(role.as_str())
            .bind(uv_base + id)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(AccountId(acc))
    }

    pub async fn add_alias(&self, local: &str, domain: DomainId, account: AccountId) -> Result<()> {
        let local = normalize_local(local)?;
        sqlx::query("INSERT INTO aliases (local_part, domain_id, account_id) VALUES (?, ?, ?)")
            .bind(local)
            .bind(domain.0)
            .bind(account.0)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    pub async fn create_list(&self, local: &str, domain: DomainId) -> Result<ListId> {
        let local = normalize_local(local)?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO lists (local_part, domain_id) VALUES (?, ?) RETURNING id",
        )
        .bind(local)
        .bind(domain.0)
        .fetch_one(self.store.pool())
        .await
        .map_err(map_db_error)?;
        Ok(ListId(id))
    }

    /// Exactly one of `account` / `external` must be given. A list holds at most
    /// [`MAX_LIST_MEMBERS`] members; adding beyond that is `Error::Quota`.
    pub async fn add_list_member(
        &self,
        list: ListId,
        account: Option<AccountId>,
        external: Option<&str>,
    ) -> Result<()> {
        let external = match (account, external) {
            (Some(_), None) => None,
            (None, Some(e)) => {
                let (l, d) = parse_address(e)?;
                Some(format!("{l}@{d}"))
            }
            _ => {
                return Err(Error::Invalid(
                    "exactly one of account or external address is required".into(),
                ))
            }
        };
        // The count and the insert share one write transaction, so concurrent adds cannot
        // both take the last slot.
        let mut tx = self.store.begin_write().await?;
        let members: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM list_members WHERE list_id = ?")
                .bind(list.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_db_error)?;
        if usize::try_from(members).map_or(true, |m| m >= MAX_LIST_MEMBERS) {
            return Err(Error::Quota("list has too many members".into()));
        }
        sqlx::query("INSERT INTO list_members (list_id, account_id, external) VALUES (?, ?, ?)")
            .bind(list.0)
            .bind(account.map(|a| a.0))
            .bind(external)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(())
    }

    /// Resolve an address to an account (account, then alias, then list). At most two
    /// statements: the domain and the kind of the name in one, then the members of a list.
    /// A list expands to at most [`MAX_LIST_MEMBERS`] members; a larger one is
    /// `Error::Quota`, never a truncated list.
    ///
    /// Portability: plain joins and `UNION ALL` with `ORDER BY ... LIMIT 1`; the member
    /// order uses SQLite's `rowid` (insertion order), which Postgres would replace with a
    /// sequence column.
    pub async fn resolve(&self, addr: &str) -> Result<Resolved> {
        let (local, domain) = parse_address(addr)?;
        let pool = self.store.pool();
        let not_found = || Error::NotFound("address".into());
        let (kind, id): (i64, i64) = sqlx::query_as(
            "SELECT kind, id FROM ( \
               SELECT 1 AS kind, a.id AS id FROM domains d JOIN accounts a \
                 ON a.domain_id = d.id WHERE d.name = ?1 AND a.local_part = ?2 \
               UNION ALL \
               SELECT 2, al.account_id FROM domains d JOIN aliases al \
                 ON al.domain_id = d.id WHERE d.name = ?1 AND al.local_part = ?2 \
               UNION ALL \
               SELECT 3, l.id FROM domains d JOIN lists l \
                 ON l.domain_id = d.id WHERE d.name = ?1 AND l.local_part = ?2 \
             ) ORDER BY kind LIMIT 1",
        )
        .bind(&domain)
        .bind(&local)
        .fetch_optional(pool)
        .await
        .map_err(map_db_error)?
        .ok_or_else(not_found)?;
        if kind != 3 {
            return Ok(Resolved::Account(AccountId(id)));
        }
        let rows: Vec<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT account_id, external FROM list_members WHERE list_id = ? ORDER BY rowid LIMIT ?",
        )
        .bind(id)
        .bind(MAX_LIST_MEMBERS as i64 + 1)
        .fetch_all(pool)
        .await
        .map_err(map_db_error)?;
        if rows.len() > MAX_LIST_MEMBERS {
            return Err(Error::Quota("list has too many members".into()));
        }
        Ok(Resolved::List(
            rows.into_iter()
                .filter_map(|(a, e)| match (a, e) {
                    (Some(a), _) => Some(Resolved::Account(AccountId(a))),
                    (None, Some(e)) => Some(Resolved::External(e)),
                    _ => None,
                })
                .collect(),
        ))
    }

    pub async fn mailbox_for(&self, account: AccountId) -> Result<MailboxId> {
        sqlx::query_scalar::<_, i64>("SELECT id FROM mailboxes WHERE account_id = ?")
            .bind(account.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .map(MailboxId)
            .ok_or_else(|| Error::NotFound("mailbox for account".into()))
    }
}
