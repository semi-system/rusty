//! `SettingsRepo` on Postgres: single statements, atomic by themselves.

use async_trait::async_trait;
use rusty_core::{Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::settings::{sql, SettingsRepo, PROTOCOLS};

#[async_trait]
impl SettingsRepo for PgRepo {
    async fn protocols(&self) -> Result<Vec<(String, bool)>> {
        let rows = sqlx::query(sql::LIST).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok((x.try_get::<String, _>(0)?.trim_start_matches("protocol_").to_owned(), x.try_get::<bool, _>(1)?)))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn set_protocol(&self, protocol: &str, enabled: bool, now: Option<i64>) -> Result<()> {
        if !PROTOCOLS.contains(&protocol) {
            return Err(Error::Invalid("unknown protocol".into()));
        }
        sqlx::query(r(sql::SET)).bind(format!("protocol_{protocol}")).bind(enabled).bind(now).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }
}
