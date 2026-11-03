use rusty_core::{AccountId, DomainId, Error, PlanId, Result};
use rusty_store::map_db_error;
use serde_json::Value;

use crate::dir::Directory;

pub(crate) const GLOBAL_PLAN: &str = "__global__";

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

pub(crate) fn ensure_object(v: &Value) -> Result<()> {
    if v.is_object() {
        Ok(())
    } else {
        Err(Error::Invalid("config must be a JSON object".into()))
    }
}

fn lookup(json: &str, key: &str) -> Result<Option<Value>> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| Error::storage_with("stored config is not valid JSON", e))?;
    Ok(v.get(key).filter(|x| !x.is_null()).cloned())
}

impl Directory {
    /// `sql` is one of the constant UPDATE statements below; `what` is the fixed NotFound text.
    async fn update_config(
        &self,
        sql: &'static str,
        what: &'static str,
        id: i64,
        config: Value,
    ) -> Result<()> {
        ensure_object(&config)?;
        let res = sqlx::query(sql)
            .bind(config.to_string())
            .bind(id)
            .execute(self.store().pool())
            .await
            .map_err(map_db_error)?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound(what.into()));
        }
        Ok(())
    }

    pub async fn set_domain_config(&self, domain: DomainId, config: Value) -> Result<()> {
        const SQL: &str = "UPDATE domains SET config = ? WHERE id = ?";
        self.update_config(SQL, "domain", domain.0, config).await
    }

    pub async fn set_account_config(&self, account: AccountId, config: Value) -> Result<()> {
        const SQL: &str = "UPDATE accounts SET config = ? WHERE id = ?";
        self.update_config(SQL, "account", account.0, config).await
    }

    pub async fn set_plan_config(&self, plan: PlanId, config: Value) -> Result<()> {
        const SQL: &str = "UPDATE plans SET config = ? WHERE id = ?";
        self.update_config(SQL, "plan", plan.0, config).await
    }

    pub async fn set_global_config(&self, config: Value) -> Result<()> {
        ensure_object(&config)?;
        sqlx::query(
            "INSERT INTO plans (name, config) VALUES (?, ?) \
             ON CONFLICT(name) DO UPDATE SET config = excluded.config",
        )
        .bind(GLOBAL_PLAN)
        .bind(config.to_string())
        .execute(self.store().pool())
        .await
        .map_err(map_db_error)?;
        Ok(())
    }

    pub async fn set_status(&self, account: AccountId, status: AccountStatus) -> Result<()> {
        let mut tx = self.store().begin_write().await?;
        let res = sqlx::query("UPDATE accounts SET status = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(account.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound("account".into()));
        }
        // Locking or disabling an account kills its capabilities in the same transaction.
        if status != AccountStatus::Active {
            crate::caps::revoke_all_in(&mut tx, account, None).await?;
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(())
    }

    /// First hit wins: account, then its plan, then its domain, then the global config.
    /// A JSON `null` value for the key means "inherit": that layer is skipped and lower layers
    /// are consulted; if every layer is null or absent the result is `None`.
    pub async fn effective_config(&self, account: AccountId, key: &str) -> Result<Option<Value>> {
        let pool = self.store().pool();
        let (acfg, plan_id, domain_id): (String, Option<i64>, i64) =
            sqlx::query_as("SELECT config, plan_id, domain_id FROM accounts WHERE id = ?")
                .bind(account.0)
                .fetch_optional(pool)
                .await
                .map_err(map_db_error)?
                .ok_or_else(|| Error::NotFound("account".into()))?;
        if let Some(v) = lookup(&acfg, key)? {
            return Ok(Some(v));
        }
        if let Some(pid) = plan_id {
            let cfg: Option<String> = sqlx::query_scalar("SELECT config FROM plans WHERE id = ?")
                .bind(pid)
                .fetch_optional(pool)
                .await
                .map_err(map_db_error)?;
            if let Some(v) = cfg.map(|c| lookup(&c, key)).transpose()?.flatten() {
                return Ok(Some(v));
            }
        }
        let dcfg: Option<String> = sqlx::query_scalar("SELECT config FROM domains WHERE id = ?")
            .bind(domain_id)
            .fetch_optional(pool)
            .await
            .map_err(map_db_error)?;
        if let Some(v) = dcfg.map(|c| lookup(&c, key)).transpose()?.flatten() {
            return Ok(Some(v));
        }
        let gcfg: Option<String> = sqlx::query_scalar("SELECT config FROM plans WHERE name = ?")
            .bind(GLOBAL_PLAN)
            .fetch_optional(pool)
            .await
            .map_err(map_db_error)?;
        gcfg.map(|c| lookup(&c, key))
            .transpose()
            .map(Option::flatten)
    }
}
