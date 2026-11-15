//! Sign-in and per-request authorization of the admin web, against a real directory and the grants table: one fixed failure for every reason,
//! the throttle, the tenant scope, and "the database decides, not the session".

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_admin::{AdminAuth, ManualNow, Scope, SessionConfig, SessionStore, SignInError};
use rusty_core::{AccountId, DomainId};
use rusty_directory::{AccountStatus, Directory, LocalAuthenticator};
use rusty_store::fixtures::pg_db as pg;
use rusty_store::{AdminStore, Store, StoreConfig};
use rusty_svc::{AuthThrottle, SystemClock};
use rusty_valkey::{ManualClock, MemoryKv};

const PW: &str = "correct-horse-battery-1";
const IP: &str = "203.0.113.9";

struct Rig {
    dir: Directory,
    admins: AdminStore,
    auth: AdminAuth,
    now: Arc<ManualNow>,
    kv_clock: Arc<ManualClock>,
    _store: Store,
    _db: pg::PgTestDb,
}

async fn rig(max_failures: u32) -> Rig {
    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    let db = pg::PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let dir = Directory::with_repo(store.repos().directory.clone());
    let admins = AdminStore::with_repo(store.repos().admin.clone());
    let kv_clock = Arc::new(ManualClock::new());
    let now = Arc::new(ManualNow::new(1_000_000));
    let sessions = SessionStore::new(Arc::new(MemoryKv::with_clock(kv_clock.clone())), SessionConfig::default()).with_clock(now.clone());
    let throttle = Arc::new(AuthThrottle::new(max_failures, 600, 4, Arc::new(SystemClock::new())));
    let authenticator = Arc::new(LocalAuthenticator::new(dir.clone()).await.unwrap());
    let auth = AdminAuth::new(authenticator, dir.clone(), admins.clone(), sessions, throttle);
    Rig { dir, admins, auth, now, kv_clock, _store: store, _db: db }
}

