//! `DirectoryRepo` on Postgres. Single-statement operations run on the pool (auto-commit);
//! read-then-write operations use `tx_retry!` and lock the row they depend on first.

use async_trait::async_trait;
use rusty_core::{AccountId, DomainId, Error, ListId, MailboxId, PlanId, Result};
use sqlx::PgConnection;

use super::tx::{tx_retry, TxError, TxResult};
use super::PgRepo;
use crate::map_db_error;
use crate::repo::directory::{
    capability_expiry, check_list_capacity, check_no_nul, check_text, list_resolution, sql,
    MAX_DOMAIN_BYTES, MAX_EXTERNAL_MEMBER_BYTES, MAX_LOCAL_PART_BYTES, MAX_PLAN_NAME_BYTES,
};
use crate::repo::{
    render_pg as r, AccountStatus, AuthRow, CapabilityGrant, ConfigLayers, ConfigTarget,
    DirectoryRepo, NewAccount, Resolution, GLOBAL_PLAN, SYSTEM_FOLDERS, SYSTEM_FOLDER_TABLE,
};

const UV_MAX_BASE: i64 = u32::MAX as i64 - SYSTEM_FOLDERS;

/// Lock the account row so capability issue / revoke-all / status / password changes of one
/// account serialize. `FOR NO KEY UPDATE` does not block the key-share locks of foreign-key
/// checks (aliases, capabilities) against the account.
async fn lock_account(c: &mut PgConnection, account: AccountId) -> TxResult<bool> {
    let found = sqlx::query_scalar::<_, i64>(&crate::repo::pg_lit("SELECT id FROM accounts WHERE id = $1 FOR NO KEY UPDATE"))
        .bind(account.0)
        .fetch_optional(&mut *c)
        .await?;
    Ok(found.is_some())
}

async fn revoke_all_in(c: &mut PgConnection, account: AccountId, now: Option<i64>) -> TxResult<u64> {
    Ok(sqlx::query(&r(sql::CAP_REVOKE_ALL))
        .bind(now)
        .bind(account.0)
        .execute(&mut *c)
        .await?
        .rows_affected())
}

#[async_trait]
impl DirectoryRepo for PgRepo {
    async fn create_domain(&self, name: &str) -> Result<DomainId> {
        check_text(name, MAX_DOMAIN_BYTES)?;
        let id: i64 = sqlx::query_scalar(&r(sql::INSERT_DOMAIN))
            .bind(name)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(DomainId(id))
    }

    async fn create_plan(&self, name: &str, config: &str) -> Result<PlanId> {
        check_text(name, MAX_PLAN_NAME_BYTES)?;
        check_no_nul(config)?;
        let id: i64 = sqlx::query_scalar(&r(sql::INSERT_PLAN))
            .bind(name)
            .bind(config)
            .fetch_one(self.store.pool())
            .await
            .map_err(map_db_error)?;
        Ok(PlanId(id))
    }

    async fn create_account(&self, new: &NewAccount) -> Result<AccountId> {
        check_text(&new.local, MAX_LOCAL_PART_BYTES)?;
        if let Some(hash) = &new.password_hash {
            check_no_nul(hash)?;
        }
        let ids: Vec<i64> = SYSTEM_FOLDER_TABLE.iter().map(|f| f.0).collect();
        let names: Vec<&str> = SYSTEM_FOLDER_TABLE.iter().map(|f| f.1).collect();
        let roles: Vec<&str> = SYSTEM_FOLDER_TABLE.iter().map(|f| f.2).collect();
        let acc = tx_retry!(self, None, |c| {
            let acc: i64 = sqlx::query_scalar(&r(sql::INSERT_ACCOUNT))
                .bind(&new.local)
                .bind(new.domain.0)
                .bind(new.plan.map(|p| p.0))
                .bind(new.password_hash.as_deref())
                .fetch_one(&mut *c)
                .await?;
            // uid_validity high-water mark = clamped DATABASE clock + 5 (largest value issued).
            let (mb, uv_base): (i64, i64) = sqlx::query_as(
                &crate::repo::pg_lit("INSERT INTO mailboxes (account_id, uid_validity, next_folder_id) \
                 VALUES ($1, LEAST(GREATEST(rs_now(), 1), $2) + $3, $4) \
                 RETURNING id, uid_validity - $3"),
            )
            .bind(acc)
            .bind(UV_MAX_BASE)
            .bind(SYSTEM_FOLDERS)
            .bind(SYSTEM_FOLDERS + 1)
            .fetch_one(&mut *c)
            .await?;
            sqlx::query(
                &crate::repo::pg_lit("INSERT INTO folders (mailbox_id, id, parent_id, name, role, uid_validity) \
                 SELECT $1, t.id, NULL, t.name, t.role, $2 + t.id \
                 FROM unnest($3::bigint[], $4::text[], $5::text[]) AS t(id, name, role)"),
            )
            .bind(mb)
            .bind(uv_base)
            .bind(&ids)
            .bind(&names)
            .bind(&roles)
            .execute(&mut *c)
            .await?;
            Ok(acc)
        })?;
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
        check_no_nul(hash)?;
        tx_retry!(self, None, |c| {
            if !lock_account(&mut *c, account).await? {
                return Err(TxError::Domain(Error::NotFound("account".into())));
            }
            sqlx::query(&r(sql::UPDATE_PASSWORD))
                .bind(hash)
                .bind(account.0)
                .execute(&mut *c)
                .await?;
            revoke_all_in(&mut *c, account, None).await?;
            Ok(())
        })
    }

