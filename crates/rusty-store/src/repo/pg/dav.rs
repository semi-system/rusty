//! `DavRepo` on Postgres. A write of an object locks the row of its collection for the length of one transaction, so the revision of a collection counts its changes without a gap or a repeat.

use async_trait::async_trait;
use rusty_core::{AccountId, Error, Result};
use sha2::{Digest, Sha256};
use sqlx::Row;

use super::PgRepo;
use crate::map_db_error;
use crate::repo::dav::{
    sql, Changes, Collection, DavRepo, DeleteResult, NewObject, Object, Precondition, PutResult, MAX_COLLECTIONS_PER_ACCOUNT, MAX_OBJECTS_PER_ACCOUNT, MAX_OBJECTS_PER_COLLECTION, MAX_OBJECT_BYTES,
    TOMBSTONE_KEEP_SECS,
};
use crate::repo::render_pg as r;

fn collection(x: &sqlx::postgres::PgRow) -> std::result::Result<Collection, sqlx::Error> {
    Ok(Collection {
        id: x.try_get(0)?,
        kind: x.try_get(1)?,
        name: x.try_get(2)?,
        display_name: x.try_get(3)?,
        description: x.try_get(4)?,
        color: x.try_get(5)?,
        revision: x.try_get(6)?,
        created_at: x.try_get(7)?,
        updated_at: x.try_get(8)?,
    })
}

fn object(x: &sqlx::postgres::PgRow) -> std::result::Result<Object, sqlx::Error> {
    Ok(Object {
        name: x.try_get(0)?,
        uid: x.try_get(1)?,
        etag: x.try_get(2)?,
        body: x.try_get(3)?,
        size: i64::from(x.try_get::<i32, _>(4)?),
        component: x.try_get(5)?,
        starts_at: x.try_get(6)?,
        ends_at: x.try_get(7)?,
        recurring: x.try_get(8)?,
        revision: x.try_get(9)?,
        updated_at: x.try_get(10)?,
    })
}

/// The entity tag of an object: a hash of its text, so the same text is the same tag everywhere.
fn etag_of(body: &str) -> String {
    hex::encode(Sha256::digest(body.as_bytes())).chars().take(32).collect()
}

fn kind_ok(kind: &str) -> Result<()> {
    if kind == "calendar" || kind == "addressbook" {
        Ok(())
    } else {
        Err(Error::Invalid("collection kind".into()))
    }
}

