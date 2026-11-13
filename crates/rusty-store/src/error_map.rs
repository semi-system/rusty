//! Database error to domain error mapping. Postgres errors are classified by SQLSTATE only, never by
//! message text, and the answers are fixed strings: nothing a caller supplied can leak through them.

use rusty_core::Error;

/// Map a database error to a domain error without leaking raw database text.
/// UNIQUE/PRIMARY KEY -> `Conflict("already exists")`, FK -> `Conflict("referenced row
/// missing or still in use")`, CHECK/NOT NULL -> `Invalid("invalid value")`. The trigger codes
/// chosen by Rusty (`RS001`, `RS002`) are classified by SQLSTATE. Everything else is `Storage`.
pub fn map_db_error(e: sqlx::Error) -> Error {
    // Transient pool conditions first: no database error is involved.
    if matches!(e, sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed) {
        return busy(BUSY_POOL);
    }
    if let sqlx::Error::Database(db) = &e {
        // Postgres trigger codes are chosen by Rusty and classified only by SQLSTATE. Never
        // inspect Postgres message text: caller data may contain strings resembling a code.
        match db.code().as_deref() {
            Some("RS001") => return Error::Conflict("database conflict".into()),
            Some("RS002") => return Error::Invalid("invalid value".into()),
            // ON DELETE RESTRICT raises restrict_violation, not foreign_key_violation (the driver
            // does not classify it): the same answer as a foreign key failure.
            Some("23001") => {
                return Error::Conflict("referenced row missing or still in use".into());
            }
            // Values the database cannot store or index: 54000 index row too large, 22021 NUL in
            // text, 22003 numeric overflow, 22001 value too long, 22P05 untranslatable character.
            // The repositories validate such input first (repo::directory::check_text); this is
            // the defence in depth, so a lax caller gets a validation error, never `Storage`.
            Some("54000") | Some("22021") | Some("22003") | Some("22001") | Some("22P05") => {
                return Error::Invalid("invalid value".into());
            }
            // read_only_sql_transaction: a write reached a standby (failover in progress). The
            // statement did not happen and a retry reaches the new primary: `Busy`.
            Some("25006") => return busy("database read-only"),
            // Transient classes (D1). 40P01/40001 reach this function only after the retry loop
            // gave up (or outside it): the caller may retry later.
            Some("55P03") => return busy("lock timeout"),
            Some("57014") => return busy("statement timeout"),
            Some("53300") => return busy("too many connections"),
            Some("40P01") | Some("40001") => return busy("transaction conflict"),
            _ => {}
        }
        use sqlx::error::ErrorKind;
        match db.kind() {
            ErrorKind::UniqueViolation => return Error::Conflict("already exists".into()),
            ErrorKind::ForeignKeyViolation => {
                return Error::Conflict("referenced row missing or still in use".into());
            }
            ErrorKind::CheckViolation | ErrorKind::NotNullViolation => {
                return Error::Invalid("invalid value".into());
            }
            _ => {}
        }
    }
    Error::storage_with("database error", e)
}

/// Fixed reason for pool-acquire timeout / closed pool.
pub(crate) const BUSY_POOL: &str = "connection pool unavailable";

/// A transient failure with a fixed, caller-data-free reason (D1).
pub(crate) fn busy(reason: &str) -> Error {
    Error::Busy(reason.to_owned())
}
