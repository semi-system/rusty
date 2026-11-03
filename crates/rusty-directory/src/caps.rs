//! Hashed, revocable, short-lived capability tokens (distributed-model spec §10).
//!
//! A capability is a 256-bit random bearer token (`OsRng`, 64 lowercase hex characters) that
//! a front-end service presents to a worker instead of a password. Properties:
//!
//! - **Only a hash is stored.** The `capabilities` table keeps `SHA-256(tag || token_bytes)`
//!   (`tag` = `rusty.capability.v1`, domain separation) as its primary key. The token itself
//!   is returned once, by [`Directory::issue_capability`], and is never written to the
//!   database, a log or an error message. A database leak therefore yields no usable token.
//! - **No hand-written comparison.** A presented token is hashed and looked up by the
//!   primary key; the database index lookup *is* the comparison. This module contains no
//!   byte-by-byte equality on secrets, and the token is high-entropy random, so lookup timing
//!   reveals nothing useful about other tokens. A malformed token takes the same shape (the
//!   same query with a constant dummy hash) and yields `Ok(None)` like an unknown one.
//! - **Short-lived and revocable.** `ttl` is clamped to `1..=86400` seconds (the schema also
//!   enforces it). At most [`MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT`] are active per account;
//!   issuing one more revokes the oldest. [`Directory::set_password`] and
//!   [`Directory::set_status`] (to a non-active status) revoke every capability of the
//!   account in the same transaction; deleting the account cascades.
//! - **Tenant-bound.** The row carries `(account_id, domain_id)` under a composite foreign
//!   key to `accounts(id, domain_id)`, so the domain is always the account's own. Validation
//!   returns both, additionally requires the account to be `active`, and knows nothing about
//!   mailboxes: there is no foreign key or join into any mailbox's data.
//! - **No enumeration surface.** Malformed, unknown, expired, revoked, wrong-role and
//!   non-active-account tokens all return `Ok(None)`. Errors are fixed strings that never echo
//!   the token or any account data. The surrogate `rowid` is internal and never exposed.
//!
//! Limitation (honest): there is no `zeroize` dependency, and Rust gives no guarantee that
//! copies made by moves, the allocator or the SHA-256 implementation's internal state are
//! erased. This module overwrites the byte arrays and the hex buffer it controls with safe
//! code (`fill(0)`) as soon as the hash has been computed, which narrows but does not close
//! the window in which a token lives in process memory. [`Capability`] overwrites its own
//! buffer on drop; the `&str` handed out by [`Capability::expose_secret`] and any copy the
//! caller makes are the caller's responsibility. Compilers may elide dead stores.
//!
//! Timing: a *holder of a real token* can tell an unknown token from a known-but-dead one
//! (expired, revoked, wrong role, non-active account) by timing, because the dead row is found
//! and then filtered. This is accepted: the holder already has the token, and a caller without
//! one cannot tell a malformed token from an unknown one.
//!
//! Times come from the database clock (`unixepoch()`); the `*_at` twins take an explicit
//! `now` for tests.

use rand_core::{OsRng, RngCore};
use rusty_core::{AccountId, DomainId, Error, Result};
use rusty_store::{map_db_error, SqliteStore};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;

use crate::dir::Directory;

/// Domain-separation tag hashed in front of the token bytes.
const HASH_TAG: &[u8] = b"rusty.capability.v1";
/// Token size in bytes (256 bits).
const TOKEN_BYTES: usize = 32;
/// Token size in hex characters.
const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;
/// Longest lifetime of one capability, in seconds (also a schema CHECK).
pub const MAX_CAPABILITY_TTL_SECS: u32 = 86_400;
/// Active (unrevoked, unexpired) capabilities per account; the oldest are revoked beyond it.
pub const MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT: i64 = 64;
/// Hash used for the lookup of a malformed token, so its path has the same shape as any other.
const DUMMY_HASH: [u8; 32] = [0u8; 32];

/// What a capability may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityRole {
    Imap,
    Jmap,
    Dav,
    SmtpSubmit,
}

impl CapabilityRole {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityRole::Imap => "imap",
            CapabilityRole::Jmap => "jmap",
            CapabilityRole::Dav => "dav",
            CapabilityRole::SmtpSubmit => "smtp-submit",
        }
    }
}

/// A freshly issued bearer token. Deliberately has no `Display`, `Serialize`, `Clone` or
/// `PartialEq`, and `Debug` prints a fixed placeholder. Its buffer is overwritten on drop.
pub struct Capability(String);

impl Capability {
    /// The token, for handing to the client exactly once. Treat as a secret.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capability(<redacted>)")
    }
}

impl Drop for Capability {
    fn drop(&mut self) {
        wipe_string(&mut self.0);
    }
}

/// What a valid capability grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityInfo {
    pub account: AccountId,
    pub domain: DomainId,
    pub role: CapabilityRole,
    /// Unix seconds; the capability is valid while `now < expires_at`.
    pub expires_at: i64,
}

