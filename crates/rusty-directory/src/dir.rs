use std::time::{SystemTime, UNIX_EPOCH};

use rusty_core::{AccountId, DomainId, Error, FolderRole, ListId, MailboxId, PlanId, Result};
use rusty_store::{map_db_error, SqliteStore};

use crate::auth::hash_password;
use crate::config::ensure_object;

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
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
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
        let mb: i64 = sqlx::query_scalar(
            "INSERT INTO mailboxes (account_id, uid_validity) VALUES (?, ?) RETURNING id",
        )
        .bind(acc)
        .bind(uv_base + SYSTEM_FOLDERS)
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

    /// Exactly one of `account` / `external` must be given.
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
        sqlx::query("INSERT INTO list_members (list_id, account_id, external) VALUES (?, ?, ?)")
            .bind(list.0)
            .bind(account.map(|a| a.0))
            .bind(external)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    pub async fn resolve(&self, addr: &str) -> Result<Resolved> {
        let (local, domain) = parse_address(addr)?;
        let pool = self.store.pool();
        let not_found = || Error::NotFound("address".into());
        let dom: i64 = sqlx::query_scalar("SELECT id FROM domains WHERE name = ?")
            .bind(&domain)
            .fetch_optional(pool)
            .await
            .map_err(map_db_error)?
            .ok_or_else(not_found)?;
        if let Some(id) = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM accounts WHERE local_part = ? AND domain_id = ?",
        )
        .bind(&local)
        .bind(dom)
        .fetch_optional(pool)
        .await
        .map_err(map_db_error)?
        {
            return Ok(Resolved::Account(AccountId(id)));
        }
        if let Some(id) = sqlx::query_scalar::<_, i64>(
            "SELECT account_id FROM aliases WHERE local_part = ? AND domain_id = ?",
        )
        .bind(&local)
        .bind(dom)
        .fetch_optional(pool)
        .await
        .map_err(map_db_error)?
        {
            return Ok(Resolved::Account(AccountId(id)));
        }
        let list: i64 =
            sqlx::query_scalar("SELECT id FROM lists WHERE local_part = ? AND domain_id = ?")
                .bind(&local)
                .bind(dom)
                .fetch_optional(pool)
                .await
                .map_err(map_db_error)?
                .ok_or_else(not_found)?;
        let rows: Vec<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT account_id, external FROM list_members WHERE list_id = ? ORDER BY rowid",
        )
        .bind(list)
        .fetch_all(pool)
        .await
        .map_err(map_db_error)?;
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