impl Rig {
    async fn domain(&self, name: &str) -> DomainId {
        self.dir.create_domain(name).await.unwrap()
    }
    async fn account(&self, local: &str, d: DomainId) -> AccountId {
        self.dir.create_account(local, d, Some(PW), None).await.unwrap()
    }
    fn advance(&self, d: Duration) {
        self.kv_clock.advance(d);
        self.now.advance(d);
    }
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[tokio::test]
async fn a_platform_administrator_signs_in_and_is_known_as_one() {
    let r = rig(5).await;
    let d = r.domain("one.test").await;
    let a = r.account("root", d).await;
    r.admins.grant(a, None).await.unwrap();
    let (id, session) = r.auth.sign_in(ip(IP), "root@one.test", PW).await.unwrap();
    assert_eq!((session.account, session.domain), (a.0, None));
    let (s, p) = r.auth.principal(&id).await.unwrap().unwrap();
    assert_eq!((p.account, p.scope), (a, Scope::Platform));
    assert_eq!(s.csrf, session.csrf);
    assert!(p.scope.allows(d) && p.scope.allows(DomainId(999)) && p.scope.is_platform());
    // The address is normalised like every login: upper case and an alias both work.
    assert!(r.auth.sign_in(ip(IP), "ROOT@ONE.TEST", PW).await.is_ok());
}

#[tokio::test]
async fn a_domain_administrator_is_confined_to_its_domain() {
    let r = rig(5).await;
    let (d1, d2) = (r.domain("one.test").await, r.domain("two.test").await);
    let a = r.account("boss", d1).await;
    r.admins.grant(a, Some(d1)).await.unwrap();
    let (id, _) = r.auth.sign_in(ip(IP), "boss@one.test", PW).await.unwrap();
    let (_, p) = r.auth.principal(&id).await.unwrap().unwrap();
    assert_eq!(p.scope, Scope::Domain(d1));
    assert!(p.scope.allows(d1) && !p.scope.allows(d2) && !p.scope.is_platform());
}

#[tokio::test]
async fn every_way_to_fail_is_the_same_error() {
    let r = rig(50).await;
    let d = r.domain("one.test").await;
    let admin = r.account("root", d).await;
    r.admins.grant(admin, None).await.unwrap();
    r.account("plain", d).await; // right password, but not an administrator
    let off = r.account("off", d).await;
    r.admins.grant(off, None).await.unwrap();
    r.dir.set_status(off, AccountStatus::Disabled).await.unwrap();
    for (who, pw) in [
        ("root@one.test", "wrong-password-1"),
        ("nobody@one.test", PW),
        ("plain@one.test", PW),
        ("off@one.test", PW),
        ("root@nowhere.test", PW),
        ("not an address", PW),
        ("", PW),
        ("root@one.test", ""),
        ("root@one.test", &"x".repeat(5000)),
    ] {
        let e = r.auth.sign_in(ip(IP), who, pw).await.err();
        assert_eq!(e, Some(SignInError::Invalid), "{who:?}");
    }
}

#[tokio::test]
async fn failures_are_throttled_per_address_and_per_account_and_success_clears_them() {
    let r = rig(3).await;
    let d = r.domain("one.test").await;
    let a = r.account("root", d).await;
    r.admins.grant(a, None).await.unwrap();
    for _ in 0..3 {
        assert_eq!(r.auth.sign_in(ip(IP), "root@one.test", "wrong-password-1").await.err(), Some(SignInError::Invalid));
    }
    // The fourth try is refused before any password is checked, even with the right one.
    assert!(matches!(r.auth.sign_in(ip(IP), "root@one.test", PW).await, Err(SignInError::Throttled { .. })));
    // The same account from another address has its own counter for that address, but the account counter is per (address, account).
    assert!(r.auth.sign_in(ip("198.51.100.7"), "root@one.test", PW).await.is_ok(), "another source address is not blocked by this one");
    // Guessing at unknown names is bounded as well.
    for _ in 0..3 {
        let _ = r.auth.sign_in(ip("198.51.100.8"), "ghost@one.test", "wrong-password-1").await;
    }
    assert!(matches!(r.auth.sign_in(ip("198.51.100.8"), "ghost@one.test", "wrong-password-1").await, Err(SignInError::Throttled { .. })));
}

#[tokio::test]
async fn a_valid_password_without_a_grant_counts_as_a_failure() {
    let r = rig(2).await;
    let d = r.domain("one.test").await;
    r.account("plain", d).await;
    for _ in 0..2 {
        assert_eq!(r.auth.sign_in(ip(IP), "plain@one.test", PW).await.err(), Some(SignInError::Invalid));
    }
    assert!(matches!(r.auth.sign_in(ip(IP), "plain@one.test", PW).await, Err(SignInError::Throttled { .. })), "probing for administrators is throttled like guessing");
}

#[tokio::test]
async fn the_database_decides_at_every_request_not_the_session() {
    let r = rig(5).await;
    let (d1, d2) = (r.domain("one.test").await, r.domain("two.test").await);
    let a = r.account("boss", d1).await;
    r.admins.grant(a, None).await.unwrap();
    let (id, _) = r.auth.sign_in(ip(IP), "boss@one.test", PW).await.unwrap();
    // Narrowed to its own domain: the next request already sees the new scope although the session was made as a platform one.
    r.admins.grant(a, Some(d1)).await.unwrap();
    let (_, p) = r.auth.principal(&id).await.unwrap().unwrap();
    assert_eq!(p.scope, Scope::Domain(d1));
    assert!(!p.scope.allows(d2));
    // Grant removed: not signed in, and the session is gone for good (re-granting does not bring it back).
    r.admins.revoke(a).await.unwrap();
    assert!(r.auth.principal(&id).await.unwrap().is_none());
    r.admins.grant(a, None).await.unwrap();
    assert!(r.auth.principal(&id).await.unwrap().is_none(), "a session that was refused once is ended");
}

#[tokio::test]
async fn a_disabled_account_is_signed_out_at_its_next_request() {
    let r = rig(5).await;
    let d = r.domain("one.test").await;
    let a = r.account("root", d).await;
    r.admins.grant(a, None).await.unwrap();
    let (id, _) = r.auth.sign_in(ip(IP), "root@one.test", PW).await.unwrap();
    assert!(r.auth.principal(&id).await.unwrap().is_some());
    r.dir.set_status(a, AccountStatus::Disabled).await.unwrap();
    assert!(r.auth.principal(&id).await.unwrap().is_none());
    assert!(r.auth.sign_in(ip(IP), "root@one.test", PW).await.is_err());
}

#[tokio::test]
async fn sign_out_and_revocation_end_sessions_and_a_password_change_is_a_revocation() {
    let r = rig(5).await;
    let d = r.domain("one.test").await;
    let a = r.account("root", d).await;
    r.admins.grant(a, None).await.unwrap();
    let (s1, _) = r.auth.sign_in(ip(IP), "root@one.test", PW).await.unwrap();
    let (s2, _) = r.auth.sign_in(ip(IP), "root@one.test", PW).await.unwrap();
    assert_ne!(s1, s2);
    r.auth.sign_out(&s1).await.unwrap();
    assert!(r.auth.principal(&s1).await.unwrap().is_none());
    assert!(r.auth.principal(&s2).await.unwrap().is_some(), "the other session is its own");
    r.advance(Duration::from_secs(5));
    r.dir.set_password(a, "a-new-password-2").await.unwrap();
    r.auth.revoke_account(a).await.unwrap();
    assert!(r.auth.principal(&s2).await.unwrap().is_none());
    r.advance(Duration::from_secs(5));
    assert!(r.auth.sign_in(ip(IP), "root@one.test", PW).await.is_err(), "the old password no longer works");
    assert!(r.auth.sign_in(ip(IP), "root@one.test", "a-new-password-2").await.is_ok());
}

#[tokio::test]
async fn a_session_that_never_existed_or_is_malformed_is_not_signed_in() {
    let r = rig(5).await;
    assert!(r.auth.principal(&"a".repeat(64)).await.unwrap().is_none());
    assert!(r.auth.principal("junk").await.unwrap().is_none());
    assert!(r.auth.principal("").await.unwrap().is_none());
}
