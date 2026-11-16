//! `HeartbeatRepo`: the heartbeats of the running services (migration 0027, docs/design-admin-monitor.md). A service writes its own row through `rs_heartbeat()` (the database derives
//! the service name from the role, so a service cannot write another one's row), the admin web reads them all, the garbage collector removes the ones nobody renews.
//! Every time in a row is the database clock's: the age is computed by the database, never from a service's own clock.

use async_trait::async_trait;
use rusty_core::Result;

/// One heartbeat as the admin web reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatRow {
    pub service: String,
    /// The container's host name: every replica has its own row.
    pub instance: String,
    pub version: String,
    /// Seconds since the process started, by the database clock at the time of the read.
    pub uptime_secs: i64,
    /// Seconds since the last beat, by the database clock at the time of the read.
    pub age_secs: i64,
    /// `ok` or `degraded`.
    pub status: String,
    /// A small JSON object of fixed keys (at most 2 KiB), written by the service.
    pub detail: String,
}

#[async_trait]
pub trait HeartbeatRepo: Send + Sync {
    /// Renew the heartbeat of one instance. `service` is used only by a login that is not a service role (the operator's role, the development setup, tests): a service role writes its own
    /// service whatever it passes.
    async fn beat(&self, service: &str, instance: &str, version: &str, uptime_secs: i64, status: &str, detail: &str) -> Result<()>;
    /// Every heartbeat, by service and instance.
    async fn list(&self) -> Result<Vec<HeartbeatRow>>;
    /// Remove the heartbeats not renewed for `older_than_secs`; how many.
    async fn prune(&self, older_than_secs: i64) -> Result<u64>;
    /// The monitor's other two figures: how many blob stores of domains are in each check state (`unchecked`, `ok`, `failed`, ...), by state name.
    async fn storage_check_counts(&self) -> Result<Vec<(String, i64)>>;
    /// How many domains have a DNS record that is `missing` or `mismatch`.
    async fn dns_problem_domains(&self) -> Result<i64>;
}

pub(crate) mod sql {
    pub const BEAT: &str = "SELECT rs_heartbeat($1, $2, $3, $4, $5, $6)";
    pub const LIST: &str = "SELECT service, instance, version, rs_now() - started_at, rs_now() - last_seen, status, detail \
        FROM service_heartbeats ORDER BY service, instance";
    pub const STORAGE_CHECKS: &str = "SELECT check_status, CAST(count(*) AS BIGINT) FROM blob_stores WHERE domain_id IS NOT NULL GROUP BY check_status ORDER BY check_status";
    pub const DNS_PROBLEMS: &str = "SELECT CAST(count(DISTINCT domain_id) AS BIGINT) FROM domain_dns_state WHERE last_status IN ('missing', 'mismatch')";
    pub const PRUNE: &str = "DELETE FROM service_heartbeats WHERE last_seen < rs_now() - $1";
}
