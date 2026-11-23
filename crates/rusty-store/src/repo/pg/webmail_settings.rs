//! `WebmailSettingsRepo` on Postgres: single statements.

use async_trait::async_trait;
use rusty_core::{AccountId, Error, Result};

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::webmail_settings::{sql, WebmailSettingsRepo, MAX_SIGNATURE};

#[async_trait]
impl WebmailSettingsRepo for PgRepo {
    async fn signature(&self, account: AccountId) -> Result<String> {
        let s: Option<String> = sqlx::query_scalar(r(sql::GET)).bind(account.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        Ok(s.unwrap_or_default())
    }

    async fn set_signature(&self, account: AccountId, signature: &str) -> Result<()> {
        if signature.chars().count() > MAX_SIGNATURE {
            return Err(Error::Invalid("signature too long".into()));
        }
        sqlx::query(r(sql::SET)).bind(account.0).bind(signature).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }
}
