//! `DnsRepo` on SQLite: `BEGIN IMMEDIATE` is the global write lock, so the domain lock of the
//! Postgres implementation is only an existence check here.

use async_trait::async_trait;
use rusty_core::dns_state::{Observation, StateRow, Step};
use rusty_core::{DomainId, Error, Result};
use sqlx::{Row, SqliteConnection};

use super::SqliteRepo;
use crate::map_db_error;
use crate::repo::dns::{plan, sql, validate_observations, DnsConfig, DnsConfigRow, DnsEventRow, DnsRepo, RawEvent, RawState};
use crate::repo::render_sqlite as r;

fn no_domain() -> Error {
    Error::NotFound("domain".into())
}

async fn clock(c: &mut SqliteConnection, now: Option<i64>) -> Result<i64> {
    match now {
        Some(n) => Ok(n),
        None => sqlx::query_scalar(&crate::repo::sqlite_lit("SELECT unixepoch()"))
            .fetch_one(c)
            .await
            .map_err(map_db_error),
    }
}

async fn require_domain(c: &mut SqliteConnection, d: DomainId) -> Result<()> {
    sqlx::query_scalar::<_, i64>(&crate::repo::sqlite_lit("SELECT id FROM domains WHERE id = $1"))
        .bind(d.0)
        .fetch_optional(c)
        .await
        .map_err(map_db_error)?
        .map(|_| ())
        .ok_or_else(no_domain)
}

async fn load_states(c: &mut SqliteConnection, d: DomainId) -> Result<Vec<StateRow>> {
    let rows = sqlx::query(&r(sql::STATES)).bind(d.0).fetch_all(c).await.map_err(map_db_error)?;
    rows.iter()
        .map(|x| {
            RawState {
                name: x.get(0),
                rtype: x.get(1),
                purpose: x.get(2),
                expected: x.get(3),
                status: x.get(4),
                seen: x.get(5),
                first_ok_at: x.get(6),
                last_ok_at: x.get(7),
                last_checked_at: x.get(8),
                status_since: x.get(9),
                failures: x.get(10),
            }
            .into_row()
        })
        .collect()
}

#[async_trait]
impl DnsRepo for SqliteRepo {
    async fn db_now(&self) -> Result<i64> {
        sqlx::query_scalar(&crate::repo::sqlite_lit("SELECT unixepoch()"))
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn config(&self, domain: DomainId) -> Result<Option<DnsConfigRow>> {
        let row = sqlx::query(&r(sql::CONFIG))
            .bind(domain.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(row.map(|x| DnsConfigRow {
            domain,
            config: DnsConfig {
                dkim_selector: x.get(0),
                dkim_algorithm: x.get(1),
                dmarc_policy: x.get(2),
                dmarc_rua: x.get(3),
                outbound_ref: x.get(4),
            },
            has_ownership_token: x.get::<i64, _>(5) != 0,
            expected_hash: x.get(6),
            created_at: x.get(7),
            updated_at: x.get(8),
        }))
    }

    async fn put_config(&self, domain: DomainId, config: &DnsConfig, now: Option<i64>) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        require_domain(&mut tx, domain).await?;
        sqlx::query(&r(sql::PUT_CONFIG))
            .bind(domain.0)
            .bind(&config.dkim_selector)
            .bind(&config.dkim_algorithm)
            .bind(&config.dmarc_policy)
            .bind(&config.dmarc_rua)
            .bind(&config.outbound_ref)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)
    }

    async fn set_ownership_token_hash(&self, domain: DomainId, hash: &str, now: Option<i64>) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        require_domain(&mut tx, domain).await?;
        sqlx::query(&r(sql::SET_TOKEN))
            .bind(domain.0)
            .bind(hash)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)
    }

    async fn ownership_token_hash(&self, domain: DomainId) -> Result<Option<String>> {
        let v: Option<Option<String>> = sqlx::query_scalar(&r(sql::GET_TOKEN))
            .bind(domain.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(v.flatten())
    }

    async fn set_expected_hash(&self, domain: DomainId, hash: &str, now: Option<i64>) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        require_domain(&mut tx, domain).await?;
        sqlx::query(&r(sql::SET_EXPECTED))
            .bind(domain.0)
            .bind(hash)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)
    }

    async fn states(&self, domain: DomainId) -> Result<Vec<StateRow>> {
        let mut c = self.store.pool().acquire().await.map_err(map_db_error)?;
        load_states(&mut c, domain).await
    }

    async fn record_observations(
        &self,
        domain: DomainId,
        observations: &[Observation],
        alert_after: u32,
        event_cap: u32,
        now: Option<i64>,
    ) -> Result<Vec<Step>> {
        validate_observations(observations)?;
        let mut tx = self.store.begin_write().await?;
        require_domain(&mut tx, domain).await?;
        let now = clock(&mut tx, now).await?;
        let existing = load_states(&mut tx, domain).await?;
        let plan = plan(existing, observations, alert_after, now);
        for s in &plan.steps {
            let row = &s.row;
            sqlx::query(&r(sql::UPSERT_STATE))
                .bind(domain.0)
                .bind(&row.key.name)
                .bind(&row.key.rtype)
                .bind(&row.key.purpose)
                .bind(&row.expected_hash)
                .bind(row.status.as_str())
                .bind(&row.seen)
                .bind(row.first_ok_at)
                .bind(row.last_ok_at)
                .bind(row.last_checked_at)
                .bind(row.status_since)
                .bind(i64::from(row.consecutive_failures))
                .execute(&mut *tx)
                .await
                .map_err(map_db_error)?;
            if let Some(ch) = &s.change {
                sqlx::query(&r(sql::INSERT_EVENT))
                    .bind(domain.0)
                    .bind(&ch.key.name)
                    .bind(&ch.key.rtype)
                    .bind(&ch.key.purpose)
                    .bind(ch.from.map(|f| f.as_str()))
                    .bind(ch.to.as_str())
                    .bind(ch.at)
                    .execute(&mut *tx)
                    .await
                    .map_err(map_db_error)?;
            }
        }
        for k in &plan.delete {
            sqlx::query(&r(sql::DELETE_STATE))
                .bind(domain.0)
                .bind(&k.name)
                .bind(&k.rtype)
                .bind(&k.purpose)
                .execute(&mut *tx)
                .await
                .map_err(map_db_error)?;
        }
        sqlx::query(&r(sql::TRIM_EVENTS))
            .bind(domain.0)
            .bind(i64::from(event_cap))
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(plan.steps)
    }

    async fn events(&self, domain: DomainId, since: i64, limit: u32) -> Result<Vec<DnsEventRow>> {
        let rows = sqlx::query(&r(sql::EVENTS))
            .bind(domain.0)
            .bind(since)
            .bind(i64::from(limit))
            .fetch_all(self.store.pool())
            .await
            .map_err(map_db_error)?;
        rows.iter()
            .map(|x| {
                RawEvent {
                    id: x.get(0),
                    name: x.get(1),
                    rtype: x.get(2),
                    purpose: x.get(3),
                    from: x.get(4),
                    to: x.get(5),
                    at: x.get(6),
                }
                .into_row()
            })
            .collect()
    }
}