#[async_trait]
impl DavRepo for PgRepo {
    async fn collections(&self, account: AccountId, kind: &str) -> Result<Vec<Collection>> {
        let rows = sqlx::query(r(sql::COLLECTIONS)).bind(account.0).bind(kind).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(collection).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn collection(&self, account: AccountId, kind: &str, name: &str) -> Result<Option<Collection>> {
        let row = sqlx::query(r(sql::COLLECTION)).bind(account.0).bind(kind).bind(name).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        row.as_ref().map(collection).transpose().map_err(map_db_error)
    }

    async fn ensure_defaults(&self, account: AccountId) -> Result<()> {
        let any: bool = sqlx::query_scalar(r(sql::ANY_COLLECTION)).bind(account.0).fetch_one(self.store.pool()).await.map_err(map_db_error)?;
        if any {
            return Ok(());
        }
        for (kind, display) in [("calendar", "Calendar"), ("addressbook", "Contacts")] {
            sqlx::query(r(sql::CREATE_COLLECTION)).bind(account.0).bind(kind).bind("default").bind(display).bind("").fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        }
        Ok(())
    }

    async fn create_collection(&self, account: AccountId, kind: &str, name: &str, display_name: &str, description: &str) -> Result<Collection> {
        kind_ok(kind)?;
        let pool = self.store.pool();
        let have: i64 = sqlx::query_scalar(r(sql::COUNT_COLLECTIONS)).bind(account.0).fetch_one(pool).await.map_err(map_db_error)?;
        if have >= MAX_COLLECTIONS_PER_ACCOUNT {
            return Err(Error::Quota("collections".into()));
        }
        match sqlx::query(r(sql::CREATE_COLLECTION)).bind(account.0).bind(kind).bind(name).bind(display_name).bind(description).fetch_optional(pool).await {
            Ok(Some(row)) => collection(&row).map_err(map_db_error),
            Ok(None) => Err(Error::Conflict("the collection exists".into())),
            // A name or a text the table's checks refuse is the caller's error, not the database's.
            Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23514") => Err(Error::Invalid("collection".into())),
            Err(e) => Err(map_db_error(e)),
        }
    }

    async fn update_collection(&self, account: AccountId, kind: &str, name: &str, display_name: Option<&str>, description: Option<&str>, color: Option<&str>) -> Result<bool> {
        match sqlx::query(r(sql::UPDATE_COLLECTION)).bind(account.0).bind(kind).bind(name).bind(display_name).bind(description).bind(color).execute(self.store.pool()).await {
            Ok(done) => Ok(done.rows_affected() > 0),
            Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23514") => Err(Error::Invalid("collection".into())),
            Err(e) => Err(map_db_error(e)),
        }
    }

    async fn delete_collection(&self, account: AccountId, kind: &str, name: &str) -> Result<bool> {
        Ok(sqlx::query(r(sql::DELETE_COLLECTION)).bind(account.0).bind(kind).bind(name).execute(self.store.pool()).await.map_err(map_db_error)?.rows_affected() > 0)
    }

    async fn object(&self, collection: i64, name: &str) -> Result<Option<Object>> {
        let row = sqlx::query(r(sql::OBJECT)).bind(collection).bind(name).fetch_optional(self.store.pool()).await.map_err(map_db_error)?;
        row.as_ref().map(object).transpose().map_err(map_db_error)
    }

    async fn objects(&self, collection: i64, limit: i64) -> Result<Vec<Object>> {
        let rows = sqlx::query(r(sql::OBJECTS)).bind(collection).bind(limit).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(object).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn objects_named(&self, collection: i64, names: &[String]) -> Result<Vec<Object>> {
        let rows = sqlx::query(r(sql::OBJECTS_NAMED)).bind(collection).bind(names).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(object).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn objects_in_range(&self, collection: i64, component: Option<&str>, from: Option<i64>, to: Option<i64>, limit: i64) -> Result<Vec<Object>> {
        let rows = sqlx::query(r(sql::OBJECTS_RANGE)).bind(collection).bind(component).bind(from).bind(to).bind(limit).fetch_all(self.store.pool()).await.map_err(map_db_error)?;
        rows.iter().map(object).collect::<std::result::Result<Vec<_>, _>>().map_err(map_db_error)
    }

    async fn put(&self, collection: i64, new: &NewObject, pre: &Precondition) -> Result<PutResult> {
        if new.body.len() > MAX_OBJECT_BYTES {
            return Err(Error::Invalid("object too large".into()));
        }
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        // The collection row is the lock of its writers; a collection that is gone ends the write.
        if sqlx::query_scalar::<_, i64>(r(sql::LOCK)).bind(collection).fetch_optional(&mut *tx).await.map_err(map_db_error)?.is_none() {
            return Err(Error::NotFound("collection".into()));
        }
        let current: Option<String> = sqlx::query_scalar(r(sql::ETAG_OF)).bind(collection).bind(&new.name).fetch_optional(&mut *tx).await.map_err(map_db_error)?;
        match pre {
            Precondition::None => {}
            Precondition::Absent if current.is_some() => return Ok(PutResult::PreconditionFailed),
            Precondition::Absent => {}
            Precondition::Etag(e) if current.as_deref() != Some(e.as_str()) => return Ok(PutResult::PreconditionFailed),
            Precondition::Etag(_) => {}
        }
        if let Some(other) = sqlx::query_scalar::<_, String>(r(sql::UID_OWNER)).bind(collection).bind(&new.uid).bind(&new.name).fetch_optional(&mut *tx).await.map_err(map_db_error)? {
            return Ok(PutResult::UidConflict(other));
        }
        if current.is_none() {
            let in_collection: i64 = sqlx::query_scalar(r(sql::COUNT_IN_COLLECTION)).bind(collection).fetch_one(&mut *tx).await.map_err(map_db_error)?;
            if in_collection >= MAX_OBJECTS_PER_COLLECTION {
                return Ok(PutResult::Full("objects"));
            }
            let account: i64 = sqlx::query_scalar(r(sql::ACCOUNT_OF)).bind(collection).fetch_one(&mut *tx).await.map_err(map_db_error)?;
            let in_account: i64 = sqlx::query_scalar(r(sql::COUNT_IN_ACCOUNT)).bind(account).fetch_one(&mut *tx).await.map_err(map_db_error)?;
            if in_account >= MAX_OBJECTS_PER_ACCOUNT {
                return Ok(PutResult::Full("account objects"));
            }
        }
        let revision: i64 = sqlx::query_scalar(r(sql::BUMP)).bind(collection).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        let etag = etag_of(&new.body);
        let size = i32::try_from(new.body.len()).unwrap_or(i32::MAX);
        match sqlx::query(r(sql::UPSERT))
            .bind(collection)
            .bind(&new.name)
            .bind(&new.uid)
            .bind(&etag)
            .bind(&new.body)
            .bind(size)
            .bind(&new.component)
            .bind(new.starts_at)
            .bind(new.ends_at)
            .bind(new.recurring)
            .bind(revision)
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {}
            Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23514") => return Err(Error::Invalid("object".into())),
            Err(e) => return Err(map_db_error(e)),
        }
        sqlx::query(r(sql::DROP_TOMBSTONE)).bind(collection).bind(&new.name).execute(&mut *tx).await.map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(if current.is_some() { PutResult::Updated { etag, revision } } else { PutResult::Created { etag, revision } })
    }

    async fn delete(&self, collection: i64, name: &str, pre: &Precondition) -> Result<DeleteResult> {
        let mut tx = self.store.pool().begin().await.map_err(map_db_error)?;
        if sqlx::query_scalar::<_, i64>(r(sql::LOCK)).bind(collection).fetch_optional(&mut *tx).await.map_err(map_db_error)?.is_none() {
            return Ok(DeleteResult::NotFound);
        }
        let current: Option<String> = sqlx::query_scalar(r(sql::ETAG_OF)).bind(collection).bind(name).fetch_optional(&mut *tx).await.map_err(map_db_error)?;
        let Some(current) = current else { return Ok(DeleteResult::NotFound) };
        if let Precondition::Etag(e) = pre {
            if *e != current {
                return Ok(DeleteResult::PreconditionFailed);
            }
        }
        if matches!(pre, Precondition::Absent) {
            return Ok(DeleteResult::PreconditionFailed);
        }
        let revision: i64 = sqlx::query_scalar(r(sql::BUMP)).bind(collection).fetch_one(&mut *tx).await.map_err(map_db_error)?;
        sqlx::query(r(sql::DELETE_OBJECT)).bind(collection).bind(name).execute(&mut *tx).await.map_err(map_db_error)?;
        sqlx::query(r(sql::TOMBSTONE)).bind(collection).bind(name).bind(revision).execute(&mut *tx).await.map_err(map_db_error)?;
        tx.commit().await.map_err(map_db_error)?;
        Ok(DeleteResult::Deleted { revision })
    }

    async fn changes_since(&self, collection: i64, since: i64, limit: i64) -> Result<Option<Changes>> {
        let pool = self.store.pool();
        let Some(row) = sqlx::query(r(sql::REVISION_AND_FLOOR)).bind(collection).fetch_optional(pool).await.map_err(map_db_error)? else {
            return Err(Error::NotFound("collection".into()));
        };
        let (revision, floor): (i64, i64) = (row.try_get(0).map_err(map_db_error)?, row.try_get(1).map_err(map_db_error)?);
        // A token of a revision that came before the collected tombstones cannot be answered with what was deleted (a first sync, token 0, does not ask for it).
        if since != 0 && since < floor {
            return Ok(None);
        }
        let rows = sqlx::query(r(sql::CHANGED)).bind(collection).bind(since).bind(limit.saturating_add(1)).fetch_all(pool).await.map_err(map_db_error)?;
        let mut changed: Vec<(String, String, i64)> = rows
            .iter()
            .map(|x| Ok((x.try_get::<String, _>(0)?, x.try_get::<String, _>(1)?, x.try_get::<i64, _>(2)?)))
            .collect::<std::result::Result<_, sqlx::Error>>()
            .map_err(map_db_error)?;
        let truncated = i64::try_from(changed.len()).unwrap_or(i64::MAX) > limit;
        if truncated {
            changed.truncate(usize::try_from(limit).unwrap_or(0));
        }
        let token = if truncated { changed.last().map_or(since, |c| c.2) } else { changed.last().map_or(revision, |c| c.2.max(revision)) };
        let deleted: Vec<String> = sqlx::query_scalar(r(sql::DELETED)).bind(collection).bind(since).bind(token).fetch_all(pool).await.map_err(map_db_error)?;
        Ok(Some(Changes { revision: token, changed: changed.into_iter().map(|(n, e, _)| (n, e)).collect(), deleted, truncated }))
    }

    async fn purge_tombstones(&self) -> Result<u64> {
        let n: i64 = sqlx::query_scalar(r(sql::PURGE)).bind(TOMBSTONE_KEEP_SECS).fetch_one(self.store.pool()).await.map_err(map_db_error)?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
}
