//! `SearchRepo` on Postgres: single statements, and the search itself in one transaction with a time limit of its own.

use async_trait::async_trait;
use rusty_core::Result;
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::searches::{sql, Found, Search, SearchCriteria, SearchRepo};

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A text as a pattern's content: `%`, `_` and the escape character mean themselves.
fn escaped(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn search(x: &sqlx::postgres::PgRow) -> std::result::Result<Search, sqlx::Error> {
    Ok(Search {
        id: x.try_get(0)?,
        requested_by: x.try_get(1)?,
        criteria: SearchCriteria {
            domain: x.try_get(2)?,
            account: x.try_get(3)?,
            from_text: x.try_get(4)?,
            to_text: x.try_get(5)?,
            subject_text: x.try_get(6)?,
            since: x.try_get(7)?,
            until: x.try_get(8)?,
        },
        state: x.try_get(9)?,
        found: i64::from(x.try_get::<i32, _>(10)?),
        truncated: x.try_get(11)?,
        results: x.try_get(12)?,
        error: x.try_get(13)?,
        created_at: x.try_get(14)?,
        finished_at: x.try_get(15)?,
    })
}

#[async_trait]
impl SearchRepo for PgRepo {
    async fn create(&self, requested_by: &str, c: &SearchCriteria) -> Result<i64> {
        sqlx::query_scalar(r(sql::CREATE))
            .bind(cut(requested_by, 320))
            .bind(c.domain.as_deref().map(|s| cut(s, 253)))
            .bind(c.account.as_deref().map(|s| cut(s, 320)))
            .bind(c.from_text.as_deref().map(|s| cut(s, 200)))
            .bind(c.to_text.as_deref().map(|s| cut(s, 200)))
            .bind(c.subject_text.as_deref().map(|s| cut(s, 200)))
            .bind(c.since)
            .bind(c.until)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)
    }

    async fn get(&self, id: i64) -> Result<Option<Search>> {
        let x = sqlx::query(r(sql::GET)).bind(id).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(search).transpose().map_err(map_db_error)
    }

    async fn claim(&self) -> Result<Option<Search>> {
        let x = sqlx::query(r(sql::CLAIM)).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        x.as_ref().map(search).transpose().map_err(map_db_error)
    }

    async fn run(&self, c: &SearchCriteria, limit: i64) -> Result<Vec<Found>> {
        // One transaction, so that the time limit is the statement's own and does not outlive it.
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        sqlx::query(r("SELECT set_config('statement_timeout', '10000', true)")).execute(&mut *tx).await.map_err(map_db_error)?;
        let rows = sqlx::query(r(sql::RUN))
            .bind(c.domain.as_deref())
            .bind(c.account.as_deref())
            .bind(c.from_text.as_deref().map(escaped))
            .bind(c.to_text.as_deref().map(escaped))
            .bind(c.subject_text.as_deref().map(escaped))
            .bind(c.since)
            .bind(c.until)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(Found { account: x.try_get(0)?, folder: x.try_get(1)?, at: x.try_get(2)?, from: x.try_get(3)?, subject: x.try_get(4)?, size: x.try_get(5)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn finish(&self, id: i64, state: &str, found: i64, truncated: bool, results: &str, error: Option<&str>) -> Result<()> {
        let found = i32::try_from(found).unwrap_or(i32::MAX);
        sqlx::query(r(sql::FINISH)).bind(id).bind(state).bind(found).bind(truncated).bind(results).bind(error).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn purge_expired(&self, limit: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::PURGE)).bind(limit).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }

    async fn fail_stuck(&self, older_than_secs: i64) -> Result<u64> {
        Ok(sqlx::query(r(sql::FAIL_STUCK)).bind(older_than_secs).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected())
    }
}
