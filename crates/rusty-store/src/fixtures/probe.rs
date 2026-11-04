//! Counts the SQL statements the driver executes (test-only; production code carries no counter).
//! The driver emits one tracing event per executed statement (BEGIN and COMMIT included) from
//! its connection worker thread, so the counter is process-global and [`StatementProbe::start`]
//! also takes a process-wide lock: every test of a binary that uses it must call `start` first,
//! which serializes them.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, Once};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

static COUNT: AtomicUsize = AtomicUsize::new(0);
static TEXTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static LOCK: Mutex<()> = Mutex::new(());
static INIT: Once = Once::new();
const DRIVER_TARGET: &str = "sqlx::query";

struct Counter;

struct Text {
    summary: Option<String>,
    statement: Option<String>,
}

impl Text {
    fn text(self) -> Option<String> {
        match self.statement {
            Some(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            _ => self.summary,
        }
    }
}

impl tracing::field::Visit for Text {
    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
        match f.name() {
            "db.statement" => self.statement = Some(v.to_owned()),
            "summary" => self.summary = Some(v.to_owned()),
            _ => {}
        }
    }
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        let s = format!("{v:?}");
        let s = s.trim_matches('"').to_owned();
        match f.name() {
            "db.statement" => self.statement = Some(s),
            "summary" => self.summary = Some(s),
            _ => {}
        }
    }
}

impl Subscriber for Counter {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        m.target() == DRIVER_TARGET
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, e: &Event<'_>) {
        if e.metadata().target() == DRIVER_TARGET {
            COUNT.fetch_add(1, Ordering::SeqCst);
            let mut v = Text {
                summary: None,
                statement: None,
            };
            e.record(&mut v);
            if let Some(t) = v.text() {
                TEXTS.lock().unwrap_or_else(|p| p.into_inner()).push(t);
            }
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

pub struct StatementProbe {
    _lock: MutexGuard<'static, ()>,
}

impl StatementProbe {
    /// Take the lock, install the counter (once) and start from zero.
    pub fn start() -> StatementProbe {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        INIT.call_once(|| {
            tracing::subscriber::set_global_default(Counter).expect("global subscriber");
        });
        COUNT.store(0, Ordering::SeqCst);
        TEXTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
        StatementProbe { _lock: lock }
    }

    pub fn count(&self) -> usize {
        COUNT.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        COUNT.store(0, Ordering::SeqCst);
        TEXTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub fn statements(&self) -> Vec<String> {
        TEXTS.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}
