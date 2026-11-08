//! `DirectoryRepo` on SQLite. Multi-statement operations use `begin_write` (`BEGIN IMMEDIATE`:
//! the global write lock is the arbiter), single statements run on the pool.

use async_trait::async_trait;
use rusty_core::{AccountId, DomainId, Error, ListId, MailboxId, PlanId, Result};
use sqlx::SqliteConnection;

use super::SqliteRepo;
use crate::map_db_error;
use crate::repo::directory::{capability_expiry, check_list_capacity, list_resolution, sql};
use crate::repo::{
    render_sqlite as r, AccountStatus, AuthRow, CapabilityGrant, ConfigLayers, ConfigTarget,
    DirectoryRepo, NewAccount, Resolution, GLOBAL_PLAN, SYSTEM_FOLDERS, SYSTEM_FOLDER_TABLE,
};

/// Upper clamp of the uid_validity base so that `base + SYSTEM_FOLDERS` fits in u32.
const UV_MAX_BASE: i64 = u32::MAX as i64 - SYSTEM_FOLDERS;

async fn revoke_all_in(
    c: &mut SqliteConnection,
    account: AccountId,
    now: Option<i64>,
) -> Result<u64> {
    Ok(sqlx::query(&r(sql::CAP_REVOKE_ALL))
        .bind(now)
        .bind(account.0)
        .execute(c)
        .await
        .map_err(map_db_error)?
        .rows_affected())
}

#[async_trait]
impl DirectoryRepo for SqliteRepo {
    async fn create_domain(&self, name: &str) -> Result<DomainId> {
        let id: i64 = sqlx::query_scalar(&r(sql::INSERT_DOMAIN))
            .bind(name)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(DomainId(id))
    }

    async fn create_plan(&self, name: &str, config: &str) -> Result<PlanId> {
        let id: i64 = sqlx::query_scalar(&r(sql::INSERT_PLAN))
            .bind(name)
            .bind(config)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(PlanId(id))
    }

