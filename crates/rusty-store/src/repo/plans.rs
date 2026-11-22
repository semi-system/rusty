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
}

pub(crate) mod sql {
    pub const LIST: &str = "SELECT id, name, CAST(config AS TEXT) FROM plans WHERE name <> $1 ORDER BY name";
    pub const SET_ACCOUNT_PLAN: &str = "UPDATE accounts SET plan_id = (SELECT id FROM plans WHERE name = CAST($2 AS TEXT) AND name <> $3) \
        WHERE id = $1 AND (CAST($2 AS TEXT) IS NULL OR EXISTS (SELECT 1 FROM plans WHERE name = CAST($2 AS TEXT) AND name <> $3))";
    pub const MAILBOX_USED: &str = "SELECT used_bytes FROM mailboxes WHERE id = $1";
    pub const USAGE_OF: &str = "SELECT a.id, COALESCE(m.used_bytes, 0), p.name, m.last_delivery_at, \
        CAST(COALESCE(a.config->>'quota_bytes', p.config->>'quota_bytes', d.config->>'quota_bytes', g.config->>'quota_bytes') AS BIGINT) \
        FROM accounts a JOIN domains d ON d.id = a.domain_id LEFT JOIN mailboxes m ON m.account_id = a.id \
        LEFT JOIN plans p ON p.id = a.plan_id LEFT JOIN plans g ON g.name = $2 \
        WHERE a.id = ANY($1) ORDER BY a.id";
}