/// Overwrite a string's bytes with zeros (safe code: take, convert to bytes, fill, drop).
fn wipe_string(s: &mut String) {
    let mut bytes = std::mem::take(s).into_bytes();
    bytes.fill(0);
    std::hint::black_box(&bytes);
}

fn hash_token(raw: &[u8; TOKEN_BYTES]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(HASH_TAG);
    h.update(raw);
    h.finalize().into()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Decode a 64-character lowercase-hex token into `out`. `false` for anything else.
fn decode_token(token: &str, out: &mut [u8; TOKEN_BYTES]) -> bool {
    let b = token.as_bytes();
    if b.len() != TOKEN_HEX_LEN {
        return false;
    }
    for (i, slot) in out.iter_mut().enumerate() {
        match (hex_val(b[2 * i]), hex_val(b[2 * i + 1])) {
            (Some(hi), Some(lo)) => *slot = (hi << 4) | lo,
            _ => return false,
        }
    }
    true
}

fn encode_token(raw: &[u8; TOKEN_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(TOKEN_HEX_LEN);
    for b in raw {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 15) as usize]);
    }
    // Only ASCII hex digits were pushed, so this cannot fail; no unwrap/panic path anyway.
    String::from_utf8(out).unwrap_or_default()
}

/// Hash of a well-formed presented token; `None` if it is malformed.
fn parse_hash(token: &str) -> Option<[u8; 32]> {
    let mut raw = [0u8; TOKEN_BYTES];
    let hash = decode_token(token, &mut raw).then(|| hash_token(&raw));
    raw.fill(0);
    std::hint::black_box(&raw);
    hash
}

/// Hash for a lookup: the constant dummy hash if the token is malformed.
fn lookup_hash(token: &str) -> [u8; 32] {
    parse_hash(token).unwrap_or(DUMMY_HASH)
}

fn parse_role(s: &str) -> Result<CapabilityRole> {
    match s {
        "imap" => Ok(CapabilityRole::Imap),
        "jmap" => Ok(CapabilityRole::Jmap),
        "dav" => Ok(CapabilityRole::Dav),
        "smtp-submit" => Ok(CapabilityRole::SmtpSubmit),
        _ => Err(Error::storage("stored capability role is invalid")),
    }
}

async fn db_now(conn: &mut SqliteConnection) -> Result<i64> {
    sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(conn)
        .await
        .map_err(map_db_error)
}

/// Revoke every active capability of `account` on `conn` (inside the caller's write
/// transaction). `now = None` uses the database clock. `revoked_at` is never before
/// `issued_at` (the schema forbids it), which matters only for a test clock in the past.
pub(crate) async fn revoke_all_in(
    conn: &mut SqliteConnection,
    account: AccountId,
    now: Option<i64>,
) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE capabilities SET revoked_at = MAX(COALESCE(?1, unixepoch()), issued_at) \
         WHERE account_id = ?2 AND revoked_at IS NULL AND expires_at > COALESCE(?1, unixepoch())",
    )
    .bind(now)
    .bind(account.0)
    .execute(conn)
    .await
    .map_err(map_db_error)?;
    Ok(res.rows_affected())
}

impl Directory {
    /// Issue a capability for an `active` account. `ttl_secs` is clamped to `1..=86400`.
    /// When the account already has [`MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT`] active ones, the
    /// oldest are revoked. A missing or non-active account is `NotFound("account")`.
    pub async fn issue_capability(
        &self,
        account: AccountId,
        role: CapabilityRole,
        ttl_secs: u32,
    ) -> Result<Capability> {
        self.issue_inner(account, role, ttl_secs, None).await
    }

    #[doc(hidden)]
    pub async fn issue_capability_at(
        &self,
        account: AccountId,
        role: CapabilityRole,
        ttl_secs: u32,
        now: i64,
    ) -> Result<Capability> {
        self.issue_inner(account, role, ttl_secs, Some(now)).await
    }

