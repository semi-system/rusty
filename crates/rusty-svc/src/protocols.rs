//! The platform's protocol switches as a service reads them (migration 0030, docs/design-protocols.md): "is POP3 on?" answered from a copy of the table that is renewed at most every
//! few seconds, so a connection costs no query and a change made in the admin web reaches every replica within [`REFRESH`]. A protocol with no stored value is on. When the database
//! cannot be read the last known answer stands (on, before any read): a database hiccup must not close a protocol that was open.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusty_store::SettingsRepo;
use tokio::sync::Mutex;

/// How old the copy may be before the next question renews it.
pub const REFRESH: Duration = Duration::from_secs(5);

struct Cache {
    at: Option<Instant>,
    values: HashMap<String, bool>,
}

#[derive(Clone)]
pub struct ProtocolSwitches {
    repo: Arc<dyn SettingsRepo>,
    cache: Arc<Mutex<Cache>>,
}

impl ProtocolSwitches {
    pub fn new(repo: Arc<dyn SettingsRepo>) -> ProtocolSwitches {
        ProtocolSwitches { repo, cache: Arc::new(Mutex::new(Cache { at: None, values: HashMap::new() })) }
    }

    /// Whether the protocol (`pop3`) answers connections now.
    pub async fn enabled(&self, protocol: &str) -> bool {
        let mut c = self.cache.lock().await;
        if c.at.is_none_or(|t| t.elapsed() >= REFRESH) {
            match self.repo.protocols().await {
                Ok(rows) => c.values = rows.into_iter().collect(),
                Err(e) => tracing::warn!(error = %e, "the protocol switches could not be read; the last answer stands"),
            }
            c.at = Some(Instant::now());
        }
        c.values.get(protocol).copied().unwrap_or(true)
    }
}
