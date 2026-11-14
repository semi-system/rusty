//! Backend-neutral repository traits at OPERATION granularity (decision D3).
//!
//! - `directory`: [`DirectoryRepo`] (domains, accounts, aliases, lists, capabilities, config).
//! - `mailbox`: [`MailboxRepo`] (folders, messages, batches, deliveries, change log).
//!
//! The facades (`Directory`, `Mailbox`) own everything that does not talk to a database:
//! validation, limits, MIME parsing, blob IO ordering ("blob before metadata transaction"),
//! paging limits and result shaping. They hold `Arc<dyn Repo>`.
//!
//! A repository method is ONE unit of work that owns its transaction. The caller passes owned or
//! borrowed plain data and gets plain data back. No transaction handle crosses the trait, which
//! is what makes the Postgres retry loop (40P01/40001) possible: a whole operation can be re-run
//! because nothing outside the repository observes a half-finished attempt.
//!
//! Statement text that is identical up to placeholder syntax is shared (`sql_text`, `directory::sql`);
//! the Rust glue is Postgres. Placeholders are `$N` and
//! `{NOW}` is the database clock (`rs_now()`).

mod directory;
mod gc;
mod dns;
pub(crate) mod jobs;
mod mailbox;
#[allow(clippy::needless_borrow)] // statement literals go through the mutation hook (see mutation.rs)
pub mod pg;

pub use dns::{DkimKeyRow, DnsConfig, DnsConfigRow, DnsEventRow, DnsRepo};
pub use gc::{GcRepo, PurgeBatch};
pub use jobs::{BatchJob, BatchRequest, ClaimRequest, JobRepo, NewJob, OwnerLimits};
pub use directory::{
    AccountInfo, AccountStatus, AliasInfo, AuthRow, CapabilityGrant, ConfigLayers, ConfigTarget,
    DirectoryRepo, DomainInfo, DomainStatus, ListMember, NewAccount, Resolution, GLOBAL_PLAN,
    MAX_LIST_PAGE, MAX_STATUS_IDS,
};
pub use mailbox::{
    sql_text, Committed, DeliverEntry, Delivery, FlagUpdate, FolderCounts, FolderInfo, JobFence, MailboxRepo, MessageDetail,
    NewMessage, RawRef,
};
pub(crate) use mailbox::{
    changes_since_generic, flag_update, counts_from_row, folder_from_row, FolderRow, check_batch_len, check_distinct_ids, check_purge_limit, depth_verdict, moved_refs, not_found_folder,
    not_found_mailbox, not_found_message, plan_deliveries, plan_move, split_hits,
    summary_from_row, thread_candidates, Slot, SummaryRow,
};

/// Number of system folders created with every mailbox (folder ids 1..=SYSTEM_FOLDERS).
pub const SYSTEM_FOLDERS: i64 = 5;

/// `(id, name, role)` of the system folders. Same on every backend.
pub const SYSTEM_FOLDER_TABLE: [(i64, &str, &str); 5] = [
    (1, "Inbox", "inbox"),
    (2, "Sent", "sent"),
    (3, "Drafts", "drafts"),
    (4, "Trash", "trash"),
    (5, "Spam", "spam"),
];

/// Blob hash as the 32 raw bytes Postgres stores. `BlobHash` is validated lowercase hex.
pub(crate) fn hash_bytes(h: &rusty_core::BlobHash) -> rusty_core::Result<Vec<u8>> {
    hex::decode(h.as_str()).map_err(|_| rusty_core::Error::Invalid("invalid blob hash".into()))
}

/// The text of a Postgres statement: the database clock is `rs_now()` (`statement_timestamp()`).
pub(crate) fn render_text(sql: &str) -> String {
    let s = sql.replace("{NOW}", "rs_now()").replace("{JSONB}", "::jsonb");
    #[cfg(feature = "mutation-testing")]
    let s = crate::mutation::rendered(crate::mutation::Dialect::Postgres, s);
    s
}

/// A Postgres statement for sqlx. sqlx 0.9 refuses any SQL that is not a `'static` literal unless it is
/// wrapped in `AssertSqlSafe`; this is the single place where that assertion is made for rendered
/// text, and the `&'static str` parameter is what makes it true: only a literal in the source can
/// be passed in, so no runtime value can reach the statement text (values go through `bind`).
pub(crate) fn render_pg(sql: &'static str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(render_text(sql))
}

/// A Postgres-only statement literal. Identity in every build except `mutation-testing`, where
/// the active text mutation (src/mutation.rs) may rewrite it.
#[cfg(feature = "mutation-testing")]
pub(crate) fn pg_lit(sql: &'static str) -> sqlx::AssertSqlSafe<std::borrow::Cow<'static, str>> {
    // Same argument as `render_pg`: the input is a literal of the source, the mutation only rewrites it.
    sqlx::AssertSqlSafe(crate::mutation::statement(crate::mutation::Dialect::Postgres, sql))
}
#[cfg(not(feature = "mutation-testing"))]
#[allow(dead_code)]
#[inline(always)]
pub(crate) fn pg_lit(sql: &'static str) -> &'static str {
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_only_the_clock_token() {
        assert_eq!(
            render_text("SELECT 1 WHERE a = $1 AND t = {NOW}"),
            "SELECT 1 WHERE a = $1 AND t = rs_now()"
        );
        for s in [
            sql_text::FOLDER_EXISTS,
            directory::sql::RESOLVE_KIND,
            directory::sql::CAP_REVOKE_ONE,
            directory::sql::INSERT_PLAN,
            sql_text::ANCESTORS,
            sql_text::SUBTREE_HEIGHT,
            sql_text::TAKE_MSG_IDS_AND_SEQS,
            sql_text::EXPUNGE,
            sql_text::LIST_FOLDER,
            sql_text::CHANGES,
            directory::sql::INSERT_ACCOUNT,
        ] {
            assert!(!s.contains('?'), "{s}");
            assert!(!render_text(s).contains("{NOW}") && !render_text(s).contains("{JSONB}"), "{s}");
        }
    }
}