    async fn set_status(&self, account: AccountId, status: AccountStatus) -> Result<()> {
        tx_retry!(self, None, |c| {
            if !lock_account(&mut *c, account).await? {
                return Err(TxError::Domain(Error::NotFound("account".into())));
            }
            sqlx::query(&r(sql::UPDATE_STATUS))
                .bind(status.as_str())
                .bind(account.0)
                .execute(&mut *c)
                .await?;
            if status != AccountStatus::Active {
                revoke_all_in(&mut *c, account, None).await?;
            }
            Ok(())
        })
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
        check_text(local, MAX_LOCAL_PART_BYTES)?;
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
        check_text(local, MAX_LOCAL_PART_BYTES)?;
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
        if let Some(external) = external {
            check_text(external, MAX_EXTERNAL_MEMBER_BYTES)?;
        }
        tx_retry!(self, None, |c| {
            // Lock the list row first: the count and the per-list `seq` (assigned by the
            // list_members trigger under the same lock) must not interleave. A missing list
            // is not special-cased: the insert fails on the foreign key like on SQLite.
            sqlx::query_scalar::<_, i64>(&crate::repo::pg_lit("SELECT id FROM lists WHERE id = $1 FOR UPDATE"))
                .bind(list.0)
                .fetch_optional(&mut *c)
                .await?;
            let members: i64 = sqlx::query_scalar(&r(sql::COUNT_MEMBERS))
                .bind(list.0)
                .fetch_one(&mut *c)
                .await?;
            check_list_capacity(members, max_members).map_err(TxError::Domain)?;
            sqlx::query(&crate::repo::pg_lit("INSERT INTO list_members (list_id, account_id, external) VALUES ($1, $2, $3)"))
                .bind(list.0)
                .bind(account.map(|a| a.0))
                .bind(external)
                .execute(&mut *c)
                .await?;
            Ok(())
        })
    }

    async fn resolve(
        &self,
        local: &str,
        domain: &str,
        max_members: usize,
    ) -> Result<Option<Resolution>> {
        check_text(local, MAX_LOCAL_PART_BYTES)?;
        check_text(domain, MAX_DOMAIN_BYTES)?;
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
        check_no_nul(config)?;
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
        tx_retry!(self, None, |c| {
            if !lock_account(&mut *c, account).await? {
                return Err(TxError::Domain(Error::NotFound("account".into())));
            }
            let now = match now {
                Some(n) => n,
                None => sqlx::query_scalar(&r(sql::NOW)).fetch_one(&mut *c).await?,
            };
            let expires_at = capability_expiry(now, ttl_secs).map_err(TxError::Domain)?;
            let inserted = sqlx::query(&r(sql::CAP_INSERT))
                .bind(&token_hash[..])
                .bind(role)
                .bind(now)
                .bind(expires_at)
                .bind(account.0)
                .execute(&mut *c)
                .await?
                .rows_affected();
            if inserted == 0 {
                return Err(TxError::Domain(Error::NotFound("account".into())));
            }
            // Keep at most `max_active`: revoke the oldest beyond that. The new row is never a
            // candidate. Ties on `issued_at` (second granularity) are broken by `token_hash`, the
            // same order as on SQLite.
            sqlx::query(
                &crate::repo::pg_lit("UPDATE capabilities SET revoked_at = GREATEST($1, issued_at) WHERE token_hash IN (\
                   SELECT token_hash FROM capabilities \
                   WHERE account_id = $2 AND revoked_at IS NULL AND expires_at > $1 \
                     AND token_hash <> $4 \
                   ORDER BY issued_at, token_hash \
                   LIMIT GREATEST(0, (SELECT count(*) FROM capabilities \
                         WHERE account_id = $2 AND revoked_at IS NULL AND expires_at > $1) - $3))"),
            )
            .bind(now)
            .bind(account.0)
            .bind(max_active)
            .bind(&token_hash[..])
            .execute(&mut *c)
            .await?;
            Ok(())
        })
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
        let n = sqlx::query(&r(sql::CAP_REVOKE_ONE))
            .bind(now)
            .bind(&token_hash[..])
            .execute(self.store.pool())
            .await
            .map_err(map_db_error)?
            .rows_affected();
        Ok(n > 0)
    }

    async fn revoke_all_capabilities(&self, account: AccountId, now: Option<i64>) -> Result<u64> {
        tx_retry!(self, None, |c| {
            lock_account(&mut *c, account).await?;
            revoke_all_in(&mut *c, account, now).await
        })
    }
}