    async fn issue_inner(
        &self,
        account: AccountId,
        role: CapabilityRole,
        ttl_secs: u32,
        now: Option<i64>,
    ) -> Result<Capability> {
        let ttl = i64::from(ttl_secs.clamp(1, MAX_CAPABILITY_TTL_SECS));
        let mut raw = [0u8; TOKEN_BYTES];
        OsRng
            .try_fill_bytes(&mut raw)
            .map_err(|_| Error::storage("random source failed"))?;
        let hash = hash_token(&raw);
        let token = Capability(encode_token(&raw));
        raw.fill(0);
        std::hint::black_box(&raw);

        let mut tx = self.store().begin_write().await?;
        let now = match now {
            Some(n) => n,
            None => db_now(&mut tx).await?,
        };
        let expires_at = now
            .checked_add(ttl)
            .ok_or_else(|| Error::Invalid("capability time out of range".into()))?;
        let inserted = sqlx::query(
            "INSERT INTO capabilities (token_hash, account_id, domain_id, role, issued_at, expires_at) \
             SELECT ?, id, domain_id, ?, ?, ? FROM accounts WHERE id = ? AND status = 'active'",
        )
        .bind(&hash[..])
        .bind(role.as_str())
        .bind(now)
        .bind(expires_at)
        .bind(account.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        if inserted == 0 {
            return Err(Error::NotFound("account".into()));
        }
        // Keep at most 64 active: revoke the oldest beyond that (the new row is the newest).
        sqlx::query(
            "UPDATE capabilities SET revoked_at = MAX(?1, issued_at) WHERE rowid IN (\
               SELECT rowid FROM capabilities \
               WHERE account_id = ?2 AND revoked_at IS NULL AND expires_at > ?1 \
               ORDER BY issued_at, rowid \
               LIMIT MAX(0, (SELECT count(*) FROM capabilities \
                             WHERE account_id = ?2 AND revoked_at IS NULL AND expires_at > ?1) - ?3))",
        )
        .bind(now)
        .bind(account.0)
        .bind(MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(token)
    }

    /// Validate a presented token for `role`. `Ok(None)` for a malformed, unknown, expired,
    /// revoked or wrong-role token and for an account that is not `active`; `Err` only for
    /// storage failures.
    pub async fn validate_capability(
        &self,
        token: &str,
        role: CapabilityRole,
    ) -> Result<Option<CapabilityInfo>> {
        validate_inner(self.store(), token, role, None).await
    }

    #[doc(hidden)]
    pub async fn validate_capability_at(
        &self,
        token: &str,
        role: CapabilityRole,
        now: i64,
    ) -> Result<Option<CapabilityInfo>> {
        validate_inner(self.store(), token, role, Some(now)).await
    }

    /// Revoke one capability. `Ok(true)` if it was active; `Ok(false)` for a malformed,
    /// unknown, expired or already revoked token (a malformed one returns before any write
    /// transaction is opened).
    pub async fn revoke_capability(&self, token: &str) -> Result<bool> {
        self.revoke_one(token, None).await
    }

    #[doc(hidden)]
    pub async fn revoke_capability_at(&self, token: &str, now: i64) -> Result<bool> {
        self.revoke_one(token, Some(now)).await
    }

    async fn revoke_one(&self, token: &str, now: Option<i64>) -> Result<bool> {
        // A malformed token cannot match any row: answer without taking the writer lock.
        let Some(hash) = parse_hash(token) else {
            return Ok(false);
        };
        let mut tx = self.store().begin_write().await?;
        let n = sqlx::query(
            "UPDATE capabilities SET revoked_at = MAX(COALESCE(?1, unixepoch()), issued_at) \
             WHERE token_hash = ?2 AND revoked_at IS NULL \
               AND expires_at > COALESCE(?1, unixepoch())",
        )
        .bind(now)
        .bind(&hash[..])
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?
        .rows_affected();
        tx.commit().await.map_err(map_db_error)?;
        Ok(n > 0)
    }

    /// Revoke every active capability of `account`; returns how many were revoked.
    pub async fn revoke_all_capabilities(&self, account: AccountId) -> Result<u64> {
        self.revoke_all(account, None).await
    }

    #[doc(hidden)]
    pub async fn revoke_all_capabilities_at(&self, account: AccountId, now: i64) -> Result<u64> {
        self.revoke_all(account, Some(now)).await
    }

    async fn revoke_all(&self, account: AccountId, now: Option<i64>) -> Result<u64> {
        let mut tx = self.store().begin_write().await?;
        let n = revoke_all_in(&mut tx, account, now).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(n)
    }
}

async fn validate_inner(
    store: &SqliteStore,
    token: &str,
    role: CapabilityRole,
    now: Option<i64>,
) -> Result<Option<CapabilityInfo>> {
    let hash = lookup_hash(token);
    // One statement for every outcome: the primary-key lookup on the hash is the comparison.
    let row: Option<(i64, i64, String, i64)> = sqlx::query_as(
        "SELECT c.account_id, c.domain_id, c.role, c.expires_at \
         FROM capabilities c \
         JOIN accounts a ON a.id = c.account_id AND a.domain_id = c.domain_id \
         WHERE c.token_hash = ?1 AND c.revoked_at IS NULL \
           AND c.expires_at > COALESCE(?2, unixepoch()) \
           AND c.role = ?3 AND a.status = 'active'",
    )
    .bind(&hash[..])
    .bind(now)
    .bind(role.as_str())
    .fetch_optional(store.pool())
    .await
    .map_err(map_db_error)?;
    match row {
        None => Ok(None),
        Some((account, domain, role_s, expires_at)) => Ok(Some(CapabilityInfo {
            account: AccountId(account),
            domain: DomainId(domain),
            role: parse_role(&role_s)?,
            expires_at,
        })),
    }
}
