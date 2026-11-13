//! `DnsRepo` on Postgres (READ COMMITTED). Every write first takes the domain row `FOR NO KEY
//! UPDATE` (does not block foreign-key checks of other tables), which serialises concurrent
//! checks of one domain; transaction-scoped, so it is safe behind a transaction-pooling proxy.

use async_trait::async_trait;
use rusty_core::dns_state::{Observation, StateRow, Step};
use rusty_core::{DomainId, Error, Result};
use sqlx::{PgConnection, Row};

use super::tx::{tx_retry, TxError, TxResult};
use super::PgRepo;
use crate::map_db_error;
use crate::repo::dns::{plan, sql, validate_observations, DnsConfig, DnsConfigRow, DnsEventRow, DnsRepo, RawEvent, RawState};
use crate::repo::render_pg as r;

async fn lock_domain(c: &mut PgConnection, d: DomainId) -> TxResult<()> {
    sqlx::query_scalar::<_, i64>(&crate::repo::pg_lit("SELECT id FROM domains WHERE id = $1 FOR NO KEY UPDATE"))
        .bind(d.0)
        .fetch_optional(&mut *c)
        .await?
        .map(|_| ())
        .ok_or_else(|| Error::NotFound("domain".into()).into())
}

async fn load_states(c: &mut PgConnection, d: DomainId) -> TxResult<Vec<StateRow>> {
    let rows = sqlx::query(&r(sql::STATES)).bind(d.0).fetch_all(&mut *c).await?;
    let mut out = Vec::with_capacity(rows.len());
    for x in &rows {
        out.push(
            RawState {
                name: x.try_get(0)?,
                rtype: x.try_get(1)?,
                purpose: x.try_get(2)?,
                expected: x.try_get(3)?,
                status: x.try_get(4)?,
                seen: x.try_get(5)?,
                first_ok_at: x.try_get(6)?,
                last_ok_at: x.try_get(7)?,
                last_checked_at: x.try_get(8)?,
                status_since: x.try_get(9)?,
                failures: x.try_get(10)?,
            }
            .into_row()?,
        );
    }
    Ok(out)
}

fn plain(e: TxError) -> Error {
    match e {
        TxError::Db(e) => map_db_error(e),
        TxError::Domain(e) => e,
    }
}

#[async_trait]
impl DnsRepo for PgRepo {
    async fn db_now(&self) -> Result<i64> {
        sqlx::query_scalar(&r("SELECT {NOW}"))
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
        row.map(|x| -> std::result::Result<DnsConfigRow, sqlx::Error> {
            Ok(DnsConfigRow {
                domain,
                config: DnsConfig {
                    dkim_selector: x.try_get(0)?,
                    dkim_algorithm: x.try_get(1)?,
                    dmarc_policy: x.try_get(2)?,
                    dmarc_rua: x.try_get(3)?,
                    outbound_ref: x.try_get(4)?,
                },
                has_ownership_token: x.try_get::<i64, _>(5)? != 0,
                expected_hash: x.try_get(6)?,
                created_at: x.try_get(7)?,
                updated_at: x.try_get(8)?,
            })
        })
        .transpose()
        .map_err(map_db_error)
    }

    async fn put_config(&self, domain: DomainId, config: &DnsConfig, now: Option<i64>) -> Result<()> {
        tx_retry!(self, None, |c| {
            {
                lock_domain(&mut *c, domain).await?;
                sqlx::query(&r(sql::PUT_CONFIG))
                    .bind(domain.0)
                    .bind(&config.dkim_selector)
                    .bind(&config.dkim_algorithm)
                    .bind(&config.dmarc_policy)
                    .bind(&config.dmarc_rua)
                    .bind(&config.outbound_ref)
                    .bind(now)
                    .execute(&mut *c)
                    .await?;
                Ok(())
            }
        })
    }

    async fn set_ownership_token_hash(&self, domain: DomainId, hash: &str, now: Option<i64>) -> Result<()> {
        tx_retry!(self, None, |c| {
            {
                lock_domain(&mut *c, domain).await?;
                sqlx::query(&r(sql::SET_TOKEN))
                    .bind(domain.0)
                    .bind(hash)
                    .bind(now)
                    .execute(&mut *c)
                    .await?;
                Ok(())
            }
        })
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
        tx_retry!(self, None, |c| {
            {
                lock_domain(&mut *c, domain).await?;
                sqlx::query(&r(sql::SET_EXPECTED))
                    .bind(domain.0)
                    .bind(hash)
                    .bind(now)
                    .execute(&mut *c)
                    .await?;
                Ok(())
            }
        })
    }

    async fn states(&self, domain: DomainId) -> Result<Vec<StateRow>> {
        let mut conn = self.store.pool().acquire().await.map_err(map_db_error)?;
        load_states(&mut conn, domain).await.map_err(plain)
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
        tx_retry!(self, None, |c| {
            {
                lock_domain(&mut *c, domain).await?;
                let now: i64 = sqlx::query_scalar(&r("SELECT COALESCE($1, {NOW})"))
                    .bind(now)
                    .fetch_one(&mut *c)
                    .await?;
                let existing = load_states(&mut *c, domain).await?;
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
                        .execute(&mut *c)
                        .await?;
                    if let Some(ch) = &s.change {
                        sqlx::query(&r(sql::INSERT_EVENT))
                            .bind(domain.0)
                            .bind(&ch.key.name)
                            .bind(&ch.key.rtype)
                            .bind(&ch.key.purpose)
                            .bind(ch.from.map(|f| f.as_str()))
                            .bind(ch.to.as_str())
                            .bind(ch.at)
                            .execute(&mut *c)
                            .await?;
                    }
                }
                for k in &plan.delete {
                    sqlx::query(&r(sql::DELETE_STATE))
                        .bind(domain.0)
                        .bind(&k.name)
                        .bind(&k.rtype)
                        .bind(&k.purpose)
                        .execute(&mut *c)
                        .await?;
                }
                sqlx::query(&r(sql::TRIM_EVENTS))
                    .bind(domain.0)
                    .bind(i64::from(event_cap))
                    .execute(&mut *c)
                    .await?;
                Ok(plan.steps)
            }
        })
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
            .map(|x| -> Result<DnsEventRow> {
                RawEvent {
                    id: x.try_get(0).map_err(map_db_error)?,
                    name: x.try_get(1).map_err(map_db_error)?,
                    rtype: x.try_get(2).map_err(map_db_error)?,
                    purpose: x.try_get(3).map_err(map_db_error)?,
                    from: x.try_get(4).map_err(map_db_error)?,
                    to: x.try_get(5).map_err(map_db_error)?,
                    at: x.try_get(6).map_err(map_db_error)?,
                }
                .into_row()
            })
            .collect()
    }
}
