//! `RoutingRepo` on Postgres: single statements, atomic by themselves.

use async_trait::async_trait;
use rusty_core::{DomainId, Error, ListId, Result};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::render_pg as r;
use crate::repo::routing::{sql, ListInfo, MemberInfo, RoutingRepo};

#[async_trait]
impl RoutingRepo for PgRepo {
    async fn lists(&self, domain: DomainId) -> Result<Vec<ListInfo>> {
        let rows = sqlx::query(r(sql::LISTS)).bind(domain.0).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(ListInfo { id: ListId(x.try_get(0)?), local: x.try_get(1)?, members: x.try_get(2)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn find_list(&self, domain: DomainId, local: &str) -> Result<Option<ListInfo>> {
        let row = sqlx::query(r(sql::FIND_LIST)).bind(domain.0).bind(local).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        row.map(|x| Ok(ListInfo { id: ListId(x.try_get(0)?), local: x.try_get(1)?, members: x.try_get(2)? })).transpose().map_err(map_db_error)
    }

    async fn delete_list(&self, domain: DomainId, local: &str) -> Result<()> {
        let n = sqlx::query(r(sql::DELETE_LIST)).bind(domain.0).bind(local).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("list".into()));
        }
        Ok(())
    }

    async fn members(&self, list: ListId) -> Result<Vec<MemberInfo>> {
        let rows = sqlx::query(r(sql::MEMBERS)).bind(list.0).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter()
            .map(|x| Ok(MemberInfo { seq: x.try_get(0)?, address: x.try_get(1)?, external: x.try_get(2)? }))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
            .map_err(map_db_error)
    }

    async fn remove_member(&self, list: ListId, seq: i64) -> Result<()> {
        let n = sqlx::query(r(sql::REMOVE_MEMBER)).bind(list.0).bind(seq).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("member".into()));
        }
        Ok(())
    }

    async fn catch_all(&self, domain: DomainId) -> Result<Option<String>> {
        let v: Option<Option<String>> = sqlx::query_scalar(r(sql::CATCH_ALL)).bind(domain.0).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        Ok(v.flatten())
    }

    async fn set_catch_all(&self, domain: DomainId, local: Option<&str>) -> Result<()> {
        sqlx::query(r(sql::SET_CATCH_ALL)).bind(domain.0).bind(local).execute(self.store.pool()).await.map_err(map_db_error)?;
        Ok(())
    }

    async fn domain_aliases(&self, domain: DomainId) -> Result<Vec<String>> {
        sqlx::query_scalar(r(sql::DOMAIN_ALIASES)).bind(domain.0).fetch_all(self.store.pool()).await.map_err(map_db_error)
    }

    async fn add_domain_alias(&self, domain: DomainId, name: &str) -> Result<()> {
        let n = sqlx::query(r(sql::ADD_DOMAIN_ALIAS)).bind(domain.0).bind(name).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        if n == 0 {
            return Err(Error::Conflict("this name is already a domain or an alias".into()));
        }
        Ok(())
    }

    async fn remove_domain_alias(&self, domain: DomainId, name: &str) -> Result<()> {
        let n = sqlx::query(r(sql::REMOVE_DOMAIN_ALIAS)).bind(domain.0).bind(name).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("domain alias".into()));
        }
        Ok(())
    }
}
