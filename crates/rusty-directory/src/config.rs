use rusty_core::{AccountId, DomainId, Error, PlanId, Result};
use rusty_store::repo::ConfigTarget;
use serde_json::Value;

use crate::dir::Directory;

pub use rusty_store::repo::AccountStatus;

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
    async fn update_config(&self, target: ConfigTarget, config: Value) -> Result<()> {
        ensure_object(&config)?;
        self.repo().set_config(target, &config.to_string()).await
    }

    pub async fn set_domain_config(&self, domain: DomainId, config: Value) -> Result<()> {
        self.update_config(ConfigTarget::Domain(domain), config).await
    }

    pub async fn set_account_config(&self, account: AccountId, config: Value) -> Result<()> {
        self.update_config(ConfigTarget::Account(account), config).await
    }

    pub async fn set_plan_config(&self, plan: PlanId, config: Value) -> Result<()> {
        self.update_config(ConfigTarget::Plan(plan), config).await
    }

    pub async fn set_global_config(&self, config: Value) -> Result<()> {
        self.update_config(ConfigTarget::Global, config).await
    }

    pub async fn set_status(&self, account: AccountId, status: AccountStatus) -> Result<()> {
        // The repository revokes the account's capabilities in the same transaction when the
        // status is not Active.
        self.repo().set_status(account, status).await
    }

    /// First hit wins: account, then its plan, then its domain, then the global config.
    /// A JSON `null` value for the key means "inherit": that layer is skipped and lower layers
    /// are consulted; if every layer is null or absent the result is `None`.
    pub async fn effective_config(&self, account: AccountId, key: &str) -> Result<Option<Value>> {
        let layers = self.repo().config_layers(account).await?;
        if let Some(v) = lookup(&layers.account, key)? {
            return Ok(Some(v));
        }
        for layer in [&layers.plan, &layers.domain, &layers.global] {
            if let Some(v) = layer.as_deref().map(|c| lookup(c, key)).transpose()?.flatten() {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }
}