    async fn create_account(&self, new: &NewAccount) -> Result<AccountId> {
        let mut tx = self.store.begin_write().await?;
        let acc: i64 = sqlx::query_scalar(&r(sql::INSERT_ACCOUNT))
            .bind(&new.local)
            .bind(new.domain.0)
            .bind(new.plan.map(|p| p.0))
            .bind(new.password_hash.as_deref())
            .fetch_one(&mut *tx)
            .await
            .map_err(map_db_error)?;
        // uid_validity high-water mark = clamped DATABASE clock + 5 (largest value issued below).
        let (mb, uv_base): (i64, i64) = sqlx::query_as(
            &crate::repo::sqlite_lit("INSERT INTO mailboxes (account_id, uid_validity, next_folder_id) \
             VALUES (?1, min(max(unixepoch(), 1), ?2) + ?3, ?4) RETURNING id, uid_validity - ?3"),
        )
        .bind(acc)
        .bind(UV_MAX_BASE)
        .bind(SYSTEM_FOLDERS)
        .bind(SYSTEM_FOLDERS + 1)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_error)?;
        for (id, name, role) in SYSTEM_FOLDER_TABLE {
            sqlx::query(
                &crate::repo::sqlite_lit("INSERT INTO folders (mailbox_id, id, parent_id, name, role, uid_validity) \
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5)"),
            )
            .bind(mb)
            .bind(id)
            .bind(name)
            .bind(role)
            .bind(uv_base + id)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
        }
        tx.commit().await.map_err(map_db_error)?;
        Ok(AccountId(acc))
    }

    async fn mailbox_for(&self, account: AccountId) -> Result<MailboxId> {
        sqlx::query_scalar::<_, i64>(&r(sql::MAILBOX_FOR_ACCOUNT))
            .bind(account.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?
            .map(MailboxId)
            .ok_or_else(|| Error::NotFound("mailbox for account".into()))
    }

    async fn set_password_hash(&self, account: AccountId, hash: &str) -> Result<()> {
        // One transaction: a new password and the revocation of every capability issued under
        // the old one become visible together.
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(&r(sql::UPDATE_PASSWORD))
            .bind(hash)
            .bind(account.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("account".into()));
        }
        revoke_all_in(&mut tx, account, None).await?;
        tx.commit().await.map_err(map_db_error)
    }

    async fn set_status(&self, account: AccountId, status: AccountStatus) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(&r(sql::UPDATE_STATUS))
            .bind(status.as_str())
            .bind(account.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("account".into()));
        }
        // Locking or disabling an account kills its capabilities in the same transaction.
        if status != AccountStatus::Active {
            revoke_all_in(&mut tx, account, None).await?;
        }
        tx.commit().await.map_err(map_db_error)
    }

    async fn account_auth(&self, account: AccountId) -> Result<Option<AuthRow>> {
        let row: Option<(String, Option<String>)> = sqlx::query_as(&r(sql::ACCOUNT_AUTH))
            .bind(account.0)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(row.map(|(status, password_hash)| AuthRow {
            status,
            password_hash,
        }))
    }

    async fn add_alias(&self, local: &str, domain: DomainId, account: AccountId) -> Result<()> {
        sqlx::query(&r(sql::INSERT_ALIAS))
            .bind(local)
            .bind(domain.0)
            .bind(account.0)
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    async fn create_list(&self, local: &str, domain: DomainId) -> Result<ListId> {
        let id: i64 = sqlx::query_scalar(&r(sql::INSERT_LIST))
            .bind(local)
            .bind(domain.0)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(ListId(id))
    }

    async fn add_list_member(
        &self,
        list: ListId,
        account: Option<AccountId>,
        external: Option<&str>,
        max_members: usize,
    ) -> Result<()> {
        // The count and the insert share one write transaction, so concurrent adds cannot
        // both take the last slot or the same `seq`.
        let mut tx = self.store.begin_write().await?;
        let members: i64 = sqlx::query_scalar(&r(sql::COUNT_MEMBERS))
            .bind(list.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_db_error)?;
        check_list_capacity(members, max_members)?;
        sqlx::query(
            &crate::repo::sqlite_lit("INSERT INTO list_members (list_id, account_id, external, seq) \
             SELECT ?1, ?2, ?3, COALESCE(MAX(seq), 0) + 1 FROM list_members WHERE list_id = ?1"),
        )
        .bind(list.0)
        .bind(account.map(|a| a.0))
        .bind(external)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)
    }

    async fn resolve(
        &self,
        local: &str,
        domain: &str,
        max_members: usize,
    ) -> Result<Option<Resolution>> {
        let pool = self.store.pool();
        let Some((kind, id)): Option<(i64, i64)> = sqlx::query_as(&r(sql::RESOLVE_KIND))
            .bind(domain)
            .bind(local)
            .fetch_optional(pool)
            .await
            .map_err(map_db_error)?
        else {
            return Ok(None);
        };
        if kind != 3 {
            return Ok(Some(Resolution::Account(AccountId(id))));
        }
        let rows: Vec<(Option<i64>, Option<String>)> = sqlx::query_as(&r(sql::LIST_MEMBERS))
            .bind(id)
            .bind(max_members as i64 + 1)
            .fetch_all(pool)
            .await
            .map_err(map_db_error)?;
        list_resolution(rows, max_members).map(Some)
    }

    async fn set_config(&self, target: ConfigTarget, config: &str) -> Result<()> {
        let pool = self.store.pool();
        let (stmt, id, what) = match target {
            ConfigTarget::Domain(d) => (sql::SET_DOMAIN_CONFIG, d.0, "domain"),
            ConfigTarget::Account(a) => (sql::SET_ACCOUNT_CONFIG, a.0, "account"),
            ConfigTarget::Plan(p) => (sql::SET_PLAN_CONFIG, p.0, "plan"),
            ConfigTarget::Global => {
                sqlx::query(&r(sql::SET_GLOBAL_CONFIG))
                    .bind(GLOBAL_PLAN)
                    .bind(config)
                    .execute(pool)
                    .await
                    .map_err(map_db_error)?;
                return Ok(());
            }
        };
        let n = sqlx::query(&r(stmt))
            .bind(config)
            .bind(id)
            .execute(pool)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound(what.into()));
        }
        Ok(())
    }

    async fn config_layers(&self, account: AccountId) -> Result<ConfigLayers> {
        let row: Option<(String, Option<String>, Option<String>, Option<String>)> =
            sqlx::query_as(&r(sql::CONFIG_LAYERS))
                .bind(account.0)
                .bind(GLOBAL_PLAN)
                .fetch_optional(self.store.pool())
                .await
                .map_err(map_db_error)?;
        let (account, plan, domain, global) =
            row.ok_or_else(|| Error::NotFound("account".into()))?;
        Ok(ConfigLayers {
            account,
            plan,
            domain,
            global,
        })
    }

    async fn issue_capability(
        &self,
        account: AccountId,
        role: &str,
        token_hash: &[u8; 32],
        ttl_secs: i64,
        max_active: i64,
        now: Option<i64>,
    ) -> Result<()> {
        let mut tx = self.store.begin_write().await?;
        let now = match now {
            Some(n) => n,
            None => sqlx::query_scalar(&r(sql::NOW))
                .fetch_one(&mut *tx)
                .await
                .map_err(map_db_error)?,
        };
        let expires_at = capability_expiry(now, ttl_secs)?;
        let inserted = sqlx::query(&r(sql::CAP_INSERT))
            .bind(&token_hash[..])
            .bind(role)
            .bind(now)
            .bind(expires_at)
            .bind(account.0)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        if inserted == 0 {
            return Err(Error::NotFound("account".into()));
        }
        // Keep at most `max_active`: revoke the oldest beyond that. Ties on `issued_at` (second
        // granularity) are broken by `token_hash`, exactly as on Postgres, and the new row is
        // never a candidate.
        sqlx::query(
            &crate::repo::sqlite_lit("UPDATE capabilities SET revoked_at = MAX(?1, issued_at) WHERE rowid IN (\
               SELECT rowid FROM capabilities \
               WHERE account_id = ?2 AND revoked_at IS NULL AND expires_at > ?1 \
                 AND token_hash <> ?4 \
               ORDER BY issued_at, token_hash \
               LIMIT MAX(0, (SELECT count(*) FROM capabilities \
                             WHERE account_id = ?2 AND revoked_at IS NULL AND expires_at > ?1) - ?3))"),
        )
        .bind(now)
        .bind(account.0)
        .bind(max_active)
        .bind(&token_hash[..])
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)
    }

    async fn validate_capability(
        &self,
        token_hash: &[u8; 32],
        role: &str,
        now: Option<i64>,
    ) -> Result<Option<CapabilityGrant>> {
        let row: Option<(i64, i64, String, i64)> = sqlx::query_as(&r(sql::CAP_VALIDATE))
            .bind(&token_hash[..])
            .bind(now)
            .bind(role)
            .fetch_optional(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(row.map(|(account, domain, role, expires_at)| CapabilityGrant {
            account: AccountId(account),
            domain: DomainId(domain),
            role,
            expires_at,
        }))
    }

    async fn revoke_capability(&self, token_hash: &[u8; 32], now: Option<i64>) -> Result<bool> {
        let mut tx = self.store.begin_write().await?;
        let n = sqlx::query(&r(sql::CAP_REVOKE_ONE))
            .bind(now)
            .bind(&token_hash[..])
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?
            .rows_affected();
        tx.commit().await.map_err(map_db_error)?;
        Ok(n > 0)
    }

    async fn revoke_all_capabilities(&self, account: AccountId, now: Option<i64>) -> Result<u64> {
        let mut tx = self.store.begin_write().await?;
        let n = revoke_all_in(&mut tx, account, now).await?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(n)
    }
}
