//! The heartbeat service: it beats at once and then at its interval, never takes the service down when the database fails, and stops on shutdown.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{dev_config, parts, Parts};
use rusty_core::{Error, Result};
use rusty_store::{HeartbeatRepo, HeartbeatRow};
use rusty_svc::{Context, HeartbeatService, Service, TlsProvider};
use tokio_util::sync::CancellationToken;

type Beat = (String, String, String, i64, String, String);

/// Records every beat; the first `fail` writes fail.
#[derive(Default)]
struct Recorder {
    beats: Mutex<Vec<Beat>>,
    fail: Mutex<usize>,
}

#[async_trait]
impl HeartbeatRepo for Recorder {
    async fn beat(&self, service: &str, instance: &str, version: &str, uptime_secs: i64, status: &str, detail: &str) -> Result<()> {
        {
            let mut fail = self.fail.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(Error::storage("the database is down"));
            }
        }
        self.beats.lock().unwrap().push((service.into(), instance.into(), version.into(), uptime_secs, status.into(), detail.into()));
        Ok(())
    }
    async fn list(&self) -> Result<Vec<HeartbeatRow>> {
        Ok(Vec::new())
    }
    async fn prune(&self, _older_than_secs: i64) -> Result<u64> {
        Ok(0)
    }
    async fn storage_check_counts(&self) -> Result<Vec<(String, i64)>> {
        Ok(Vec::new())
    }
    async fn dns_problem_domains(&self) -> Result<i64> {
        Ok(0)
    }
}

async fn ctx(p: &Parts) -> Context {
    let cfg = Arc::new(dev_config(p.dir.path()));
    let tls = TlsProvider::from_config(&cfg.tls, "localhost", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    Context::new(p.store.repos(), p.blobs.clone(), cfg, tls).await.unwrap()
}

#[tokio::test]
async fn it_beats_at_once_then_every_interval_and_stops_on_shutdown() {
    let p = parts().await;
    let rec = Arc::new(Recorder::default());
    let svc = HeartbeatService::new(rec.clone(), "imap", "9.9.9").with_detail("{\"listeners\":2}".into()).with_every(Duration::from_millis(30));
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(Box::new(svc).run(ctx(&p).await, shutdown.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), task).await.expect("it stops on shutdown").unwrap().unwrap();
    let beats = rec.beats.lock().unwrap().clone();
    assert!(beats.len() >= 3, "a beat at once and one at each interval: {}", beats.len());
    assert_eq!((beats[0].0.as_str(), beats[0].2.as_str(), beats[0].4.as_str(), beats[0].5.as_str()), ("imap", "9.9.9", "ok", "{\"listeners\":2}"));
    assert_eq!(beats[0].3, 0, "the first beat is at the start");
    assert!(!beats[0].1.is_empty(), "an instance name");
    assert!(beats.windows(2).all(|w| w[0].3 <= w[1].3), "the uptime never goes back");
    let after = rec.beats.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rec.beats.lock().unwrap().len(), after, "no beat after shutdown");
}

#[tokio::test]
async fn a_failing_database_does_not_stop_the_service_and_it_recovers() {
    let p = parts().await;
    let rec = Arc::new(Recorder::default());
    *rec.fail.lock().unwrap() = 4;
    let svc = HeartbeatService::new(rec.clone(), "gc", "1").with_every(Duration::from_millis(20));
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(Box::new(svc).run(ctx(&p).await, shutdown.clone()));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!task.is_finished(), "a failing write is not a failing service");
    assert!(!rec.beats.lock().unwrap().is_empty(), "it beats again when the database is back");
    shutdown.cancel();
    task.await.unwrap().unwrap();
}
