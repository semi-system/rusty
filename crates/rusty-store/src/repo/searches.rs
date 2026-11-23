//! `SearchRepo`: searches of every mailbox for a platform administrator (migration 0049, docs/design-admin-search.md). The admin service creates and reads them; the service that holds the mailboxes
//! (the IMAP service) claims, runs and finishes them; the garbage collector removes what has expired.

use async_trait::async_trait;
use rusty_core::Result;

/// What to look for. Every text is a "contains" (case does not matter) and every text and the dates are optional; the service refuses a search with none of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchCriteria {
    pub domain: Option<String>,
    pub account: Option<String>,
    pub from_text: Option<String>,
    pub to_text: Option<String>,
    pub subject_text: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

/// A search as stored.
#[derive(Debug, Clone)]
pub struct Search {
    pub id: i64,
    pub requested_by: String,
    pub criteria: SearchCriteria,
    /// `queued`, `running`, `done` or `failed`.
    pub state: String,
    pub found: i64,
    pub truncated: bool,
    /// A JSON array of the results (see [`Found`]).
    pub results: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

/// One message that matched: never its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub account: String,
    pub folder: String,
    pub at: i64,
    pub from: String,
    pub subject: String,
    pub size: i64,
}

/// The most results one search keeps.
pub const MAX_FOUND: i64 = 100;

#[async_trait]
pub trait SearchRepo: Send + Sync {
    /// Ask for a search (`queued`); returns its id.
    async fn create(&self, requested_by: &str, criteria: &SearchCriteria) -> Result<i64>;
    async fn get(&self, id: i64) -> Result<Option<Search>>;
    /// The worker takes the oldest queued search (one at a time, whoever asks first): it becomes `running`.
    async fn claim(&self) -> Result<Option<Search>>;
    /// Look in every mailbox for what `criteria` ask: the newest first, at most `limit`, within a time limit of ten seconds. The mail is read here and nowhere else.
    async fn run(&self, criteria: &SearchCriteria, limit: i64) -> Result<Vec<Found>>;
    /// The worker is done: `done` or `failed`, with how many are kept, whether more matched, the results (JSON) and, for a failure, the fixed word of why.
    async fn finish(&self, id: i64, state: &str, found: i64, truncated: bool, results: &str, error: Option<&str>) -> Result<()>;
    /// Remove the searches that have expired; how many, at most `limit`.
    async fn purge_expired(&self, limit: i64) -> Result<u64>;
    /// A search left `running` by a worker that stopped is made `failed` after this many seconds.
    async fn fail_stuck(&self, older_than_secs: i64) -> Result<u64>;
}

pub(crate) mod sql {
    pub const CREATE: &str = "INSERT INTO message_searches (requested_by, domain, account, from_text, to_text, subject_text, since, until, created_at, expires_at) \
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, rs_now(), rs_now() + 86400) RETURNING id";
    pub const GET: &str = "SELECT id, requested_by, domain, account, from_text, to_text, subject_text, since, until, state, found, more_matched, results, error, created_at, finished_at \
        FROM message_searches WHERE id = $1";
    pub const CLAIM: &str = "UPDATE message_searches SET state = 'running' WHERE id = (SELECT id FROM message_searches WHERE state = 'queued' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) \
        RETURNING id, requested_by, domain, account, from_text, to_text, subject_text, since, until, state, found, more_matched, results, error, created_at, finished_at";
    pub const FINISH: &str = "UPDATE message_searches SET state = $2, found = $3, more_matched = $4, results = $5, error = $6, finished_at = rs_now() WHERE id = $1 AND state = 'running'";
    pub const PURGE: &str = "DELETE FROM message_searches WHERE id IN (SELECT id FROM message_searches WHERE expires_at < rs_now() ORDER BY id LIMIT $1)";
    pub const FAIL_STUCK: &str = "UPDATE message_searches SET state = 'failed', error = 'failed', finished_at = rs_now() WHERE state = 'running' AND created_at < rs_now() - $1";
    /// ($1 domain, $2 account address, $3 from, $4 to, $5 subject, $6 since, $7 until, $8 limit.) The texts arrive with `%`, `_` and the escape character already escaped by the caller.
    pub const RUN: &str = "SELECT a.local_part || '@' || d.name, f.name, m.internal_date, m.from_addr, m.subject, m.size \
        FROM messages m \
        JOIN folders f ON f.mailbox_id = m.mailbox_id AND f.id = m.folder_id \
        JOIN mailboxes mb ON mb.id = m.mailbox_id \
        JOIN accounts a ON a.id = mb.account_id \
        JOIN domains d ON d.id = a.domain_id \
        WHERE m.deleted_at IS NULL \
          AND (CAST($1 AS text) IS NULL OR d.name = $1) \
          AND (CAST($2 AS text) IS NULL OR a.local_part || '@' || d.name = $2) \
          AND (CAST($3 AS text) IS NULL OR lower(m.from_addr) LIKE '%' || lower($3) || '%' ESCAPE '\\') \
          AND (CAST($4 AS text) IS NULL OR lower(m.to_addrs) LIKE '%' || lower($4) || '%' ESCAPE '\\') \
          AND (CAST($5 AS text) IS NULL OR lower(m.subject) LIKE '%' || lower($5) || '%' ESCAPE '\\') \
          AND (CAST($6 AS bigint) IS NULL OR m.internal_date >= $6) \
          AND (CAST($7 AS bigint) IS NULL OR m.internal_date <= $7) \
        ORDER BY m.internal_date DESC, m.mailbox_id, m.id LIMIT $8";
}

/// The results as the JSON array that is stored and shown: objects of the fields of [`Found`].
pub fn results_json(rows: &[Found]) -> String {
    let items: Vec<_> = rows.iter().map(|f| serde_json::json!({ "account": f.account, "folder": f.folder, "at": f.at, "from": f.from, "subject": f.subject, "size": f.size })).collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_owned())
}
