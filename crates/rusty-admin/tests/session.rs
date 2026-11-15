//! Sessions of the admin web over the in-process `Kv`: ids, idle and absolute timeouts, revocation of an account, failing closed.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_admin::{ManualNow, SessionConfig, SessionStore};
use rusty_core::{AccountId, DomainId, Error, Result};
use rusty_valkey::{Kv, ManualClock, MemoryKv, WindowOutcome};

struct Rig {
    store: SessionStore,
    kv: Arc<MemoryKv>,
    kv_clock: Arc<ManualClock>,
    now: Arc<ManualNow>,
}

impl Rig {
    fn new() -> Rig {
        let kv_clock = Arc::new(ManualClock::new());
        let kv = Arc::new(MemoryKv::with_clock(kv_clock.clone()));
        let now = Arc::new(ManualNow::new(1_000_000));
        let store = SessionStore::new(kv.clone(), SessionConfig::default()).with_clock(now.clone());
        Rig { store, kv, kv_clock, now }
    }

    fn advance(&self, d: Duration) {
        self.kv_clock.advance(d);
        self.now.advance(d);
    }
}

const MIN: Duration = Duration::from_secs(60);

#[tokio::test]
async fn a_new_session_has_a_random_id_and_csrf_token_and_the_grant_it_was_made_with() {
    let r = Rig::new();
    let (id, s) = r.store.create(AccountId(7), Some(DomainId(3))).await.unwrap();
    assert_eq!(id.len(), 64);
    assert!(id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert_eq!((s.account, s.domain), (7, Some(3)));
    assert_eq!(s.csrf.len(), 64);
    assert_ne!(id, s.csrf, "the CSRF token is not the session id");
    let (id2, s2) = r.store.create(AccountId(7), None).await.unwrap();
    assert_ne!(id, id2, "a sign-in never reuses an id");
    assert_ne!(s.csrf, s2.csrf);
    assert_eq!(s2.domain, None);
    let got = r.store.get(&id).await.unwrap().unwrap();
    assert_eq!((got.account, got.domain, got.csrf.as_str()), (7, Some(3), s.csrf.as_str()));
}

#[tokio::test]
async fn only_the_hash_of_the_id_is_stored() {
    let r = Rig::new();
    let (id, _) = r.store.create(AccountId(1), None).await.unwrap();
    // The id itself is not a key anywhere, under any of the obvious spellings.
    for k in [id.clone(), format!("admin:sess:{id}"), format!("sess:{id}")] {
        assert!(r.kv.get(&k).await.unwrap().is_none(), "{k}");
    }
    let hashed = format!("admin:sess:{}", hex_sha256(&id));
    let raw = r.kv.get(&hashed).await.unwrap().expect("stored under the hash");
    assert!(!String::from_utf8_lossy(&raw).contains(&id), "the record does not hold the id either");
}

fn hex_sha256(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn malformed_ids_are_no_session() {
    let r = Rig::new();
    let (id, _) = r.store.create(AccountId(1), None).await.unwrap();
    for bad in ["", "x", &id[..63], &format!("{id}0"), &id.to_uppercase(), &format!("{}g", &id[..63]), "../../etc/passwd", &"a".repeat(10_000)] {
        assert!(r.store.get(bad).await.unwrap().is_none(), "{bad:?}");
    }
}

#[tokio::test]
async fn an_idle_session_ends_after_thirty_minutes_and_use_renews_it() {
    let r = Rig::new();
    let (id, _) = r.store.create(AccountId(1), None).await.unwrap();
    r.advance(29 * MIN);
    assert!(r.store.get(&id).await.unwrap().is_some(), "29 minutes: still there (and renewed)");
    r.advance(29 * MIN);
    assert!(r.store.get(&id).await.unwrap().is_some(), "29 more minutes after the renewal");
    r.advance(31 * MIN);
    assert!(r.store.get(&id).await.unwrap().is_none(), "31 idle minutes: gone");
    assert!(r.store.get(&id).await.unwrap().is_none(), "and it stays gone");
}

#[tokio::test]
async fn a_session_ends_eight_hours_after_sign_in_however_busy_it_is() {
    let r = Rig::new();
    let (id, _) = r.store.create(AccountId(1), None).await.unwrap();
    for _ in 0..15 {
        r.advance(29 * MIN);
        assert!(r.store.get(&id).await.unwrap().is_some());
    }
    // 15 x 29 minutes = 7 h 15 min: still inside the absolute limit.
    r.advance(29 * MIN); // 7 h 44 min
    assert!(r.store.get(&id).await.unwrap().is_some());
    r.advance(29 * MIN); // 8 h 13 min
    assert!(r.store.get(&id).await.unwrap().is_none(), "past eight hours it ends although it was used a minute ago");
}

#[tokio::test]
async fn signing_out_ends_the_session_and_twice_is_fine() {
    let r = Rig::new();
    let (id, _) = r.store.create(AccountId(1), None).await.unwrap();
    r.store.destroy(&id).await.unwrap();
    assert!(r.store.get(&id).await.unwrap().is_none());
    r.store.destroy(&id).await.unwrap();
    r.store.destroy("not-an-id").await.unwrap();
}

#[tokio::test]
async fn revoking_an_account_ends_all_its_sessions_and_not_the_ones_made_after() {
    let r = Rig::new();
    let (a1, _) = r.store.create(AccountId(5), None).await.unwrap();
    let (a2, _) = r.store.create(AccountId(5), Some(DomainId(2))).await.unwrap();
    let (other, _) = r.store.create(AccountId(6), None).await.unwrap();
    r.advance(10 * Duration::from_secs(1));
    r.store.revoke_account(AccountId(5)).await.unwrap();
    assert!(r.store.get(&a1).await.unwrap().is_none() && r.store.get(&a2).await.unwrap().is_none());
    assert!(r.store.get(&other).await.unwrap().is_some(), "another account is untouched");
    r.advance(Duration::from_secs(1));
    let (fresh, _) = r.store.create(AccountId(5), None).await.unwrap();
    assert!(r.store.get(&fresh).await.unwrap().is_some(), "the person signs in again");
}

#[tokio::test]
async fn a_record_that_is_not_ours_is_dropped_not_trusted() {
    let r = Rig::new();
    let (id, _) = r.store.create(AccountId(1), None).await.unwrap();
    r.kv.set(&format!("admin:sess:{}", hex_sha256(&id)), b"{\"account\":1", Duration::from_secs(60)).await.unwrap();
    assert!(r.store.get(&id).await.unwrap().is_none());
    assert!(r.kv.get(&format!("admin:sess:{}", hex_sha256(&id))).await.unwrap().is_none(), "and it is removed");
}

/// A store that cannot answer.
struct Down;

#[async_trait]
impl Kv for Down {
    async fn ping(&self) -> Result<()> {
        Err(Error::storage("down"))
    }
    async fn window_try_record(&self, _: &str, _: Duration, _: u32) -> Result<WindowOutcome> {
        Err(Error::storage("down"))
    }
    async fn window_count(&self, _: &str, _: Duration) -> Result<u32> {
        Err(Error::storage("down"))
    }
    async fn counter_incr(&self, _: &str, _: Duration) -> Result<u64> {
        Err(Error::storage("down"))
    }
    async fn counter_get(&self, _: &str) -> Result<u64> {
        Err(Error::storage("down"))
    }
    async fn set(&self, _: &str, _: &[u8], _: Duration) -> Result<()> {
        Err(Error::storage("down"))
    }
    async fn get(&self, _: &str) -> Result<Option<Vec<u8>>> {
        Err(Error::storage("down"))
    }
    async fn touch(&self, _: &str, _: Duration) -> Result<bool> {
        Err(Error::storage("down"))
    }
    async fn delete(&self, _: &str) -> Result<()> {
        Err(Error::storage("down"))
    }
}

#[tokio::test]
async fn when_the_store_is_down_nobody_is_signed_in_and_nothing_is_created() {
    let store = SessionStore::new(Arc::new(Down), SessionConfig::default());
    assert!(store.create(AccountId(1), None).await.is_err());
    let id = "a".repeat(64);
    assert!(store.get(&id).await.is_err(), "an error, never Some");
    assert!(store.revoke_account(AccountId(1)).await.is_err());
}
