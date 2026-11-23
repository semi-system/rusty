//! `DnsZoneRepo` on Postgres: single statements.

use std::collections::HashMap;

use async_trait::async_trait;
use rusty_core::{DomainId, Error, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::dns_zone::{sql, DnsZoneRepo, ExtraRecord, HostedDomain, MAX_EXTRA_RECORDS};
use crate::repo::render_pg as r;

fn extra(x: &sqlx::postgres::PgRow, at: usize) -> std::result::Result<ExtraRecord, sqlx::Error> {
    Ok(ExtraRecord { id: x.try_get(at)?, name: x.try_get(at + 1)?, rtype: x.try_get(at + 2)?, value: x.try_get(at + 3)?, ttl: u32::try_from(x.try_get::<i32, _>(at + 4)?).unwrap_or(3600) })
}

#[async_trait]
impl DnsZoneRepo for PgRepo {
    async fn hosted(&self) -> Result<Vec<HostedDomain>> {
        let pool = self.store.pool();
        let rows = sqlx::query(r(sql::HOSTED)).fetch_all(pool).await.map_err(map_db_error)?;
        let ids: Vec<i64> = rows.iter().map(|x| x.try_get(0)).collect::<std::result::Result<_, _>>().map_err(map_db_error)?;
        let mut keys: HashMap<i64, Vec<(String, String, String)>> = HashMap::new();
        for k in sqlx::query(r(sql::KEYS)).bind(&ids).fetch_all(pool).await.map_err(map_db_error)? {
            keys.entry(k.try_get(0).map_err(map_db_error)?).or_default().push((k.try_get(1).map_err(map_db_error)?, k.try_get(2).map_err(map_db_error)?, k.try_get(3).map_err(map_db_error)?));
        }
        let mut extras: HashMap<i64, Vec<ExtraRecord>> = HashMap::new();
        for e in sqlx::query(r(sql::EXTRAS_OF_MANY)).bind(&ids).fetch_all(pool).await.map_err(map_db_error)? {
            extras.entry(e.try_get(0).map_err(map_db_error)?).or_default().push(extra(&e, 1).map_err(map_db_error)?);
        }
        rows.iter()
            .map(|x| {
                let id: i64 = x.try_get(0)?;
                Ok(HostedDomain { name: x.try_get(1)?, spf_include: x.try_get(2)?, dkim: keys.remove(&id).unwrap_or_default(), extra: extras.remove(&id).unwrap_or_default() })
            })
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn set_mode(&self, domain: DomainId, mode: &str) -> Result<()> {
        if !matches!(mode, "external" | "hosted") {
            return Err(Error::Invalid("dns mode".into()));
        }
        let done = sqlx::query(r(sql::SET_MODE)).bind(domain.0).bind(mode).execute(self.store.pool()).await.map_err(map_db_error)?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("domain".into()));
        }
        Ok(())
    }

    async fn mode(&self, domain: DomainId) -> Result<String> {
        sqlx::query_scalar(r(sql::MODE)).bind(domain.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?.ok_or_else(|| Error::NotFound("domain".into()))
    }

    async fn extras(&self, domain: DomainId) -> Result<Vec<ExtraRecord>> {
        let rows = sqlx::query(r(sql::EXTRAS_OF)).bind(domain.0).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(|x| extra(x, 0)).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn add_extra(&self, domain: DomainId, name: &str, rtype: &str, value: &str, ttl: u32) -> Result<i64> {
        let have: i64 = sqlx::query_scalar(r(sql::COUNT)).bind(domain.0).fetch_one(self.store.pool()).await.map_err(map_db_error)?;
        if have >= MAX_EXTRA_RECORDS {
            return Err(Error::Quota("extra records".into()));
        }
        let ttl = i32::try_from(ttl).map_err(|_| Error::Invalid("ttl".into()))?;
        sqlx::query_scalar(r(sql::ADD)).bind(domain.0).bind(name).bind(rtype).bind(value).bind(ttl).fetch_one(self.store.pool()).await.map_err(map_db_error)
    }

    async fn remove_extra(&self, domain: DomainId, id: i64) -> Result<bool> {
        Ok(sqlx::query(r(sql::REMOVE)).bind(domain.0).bind(id).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }
}
