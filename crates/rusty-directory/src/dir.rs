use std::fmt;
use std::sync::Arc;

use rusty_core::{AccountId, DomainId, Error, ListId, MailboxId, PlanId, Result};
use rusty_store::repo::sqlite::SqliteRepo;
use rusty_store::repo::{
    AccountInfo, AliasInfo, DirectoryRepo, DomainInfo, DomainStatus, ListMember, NewAccount,
    Resolution, GLOBAL_PLAN,
};
use rusty_store::SqliteStore;

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

/// Facade over a [`DirectoryRepo`]: normalization, validation, password hashing and result
/// shaping live here; every SQL statement lives in `rusty-store`. Cloning is cheap.
#[derive(Clone)]
pub struct Directory {
    repo: Arc<dyn DirectoryRepo>,
}

impl fmt::Debug for Directory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Directory")
    }
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
    /// A directory over the SQLite backend.
    pub fn new(store: SqliteStore) -> Directory {
        Directory {
            repo: Arc::new(SqliteRepo::new(store)),
        }
    }

    /// A directory over any backend's repository (`Store::open(..)?.repos().directory`).
    pub fn with_repo(repo: Arc<dyn DirectoryRepo>) -> Directory {
        Directory { repo }
    }

    pub(crate) fn repo(&self) -> &dyn DirectoryRepo {
        &*self.repo
    }

    pub async fn create_domain(&self, name: &str) -> Result<DomainId> {
        let name = normalize_domain(name)?;
        self.repo.create_domain(&name).await
    }

    pub async fn create_plan(&self, name: &str, config: serde_json::Value) -> Result<PlanId> {
        ensure_object(&config)?;
        let name = name.trim();
        if name == GLOBAL_PLAN {
            return Err(Error::Invalid("reserved plan name".into()));
        }
        self.repo.create_plan(name, &config.to_string()).await
    }

    /// Inserts the account, its `mailboxes` row and the system folders in one transaction
    /// (the repository seeds every counter, see `DirectoryRepo::create_account`).
    pub async fn create_account(
        &self,
        local: &str,
        domain: DomainId,
        password: Option<&str>,
        plan: Option<PlanId>,
    ) -> Result<AccountId> {
        let local = normalize_local(local)?;
        let password_hash = match password {
            Some(p) => Some(hash_password(p).await?),
            None => None,
        };
        self.repo
            .create_account(&NewAccount {
                local,
                domain,
                plan,
                password_hash,
            })
            .await
    }

    pub async fn add_alias(&self, local: &str, domain: DomainId, account: AccountId) -> Result<()> {
        let local = normalize_local(local)?;
        self.repo.add_alias(&local, domain, account).await
    }

    pub async fn create_list(&self, local: &str, domain: DomainId) -> Result<ListId> {
        let local = normalize_local(local)?;
        self.repo.create_list(&local, domain).await
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
        self.repo
            .add_list_member(list, account, external.as_deref(), MAX_LIST_MEMBERS)
            .await
    }

    /// Resolve an address to an account (account, then alias, then list). At most two
    /// statements: the domain and the kind of the name in one, then the members of a list.
    /// A list expands to at most [`MAX_LIST_MEMBERS`] members; a larger one is
    /// `Error::Quota`, never a truncated list. Members come in insertion order (`seq`).
    pub async fn resolve(&self, addr: &str) -> Result<Resolved> {
        let (local, domain) = parse_address(addr)?;
        match self
            .repo
            .resolve(&local, &domain, MAX_LIST_MEMBERS)
            .await?
            .ok_or_else(|| Error::NotFound("address".into()))?
        {
            Resolution::Account(a) => Ok(Resolved::Account(a)),
            Resolution::List(members) => Ok(Resolved::List(
                members
                    .into_iter()
                    .map(|m| match m {
                        ListMember::Account(a) => Resolved::Account(a),
                        ListMember::External(e) => Resolved::External(e),
                    })
                    .collect(),
            )),
        }
    }

    pub async fn mailbox_for(&self, account: AccountId) -> Result<MailboxId> {
        self.repo.mailbox_for(account).await
    }

    // ------------------------------------------------------------ administration (U2)

    /// One page of domains in creation order; `after` is the last id of the previous page.
    /// `limit` is 1..=[`MAX_LIST_PAGE`].
    pub async fn list_domains(&self, after: Option<DomainId>, limit: usize) -> Result<Vec<DomainInfo>> {
        self.repo.list_domains(after, limit).await
    }

    /// Look a domain up by name (normalized like every other entry point).
    pub async fn find_domain(&self, name: &str) -> Result<Option<DomainInfo>> {
        let name = normalize_domain(name)?;
        self.repo.find_domain(&name).await
    }

    /// Disable or re-enable a domain: a disabled domain resolves nothing and its accounts cannot
    /// authenticate; no data is deleted.
    pub async fn set_domain_status(&self, domain: DomainId, status: DomainStatus) -> Result<()> {
        self.repo.set_domain_status(domain, status).await
    }

    /// The account with exactly this address. Unlike `resolve`, an alias or list does not match.
    pub async fn find_account(&self, addr: &str) -> Result<Option<AccountInfo>> {
        let (local, domain) = parse_address(addr)?;
        self.repo.find_account(&local, &domain).await
    }

    pub async fn list_accounts(
        &self,
        domain: DomainId,
        after: Option<AccountId>,
        limit: usize,
    ) -> Result<Vec<AccountInfo>> {
        self.repo.list_accounts(domain, after, limit).await
    }

    pub async fn list_aliases(
        &self,
        domain: DomainId,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AliasInfo>> {
        self.repo.list_aliases(domain, after, limit).await
    }

    /// Remove the alias with this address (`NotFound` if there is none).
    pub async fn remove_alias(&self, addr: &str) -> Result<()> {
        let (local, domain) = parse_address(addr)?;
        self.repo.remove_alias(&local, &domain).await
    }
}
