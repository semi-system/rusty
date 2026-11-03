//! `DirectoryRepo`: domains, plans, accounts, aliases, lists and members, address resolution,
//! configuration layers, authentication hashes and capabilities, at OPERATION granularity.
//!
//! Division of labour (spec Postgres-Valkey 3d):
//! - `rusty-directory::Directory` is the facade. It owns normalization (IDNA, local-part
//!   policy), password hashing (Argon2, off-thread), capability token generation and hashing,
//!   TTL clamping and result shaping. Every value that reaches this trait is already
//!   normalized and validated, and every database error is final (`map_db_error`).
//! - A repository method is ONE unit of work that owns its transaction. No transaction handle
//!   crosses the trait, so the Postgres retry loop (`tx_retry!`) can re-run a whole operation.
//! - Time: the DATABASE clock decides (`{NOW}`: `unixepoch()` / `rs_now()`). The only way to
//!   substitute a time is the explicit `now: Option<i64>` parameter of the capability methods
//!   (the `_at(now)` test twins of the facade); `None` means the database clock. See
//!   `docs/superpowers/specs/2026-11-03-rusty-clock-policy.md`.
//! - Locking: single-statement operations need no explicit lock. Read-then-write operations
//!   lock first: `add_list_member` locks the list row (member count + per-list `seq`),
//!   `issue_capability`, `revoke_all_capabilities`, `set_status` and `set_password_hash` lock
//!   the account row (`FOR NO KEY UPDATE` on Postgres, `BEGIN IMMEDIATE` on SQLite), so the
//!   per-account capability cap and "status change revokes capabilities" cannot interleave.

use async_trait::async_trait;
use rusty_core::{AccountId, DomainId, Error, ListId, MailboxId, PlanId, Result};

/// Input of `create_account`. `local` is already normalized by the caller (normalization lives
/// in `rusty-directory` and stays backend-neutral).
#[derive(Clone)]
pub struct NewAccount {
    pub local: String,
    pub domain: DomainId,
    pub plan: Option<PlanId>,
    pub password_hash: Option<String>,
}

impl std::fmt::Debug for NewAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewAccount")
            .field("local", &self.local)
            .field("domain", &self.domain)
            .field("plan", &self.plan)
            .field("password_hash", &self.password_hash.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListMember {
    Account(AccountId),
    External(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Account(AccountId),
    List(Vec<ListMember>),
}

/// Account lifecycle state (`accounts.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    Locked,
    Disabled,
}

impl AccountStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStatus::Active => "active",
            AccountStatus::Locked => "locked",
            AccountStatus::Disabled => "disabled",
        }
    }
}

/// Which configuration row `set_config` writes. `Global` is the reserved plan row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigTarget {
    Domain(DomainId),
    Account(AccountId),
    Plan(PlanId),
    Global,
}

/// Name of the reserved plan row that holds the global configuration.
pub const GLOBAL_PLAN: &str = "__global__";

/// The four configuration layers of an account as JSON text (`None`: no such row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLayers {
    pub account: String,
    pub plan: Option<String>,
    pub domain: Option<String>,
    pub global: Option<String>,
}

/// What `account_auth` returns: the status and the stored password hash, if any.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthRow {
    pub status: String,
    pub password_hash: Option<String>,
}

