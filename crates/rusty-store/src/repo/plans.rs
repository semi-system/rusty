//! `PlanRepo`: the plans the operator manages and what an account uses (docs/design-plans-quota.md). A plan is a named bundle of configuration keys (`quota_bytes`, `max_message_bytes`); the
//! directory layers read them (account, plan, domain, global). The admin web lists the plans, puts an account on one, and shows the usage and the quota of a page of accounts.

use async_trait::async_trait;
use rusty_core::{AccountId, Result};

/// One plan as stored; `config` is the JSON object as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    pub id: i64,
    pub name: String,
    pub config: String,
}

/// What an account holds and may hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsage {
    pub account: i64,
    /// Bytes of the messages the mailbox has (migration 0032).
    pub used_bytes: i64,
    /// When mail last arrived by delivery (migration 0037); `None`: never.
    pub last_delivery_at: Option<i64>,
    /// A temporary password is set: the account cannot sign in until it is changed (migration 0039).
    pub must_change_password: bool,
    /// The name of the plan the account is on, if any.
    pub plan: Option<String>,
    /// The quota in bytes that applies (account, plan, domain, global config; the first that has one). `None`: unlimited.
    pub quota_bytes: Option<i64>,
}

#[async_trait]
pub trait PlanRepo: Send + Sync {
    /// Every plan by name, without the reserved row of the global configuration.
    async fn list(&self) -> Result<Vec<PlanRow>>;
    /// Put the account on the plan named `plan`, or on none. `NotFound` for an unknown account or plan.
    async fn set_account_plan(&self, account: AccountId, plan: Option<&str>) -> Result<()>;
    /// Usage, plan and quota of these accounts (at most 500), in one read; an unknown id is left out.
    async fn usage_of(&self, accounts: &[i64]) -> Result<Vec<AccountUsage>>;
    /// The bytes a mailbox holds now (migration 0032), for the IMAP `QUOTA` answer. `0` for an unknown mailbox.
    async fn mailbox_used(&self, mailbox: i64) -> Result<i64>;
    /// Whether the account has a temporary password it must change before it is used (migration 0039); `false` for an unknown account.
    async fn must_change_password(&self, account: i64) -> Result<bool>;
    /// Set or clear that flag (the admin web sets it together with the password).
    async fn set_must_change_password(&self, account: i64, on: bool) -> Result<()>;
    /// The platform-wide configuration object (the last layer of every account) as JSON text; `{}` while none was ever set.
    async fn global_config(&self) -> Result<String>;
    /// Change keys of the platform-wide configuration: remove the keys in `remove`, then add the members of the JSON object `set` (the row is made when it is missing).
    async fn set_global_keys(&self, remove: &[String], set: &str) -> Result<()>;
    /// The configuration object of a domain as JSON text (`{}` for none; `NotFound` for an unknown domain).
    async fn domain_config(&self, domain: i64) -> Result<String>;
    /// Change keys of a domain's configuration in one statement: remove the keys in `remove`, then add the members of the JSON object `set`.
    async fn set_domain_keys(&self, domain: i64, remove: &[String], set: &str) -> Result<()>;
}

pub(crate) mod sql {
    pub const LIST: &str = "SELECT id, name, CAST(config AS TEXT) FROM plans WHERE name <> $1 ORDER BY name";
    pub const SET_ACCOUNT_PLAN: &str = "UPDATE accounts SET plan_id = (SELECT id FROM plans WHERE name = CAST($2 AS TEXT) AND name <> $3) \
        WHERE id = $1 AND (CAST($2 AS TEXT) IS NULL OR EXISTS (SELECT 1 FROM plans WHERE name = CAST($2 AS TEXT) AND name <> $3))";
    pub const MUST_CHANGE: &str = "SELECT must_change_password FROM accounts WHERE id = $1";
    pub const SET_MUST_CHANGE: &str = "UPDATE accounts SET must_change_password = $2 WHERE id = $1";
    pub const GLOBAL_CONFIG: &str = "SELECT CAST(config AS TEXT) FROM plans WHERE name = $1";
    pub const SET_GLOBAL_KEYS: &str = "INSERT INTO plans (name, config) VALUES ($1, CAST($3 AS jsonb)) \
        ON CONFLICT (name) DO UPDATE SET config = (plans.config - $2::text[]) || excluded.config";
    pub const DOMAIN_CONFIG: &str = "SELECT CAST(config AS TEXT) FROM domains WHERE id = $1";
    pub const SET_DOMAIN_KEYS: &str = "UPDATE domains SET config = (config - $2::text[]) || CAST($3 AS jsonb) WHERE id = $1";
    pub const MAILBOX_USED: &str = "SELECT used_bytes FROM mailboxes WHERE id = $1";
    pub const USAGE_OF: &str = "SELECT a.id, COALESCE(m.used_bytes, 0), p.name, m.last_delivery_at, a.must_change_password, \
        CAST(COALESCE(a.config->>'quota_bytes', p.config->>'quota_bytes', d.config->>'quota_bytes', g.config->>'quota_bytes') AS BIGINT) \
        FROM accounts a JOIN domains d ON d.id = a.domain_id LEFT JOIN mailboxes m ON m.account_id = a.id \
        LEFT JOIN plans p ON p.id = a.plan_id LEFT JOIN plans g ON g.name = $2 \
        WHERE a.id = ANY($1) ORDER BY a.id";
}