impl std::fmt::Debug for AuthRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRow")
            .field("status", &self.status)
            .field("password_hash", &self.password_hash.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A capability row that is currently valid for the requested role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityGrant {
    pub account: AccountId,
    pub domain: DomainId,
    pub role: String,
    pub expires_at: i64,
}

#[async_trait]
pub trait DirectoryRepo: Send + Sync {
    // ------------------------------------------------------------ domains, plans
    /// `Conflict` if the name exists. `name` is the normalized A-label form.
    async fn create_domain(&self, name: &str) -> Result<DomainId>;
    /// `config` is a JSON object as text. `Conflict` if the plan name exists.
    async fn create_plan(&self, name: &str, config: &str) -> Result<PlanId>;

    // ------------------------------------------------------------ accounts
    /// Inserts the account, its mailbox row (counters seeded, uid_validity from the database
    /// clock) and the five system folders in ONE transaction.
    async fn create_account(&self, new: &NewAccount) -> Result<AccountId>;
    /// `NotFound` if the account has no mailbox.
    async fn mailbox_for(&self, account: AccountId) -> Result<MailboxId>;
    /// `NotFound` if there is no such account. Revokes every capability of the account in the
    /// same transaction.
    async fn set_password_hash(&self, account: AccountId, hash: &str) -> Result<()>;
    /// `NotFound` if there is no such account. A status other than `Active` revokes every
    /// capability of the account in the same transaction.
    async fn set_status(&self, account: AccountId, status: AccountStatus) -> Result<()>;
    /// `None`: no such account.
    async fn account_auth(&self, account: AccountId) -> Result<Option<AuthRow>>;

    // ------------------------------------------------------------ aliases, lists
    async fn add_alias(&self, local: &str, domain: DomainId, account: AccountId) -> Result<()>;
    async fn create_list(&self, local: &str, domain: DomainId) -> Result<ListId>;
    /// Exactly one of `account` / `external` is `Some` (checked by the facade; the schema
    /// enforces it too). `Quota` when the list already holds `max_members`. The count and the
    /// insert are one transaction under the list row lock; `seq` is the next dense per-list
    /// number.
    async fn add_list_member(
        &self,
        list: ListId,
        account: Option<AccountId>,
        external: Option<&str>,
        max_members: usize,
    ) -> Result<()>;
    /// Account, then alias, then list. `None`: no such address. `Quota` if a list has more
    /// than `max_members` members (never a truncated list).
    async fn resolve(&self, local: &str, domain: &str, max_members: usize)
        -> Result<Option<Resolution>>;

    // ------------------------------------------------------------ configuration
    /// `config` is a JSON object as text. `NotFound` if the row does not exist (`Global`
    /// creates or replaces its row).
    async fn set_config(&self, target: ConfigTarget, config: &str) -> Result<()>;
    /// `NotFound` if there is no such account.
    async fn config_layers(&self, account: AccountId) -> Result<ConfigLayers>;

    // ------------------------------------------------------------ capabilities
    /// Store a capability (hash only) for an `active` account and revoke the oldest ones
    /// beyond `max_active`. `now = None`: database clock. `NotFound` for a missing or
    /// non-active account.
    async fn issue_capability(
        &self,
        account: AccountId,
        role: &str,
        token_hash: &[u8; 32],
        ttl_secs: i64,
        max_active: i64,
        now: Option<i64>,
    ) -> Result<()>;
    /// `None` for unknown, expired, revoked, wrong-role tokens and non-active accounts.
    async fn validate_capability(
        &self,
        token_hash: &[u8; 32],
        role: &str,
        now: Option<i64>,
    ) -> Result<Option<CapabilityGrant>>;
    /// `true` if the capability was active.
    async fn revoke_capability(&self, token_hash: &[u8; 32], now: Option<i64>) -> Result<bool>;
    /// Number of capabilities revoked.
    async fn revoke_all_capabilities(&self, account: AccountId, now: Option<i64>) -> Result<u64>;
}

/// Statement text shared by the backends (see `repo/mod.rs` for the placeholder and token
/// rules). Only static text is rendered; caller data is always bound.
pub(crate) mod sql {
    pub const INSERT_DOMAIN: &str =
        "INSERT INTO domains (name, created_at) VALUES ($1, {NOW}) RETURNING id";
    pub const INSERT_PLAN: &str =
        "INSERT INTO plans (name, config) VALUES ($1, $2{JSONB}) RETURNING id";
    pub const INSERT_ACCOUNT: &str = "INSERT INTO accounts \
        (local_part, domain_id, plan_id, password_hash, status, created_at) \
        VALUES ($1, $2, $3, $4, 'active', {NOW}) RETURNING id";
    pub const MAILBOX_FOR_ACCOUNT: &str = "SELECT id FROM mailboxes WHERE account_id = $1";
    pub const UPDATE_PASSWORD: &str = "UPDATE accounts SET password_hash = $1 WHERE id = $2";
    pub const UPDATE_STATUS: &str = "UPDATE accounts SET status = $1 WHERE id = $2";
    pub const ACCOUNT_AUTH: &str = "SELECT status, password_hash FROM accounts WHERE id = $1";
    pub const INSERT_ALIAS: &str =
        "INSERT INTO aliases (local_part, domain_id, account_id) VALUES ($1, $2, $3)";
    pub const INSERT_LIST: &str =
        "INSERT INTO lists (local_part, domain_id) VALUES ($1, $2) RETURNING id";
    pub const COUNT_MEMBERS: &str = "SELECT COUNT(*) FROM list_members WHERE list_id = $1";
    /// Resolve step 1: kind 1 account, 2 alias, 3 list. `CAST(.. AS BIGINT)` because a bare
    /// literal is int4 on Postgres and the row decoder reads i64.
    pub const RESOLVE_KIND: &str = "SELECT kind, id FROM ( \
          SELECT CAST(1 AS BIGINT) AS kind, a.id AS id FROM domains d JOIN accounts a \
            ON a.domain_id = d.id WHERE d.name = $1 AND a.local_part = $2 \
          UNION ALL \
          SELECT 2, al.account_id FROM domains d JOIN aliases al \
            ON al.domain_id = d.id WHERE d.name = $1 AND al.local_part = $2 \
          UNION ALL \
          SELECT 3, l.id FROM domains d JOIN lists l \
            ON l.domain_id = d.id WHERE d.name = $1 AND l.local_part = $2 \
        ) AS u ORDER BY kind LIMIT 1";
    pub const LIST_MEMBERS: &str =
        "SELECT account_id, external FROM list_members WHERE list_id = $1 ORDER BY seq LIMIT $2";
    pub const SET_DOMAIN_CONFIG: &str = "UPDATE domains SET config = $1{JSONB} WHERE id = $2";
    pub const SET_ACCOUNT_CONFIG: &str = "UPDATE accounts SET config = $1{JSONB} WHERE id = $2";
    pub const SET_PLAN_CONFIG: &str = "UPDATE plans SET config = $1{JSONB} WHERE id = $2";
    pub const SET_GLOBAL_CONFIG: &str = "INSERT INTO plans (name, config) VALUES ($1, $2{JSONB}) \
        ON CONFLICT (name) DO UPDATE SET config = excluded.config";
    pub const CONFIG_LAYERS: &str = "SELECT CAST(a.config AS TEXT), CAST(p.config AS TEXT), \
        CAST(d.config AS TEXT), (SELECT CAST(g.config AS TEXT) FROM plans g WHERE g.name = $2) \
        FROM accounts a JOIN domains d ON d.id = a.domain_id \
        LEFT JOIN plans p ON p.id = a.plan_id WHERE a.id = $1";
    /// The effective time of a capability statement: the caller's `now` ($N) or the database
    /// clock. Used by the three statements below.
    pub const CAP_VALIDATE: &str = "SELECT c.account_id, c.domain_id, c.role, c.expires_at \
        FROM capabilities c \
        JOIN accounts a ON a.id = c.account_id AND a.domain_id = c.domain_id \
        WHERE c.token_hash = $1 AND c.revoked_at IS NULL \
          AND c.expires_at > COALESCE(CAST($2 AS BIGINT), {NOW}) \
          AND c.role = $3 AND a.status = 'active'";
    pub const CAP_REVOKE_ONE: &str = "UPDATE capabilities SET revoked_at = \
        CASE WHEN COALESCE(CAST($1 AS BIGINT), {NOW}) > issued_at \
             THEN COALESCE(CAST($1 AS BIGINT), {NOW}) ELSE issued_at END \
        WHERE token_hash = $2 AND revoked_at IS NULL \
          AND expires_at > COALESCE(CAST($1 AS BIGINT), {NOW})";
    pub const CAP_REVOKE_ALL: &str = "UPDATE capabilities SET revoked_at = \
        CASE WHEN COALESCE(CAST($1 AS BIGINT), {NOW}) > issued_at \
             THEN COALESCE(CAST($1 AS BIGINT), {NOW}) ELSE issued_at END \
        WHERE account_id = $2 AND revoked_at IS NULL \
          AND expires_at > COALESCE(CAST($1 AS BIGINT), {NOW})";
    pub const CAP_INSERT: &str = "INSERT INTO capabilities \
        (token_hash, account_id, domain_id, role, issued_at, expires_at) \
        SELECT $1, id, domain_id, $2, $3, $4 FROM accounts WHERE id = $5 AND status = 'active'";
    /// The database clock as a scalar.
    pub const NOW: &str = "SELECT {NOW}";
}

/// Turn the rows of `LIST_MEMBERS` into a [`Resolution::List`] (policy shared by every
/// backend: more than `max_members` rows is `Quota`, never a truncated list).
pub(crate) fn list_resolution(
    rows: Vec<(Option<i64>, Option<String>)>,
    max_members: usize,
) -> Result<Resolution> {
    if rows.len() > max_members {
        return Err(Error::Quota("list has too many members".into()));
    }
    Ok(Resolution::List(
        rows.into_iter()
            .filter_map(|(a, e)| match (a, e) {
                (Some(a), _) => Some(ListMember::Account(AccountId(a))),
                (None, Some(e)) => Some(ListMember::External(e)),
                _ => None,
            })
            .collect(),
    ))
}

/// `Quota` when a list is full: `members >= max`.
pub(crate) fn check_list_capacity(members: i64, max_members: usize) -> Result<()> {
    if usize::try_from(members).map_or(true, |m| m >= max_members) {
        return Err(Error::Quota("list has too many members".into()));
    }
    Ok(())
}

/// `expires_at` of a capability issued at `now` (`Invalid` on overflow).
pub(crate) fn capability_expiry(now: i64, ttl_secs: i64) -> Result<i64> {
    now.checked_add(ttl_secs)
        .ok_or_else(|| Error::Invalid("capability time out of range".into()))
}
