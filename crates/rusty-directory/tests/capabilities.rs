use std::collections::HashSet;

use rusty_core::{AccountId, DomainId, Error};
use rusty_directory::{AccountStatus, CapabilityRole, Directory};
use rusty_store::{purge_capabilities, purge_capabilities_at, SqliteStore};

async fn setup() -> (Directory, DomainId, AccountId) {
    let d = Directory::new(SqliteStore::open("sqlite::memory:").await.unwrap());
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d
        .create_account("alice", dom, Some("pw"), None)
        .await
        .unwrap();
    (d, dom, acc)
}

async fn count(d: &Directory, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(d.store().pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn issue_validate_roundtrip() {
    let (d, dom, acc) = setup().await;
    let cap = d
        .issue_capability_at(acc, CapabilityRole::Imap, 3600, 1000)
        .await
        .unwrap();
    let t = cap.expose_secret();
    assert_eq!(t.len(), 64);
    assert!(t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    let info = d
        .validate_capability_at(t, CapabilityRole::Imap, 1001)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.account, acc);
    assert_eq!(info.domain, dom);
    assert_eq!(info.role, CapabilityRole::Imap);
    assert_eq!(info.expires_at, 4600);
    // The database-clock variants agree for a fresh token.
    let live = d
        .issue_capability(acc, CapabilityRole::Jmap, 60)
        .await
        .unwrap();
    assert!(d
        .validate_capability(live.expose_secret(), CapabilityRole::Jmap)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn only_hash_is_stored() {
    let (d, _dom, acc) = setup().await;
    let cap = d
        .issue_capability(acc, CapabilityRole::Dav, 600)
        .await
        .unwrap();
    let t = cap.expose_secret().to_string();
    let t_ascii_hex: String = t.bytes().map(|b| format!("{b:02x}")).collect();
    let pool = d.store().pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(tables.iter().any(|n| n == "capabilities"));
    let mut scanned = 0;
    for table in &tables {
        let cols: Vec<String> =
            sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .fetch_all(pool)
                .await
                .unwrap();
        for col in cols {
            scanned += 1;
            let sql = format!(
                "SELECT count(*) FROM \"{table}\" WHERE instr(CAST(\"{col}\" AS TEXT), ?1) > 0 \
                 OR instr(lower(hex(\"{col}\")), ?1) > 0 OR instr(lower(hex(\"{col}\")), ?2) > 0"
            );
            let n: i64 = sqlx::query_scalar(&sql)
                .bind(&t)
                .bind(&t_ascii_hex)
                .fetch_one(pool)
                .await
                .unwrap();
            assert_eq!(n, 0, "token found in {table}.{col}");
        }
    }
    assert!(scanned > 10);
    // The stored key is a 32-byte hash that is not the token's own bytes.
    let h: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM capabilities")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(h.len(), 32);
    let raw: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    assert_ne!(h, raw);
}

#[tokio::test]
async fn wrong_role_is_none() {
    let (d, _, acc) = setup().await;
    let cap = d
        .issue_capability(acc, CapabilityRole::Imap, 600)
        .await
        .unwrap();
    for r in [
        CapabilityRole::Jmap,
        CapabilityRole::Dav,
        CapabilityRole::SmtpSubmit,
    ] {
        assert!(d
            .validate_capability(cap.expose_secret(), r)
            .await
            .unwrap()
            .is_none());
    }
    assert!(d
        .validate_capability(cap.expose_secret(), CapabilityRole::Imap)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn expired_is_none_via_validate_at() {
    let (d, _, acc) = setup().await;
    let cap = d
        .issue_capability_at(acc, CapabilityRole::SmtpSubmit, 60, 1000)
        .await
        .unwrap();
    let t = cap.expose_secret();
    let r = CapabilityRole::SmtpSubmit;
    assert!(d
        .validate_capability_at(t, r, 1059)
        .await
        .unwrap()
        .is_some());
    assert!(d
        .validate_capability_at(t, r, 1060)
        .await
        .unwrap()
        .is_none());
    assert!(d
        .validate_capability_at(t, r, 9999)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn revoked_is_none() {
    let (d, _, acc) = setup().await;
    let a = d
        .issue_capability(acc, CapabilityRole::Imap, 600)
        .await
        .unwrap();
    let b = d
        .issue_capability(acc, CapabilityRole::Imap, 600)
        .await
        .unwrap();
    assert!(d.revoke_capability(a.expose_secret()).await.unwrap());
    assert!(!d.revoke_capability(a.expose_secret()).await.unwrap());
    assert!(!d.revoke_capability("not a token").await.unwrap());
    let r = CapabilityRole::Imap;
    assert!(d
        .validate_capability(a.expose_secret(), r)
        .await
        .unwrap()
        .is_none());
    assert!(d
        .validate_capability(b.expose_secret(), r)
        .await
        .unwrap()
        .is_some());
    assert_eq!(d.revoke_all_capabilities(acc).await.unwrap(), 1);
    assert_eq!(d.revoke_all_capabilities(acc).await.unwrap(), 0);
    assert!(d
        .validate_capability(b.expose_secret(), r)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn malformed_tokens_are_none_and_do_not_error() {
    let (d, _, _acc) = setup().await;
    let good = "0".repeat(64);
    let mut cases: Vec<String> = vec![
        String::new(),
        "0".repeat(63),
        "0".repeat(65),
        "A".repeat(64),
        "g".repeat(64),
        format!("{} ", "0".repeat(63)),
        format!("\u{e9}{}", "0".repeat(62)),
        "\0".repeat(64),
        "a".repeat(1_000_000),
        "\u{1F600}".repeat(16),
    ];
    cases.push(good); // well formed but unknown
    for t in &cases {
        for r in [CapabilityRole::Imap, CapabilityRole::SmtpSubmit] {
            assert!(d.validate_capability(t, r).await.unwrap().is_none());
        }
        assert!(!d.revoke_capability(t).await.unwrap());
    }
}

#[tokio::test]
async fn locked_account_capabilities_are_none() {
    let (d, _, acc) = setup().await;
    let cap = d
        .issue_capability(acc, CapabilityRole::Jmap, 600)
        .await
        .unwrap();
    let t = cap.expose_secret();
    let r = CapabilityRole::Jmap;
    // Bypass the revocation hook to prove validation itself checks the status.
    for status in ["locked", "disabled"] {
        sqlx::query("UPDATE accounts SET status = ? WHERE id = ?")
            .bind(status)
            .bind(acc.0)
            .execute(d.store().pool())
            .await
            .unwrap();
        assert!(d.validate_capability(t, r).await.unwrap().is_none());
    }
    sqlx::query("UPDATE accounts SET status = 'active' WHERE id = ?")
        .bind(acc.0)
        .execute(d.store().pool())
        .await
        .unwrap();
    assert!(d.validate_capability(t, r).await.unwrap().is_some());
    // Non-active accounts get no new capability, indistinguishable from a missing one.
    sqlx::query("UPDATE accounts SET status = 'locked' WHERE id = ?")
        .bind(acc.0)
        .execute(d.store().pool())
        .await
        .unwrap();
    assert!(matches!(
        d.issue_capability(acc, r, 60).await,
        Err(Error::NotFound(m)) if m == "account"
    ));
    assert!(matches!(
        d.issue_capability(AccountId(9999), r, 60).await,
        Err(Error::NotFound(m)) if m == "account"
    ));
}

#[tokio::test]
async fn password_change_revokes_all() {
    let (d, dom, acc) = setup().await;
    let other = d
        .create_account("bob", dom, Some("pw"), None)
        .await
        .unwrap();
    let r = CapabilityRole::Imap;
    let mine: Vec<_> = vec![
        d.issue_capability(acc, r, 600).await.unwrap(),
        d.issue_capability(acc, CapabilityRole::Dav, 600)
            .await
            .unwrap(),
    ];
    let theirs = d.issue_capability(other, r, 600).await.unwrap();
    d.set_password(acc, "new-pw").await.unwrap();
    assert!(d
        .validate_capability(mine[0].expose_secret(), r)
        .await
        .unwrap()
        .is_none());
    assert!(d
        .validate_capability(mine[1].expose_secret(), CapabilityRole::Dav)
        .await
        .unwrap()
        .is_none());
    assert!(d
        .validate_capability(theirs.expose_secret(), r)
        .await
        .unwrap()
        .is_some());
    // A failed set_password (unknown account) revokes nothing.
    assert!(d.set_password(AccountId(9999), "x").await.is_err());
    assert!(d
        .validate_capability(theirs.expose_secret(), r)
        .await
        .unwrap()
        .is_some());
    // New capabilities issued after the change work.
    let fresh = d.issue_capability(acc, r, 600).await.unwrap();
    assert!(d
        .validate_capability(fresh.expose_secret(), r)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn status_change_revokes_all() {
    let (d, _, acc) = setup().await;
    let r = CapabilityRole::Jmap;
    for status in [AccountStatus::Locked, AccountStatus::Disabled] {
        let cap = d.issue_capability(acc, r, 600).await.unwrap();
        d.set_status(acc, status).await.unwrap();
        d.set_status(acc, AccountStatus::Active).await.unwrap();
        // Re-activation does not resurrect it.
        assert!(d
            .validate_capability(cap.expose_secret(), r)
            .await
            .unwrap()
            .is_none());
    }
    // Setting Active on an active account keeps capabilities.
    let cap = d.issue_capability(acc, r, 600).await.unwrap();
    d.set_status(acc, AccountStatus::Active).await.unwrap();
    assert!(d
        .validate_capability(cap.expose_secret(), r)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn account_delete_cascades() {
    let (d, _, acc) = setup().await;
    d.issue_capability(acc, CapabilityRole::Imap, 600)
        .await
        .unwrap();
    assert_eq!(count(&d, "SELECT count(*) FROM capabilities").await, 1);
    sqlx::query("DELETE FROM accounts WHERE id = ?")
        .bind(acc.0)
        .execute(d.store().pool())
        .await
        .unwrap();
    assert_eq!(count(&d, "SELECT count(*) FROM capabilities").await, 0);
}

#[tokio::test]
async fn per_account_cap_of_64_revokes_oldest() {
    let (d, dom, acc) = setup().await;
    let other = d
        .create_account("bob", dom, Some("pw"), None)
        .await
        .unwrap();
    let r = CapabilityRole::Imap;
    let theirs = d.issue_capability_at(other, r, 3600, 1000).await.unwrap();
    let mut caps = Vec::new();
    for i in 0..66i64 {
        caps.push(d.issue_capability_at(acc, r, 3600, 1000 + i).await.unwrap());
    }
    let now = 2000;
    for (i, c) in caps.iter().enumerate() {
        let alive = d
            .validate_capability_at(c.expose_secret(), r, now)
            .await
            .unwrap()
            .is_some();
        assert_eq!(alive, i >= 2, "token {i}");
    }
    assert!(d
        .validate_capability_at(theirs.expose_secret(), r, now)
        .await
        .unwrap()
        .is_some());
    let active = count(
        &d,
        &format!(
            "SELECT count(*) FROM capabilities WHERE account_id = {} AND revoked_at IS NULL",
            acc.0
        ),
    )
    .await;
    assert_eq!(active, 64);
    // Expired capabilities do not count against the cap.
    let late = d.issue_capability_at(acc, r, 60, 1_000_000).await.unwrap();
    assert!(d
        .validate_capability_at(late.expose_secret(), r, 1_000_001)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn ttl_is_clamped() {
    let (d, _, acc) = setup().await;
    let r = CapabilityRole::Dav;
    for (ttl, want) in [
        (0u32, 1i64),
        (1, 1),
        (500, 500),
        (86_400, 86_400),
        (86_401, 86_400),
        (u32::MAX, 86_400),
    ] {
        let c = d.issue_capability_at(acc, r, ttl, 5000).await.unwrap();
        let info = d
            .validate_capability_at(c.expose_secret(), r, 5000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.expires_at, 5000 + want, "ttl {ttl}");
    }
}

#[tokio::test]
async fn debug_does_not_print_token() {
    let (d, _, acc) = setup().await;
    let c = d
        .issue_capability(acc, CapabilityRole::Imap, 60)
        .await
        .unwrap();
    let t = c.expose_secret().to_string();
    let dbg = format!("{c:?}");
    assert!(!dbg.contains(&t));
    assert!(!dbg.contains(&t[..8]));
    assert_eq!(dbg, "Capability(<redacted>)");
    let nested = format!("{:?}", Some(&c));
    assert!(!nested.contains(&t[..8]));
    let pretty = format!("{c:#?}");
    assert!(!pretty.contains(&t[..8]));
}

#[tokio::test]
async fn purge_capabilities_removes_only_old_dead_rows() {
    let (d, _, acc) = setup().await;
    let r = CapabilityRole::Imap;
    let _old_expired = d.issue_capability_at(acc, r, 60, 100).await.unwrap(); // expires 160
    let _recent_expired = d.issue_capability_at(acc, r, 60, 1000).await.unwrap(); // expires 1060
    let old_revoked = d.issue_capability_at(acc, r, 600, 150).await.unwrap();
    assert!(d
        .revoke_capability_at(old_revoked.expose_secret(), 200)
        .await
        .unwrap());
    let recent_revoked = d.issue_capability_at(acc, r, 3600, 900).await.unwrap();
    assert!(d
        .revoke_capability_at(recent_revoked.expose_secret(), 1000)
        .await
        .unwrap());
    let live = d.issue_capability_at(acc, r, 86_400, 1000).await.unwrap();
    assert_eq!(count(&d, "SELECT count(*) FROM capabilities").await, 5);

    let n = purge_capabilities(d.store(), 500).await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(count(&d, "SELECT count(*) FROM capabilities").await, 3);
    // Idempotent.
    assert_eq!(purge_capabilities(d.store(), 500).await.unwrap(), 0);
    assert!(d
        .validate_capability_at(live.expose_secret(), r, 1001)
        .await
        .unwrap()
        .is_some());
    // retention form: now=100_000, retention 86_400 -> older_than 13_600 removes the rest dead.
    assert_eq!(
        purge_capabilities_at(d.store(), 100_000, 86_400)
            .await
            .unwrap(),
        2
    );
    assert_eq!(count(&d, "SELECT count(*) FROM capabilities").await, 1);
}

#[tokio::test]
async fn purge_capabilities_batches_beyond_1000() {
    let (d, _, acc) = setup().await;
    for i in 0..2500i64 {
        sqlx::query(
            "INSERT INTO capabilities(token_hash,account_id,domain_id,role,issued_at,expires_at) \
             SELECT ?, id, domain_id, 'imap', 100, 160 FROM accounts WHERE id = ?",
        )
        .bind(
            i.to_be_bytes()
                .iter()
                .chain([0u8; 24].iter())
                .copied()
                .collect::<Vec<u8>>(),
        )
        .bind(acc.0)
        .execute(d.store().pool())
        .await
        .unwrap();
    }
    assert_eq!(
        purge_capabilities(d.store(), 1_000_000).await.unwrap(),
        2500
    );
}

#[tokio::test]
async fn tokens_are_unique_over_10k_issues() {
    let (d, dom, _) = setup().await;
    let mut seen = HashSet::new();
    let mut accounts = Vec::new();
    for i in 0..200 {
        accounts.push(
            d.create_account(&format!("u{i}"), dom, None, None)
                .await
                .unwrap(),
        );
    }
    for n in 0..10_000usize {
        let c = d
            .issue_capability(accounts[n % accounts.len()], CapabilityRole::Imap, 60)
            .await
            .unwrap();
        assert!(seen.insert(c.expose_secret().to_string()));
    }
    assert_eq!(seen.len(), 10_000);
}

#[tokio::test]
async fn capability_never_validates_as_another_account() {
    let (d, dom, a) = setup().await;
    let dom2 = d.create_domain("other.org").await.unwrap();
    let b = d
        .create_account("alice", dom2, Some("pw"), None)
        .await
        .unwrap();
    let r = CapabilityRole::Imap;
    let ca = d.issue_capability(a, r, 600).await.unwrap();
    let cb = d.issue_capability(b, r, 600).await.unwrap();
    let ia = d
        .validate_capability(ca.expose_secret(), r)
        .await
        .unwrap()
        .unwrap();
    let ib = d
        .validate_capability(cb.expose_secret(), r)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((ia.account, ia.domain), (a, dom));
    assert_eq!((ib.account, ib.domain), (b, dom2));
    assert_ne!(ca.expose_secret(), cb.expose_secret());
    // Revoking one tenant's account leaves the other's untouched.
    d.revoke_all_capabilities(a).await.unwrap();
    assert!(d
        .validate_capability(cb.expose_secret(), r)
        .await
        .unwrap()
        .is_some());
}

// ---------- review fixes ----------

fn hex_decode(s: &str) -> Vec<u8> {
    let v = |c: u8| match c {
        b'0'..=b'9' => c - b'0',
        _ => c - b'a' + 10,
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| (v(p[0]) << 4) | v(p[1]))
        .collect()
}

#[tokio::test]
async fn stored_hash_is_tagged_sha256_and_token_must_be_canonical() {
    use sha2::{Digest, Sha256};
    let (d, _, acc) = setup().await;
    let r = CapabilityRole::Imap;
    // A token with at least one letter and one digit.
    let cap = loop {
        let c = d.issue_capability(acc, r, 600).await.unwrap();
        let t = c.expose_secret();
        if t.bytes().any(|b| b.is_ascii_lowercase()) && t.bytes().any(|b| b.is_ascii_digit()) {
            break c;
        }
    };
    let t = cap.expose_secret().to_string();
    let mut h = Sha256::new();
    h.update(b"rusty.capability.v1");
    h.update(hex_decode(&t));
    let want: Vec<u8> = h.finalize().to_vec();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM capabilities WHERE token_hash = ?")
        .bind(&want)
        .fetch_one(d.store().pool())
        .await
        .unwrap();
    assert_eq!(n, 1, "stored hash is not SHA-256(tag || token bytes)");

    let first_alpha = t.find(|c: char| c.is_ascii_lowercase()).unwrap();
    let mut one_upper = t.clone();
    one_upper.replace_range(
        first_alpha..=first_alpha,
        &t[first_alpha..=first_alpha].to_uppercase(),
    );
    let first_digit = t.find(|c: char| c.is_ascii_digit()).unwrap();
    let mut fullwidth = t.clone();
    fullwidth.replace_range(first_digit..=first_digit, "\u{ff10}");
    let variants = [
        t.to_uppercase(),
        one_upper,
        format!("{t}\n"),
        format!(" {t}"),
        format!("{t}\0"),
        fullwidth,
        format!("{}\u{ff10}", &t[..63]),
    ];
    for v in &variants {
        assert!(
            d.validate_capability(v, r).await.unwrap().is_none(),
            "variant accepted"
        );
        assert!(!d.revoke_capability(v).await.unwrap());
    }
    assert!(d.validate_capability(&t, r).await.unwrap().is_some());
}

#[tokio::test]
async fn expired_capabilities_do_not_count_against_the_cap() {
    let (d, _, acc) = setup().await;
    let r = CapabilityRole::Imap;
    let mut live = Vec::new();
    for _ in 0..63 {
        live.push(d.issue_capability_at(acc, r, 86_400, 1000).await.unwrap());
    }
    // Ten one-second tokens, each issued after the previous one has expired.
    for i in 0..10i64 {
        d.issue_capability_at(acc, r, 1, 1100 + 2 * i)
            .await
            .unwrap();
    }
    d.issue_capability_at(acc, r, 600, 2000).await.unwrap();
    for (i, c) in live.iter().enumerate() {
        assert!(
            d.validate_capability_at(c.expose_secret(), r, 2001)
                .await
                .unwrap()
                .is_some(),
            "live token {i} was revoked"
        );
    }
}

#[tokio::test]
async fn revoke_all_count_excludes_expired_rows() {
    let (d, _, acc) = setup().await;
    let r = CapabilityRole::Dav;
    d.issue_capability_at(acc, r, 10, 1000).await.unwrap(); // expired, unrevoked
    d.issue_capability_at(acc, r, 600, 1400).await.unwrap();
    assert_eq!(d.revoke_all_capabilities_at(acc, 1500).await.unwrap(), 1);
}

#[tokio::test]
async fn revoking_an_expired_token_is_false() {
    let (d, _, acc) = setup().await;
    let r = CapabilityRole::Dav;
    let c = d.issue_capability_at(acc, r, 60, 1000).await.unwrap();
    assert!(!d
        .revoke_capability_at(c.expose_secret(), 1060)
        .await
        .unwrap());
    assert!(!d
        .revoke_capability_at(c.expose_secret(), 5000)
        .await
        .unwrap());
    let c2 = d.issue_capability_at(acc, r, 60, 1000).await.unwrap();
    assert!(d
        .revoke_capability_at(c2.expose_secret(), 1059)
        .await
        .unwrap());
}

#[tokio::test]
async fn malformed_revoke_takes_no_write_lock() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("meta.sqlite").display());
    let d = Directory::new(SqliteStore::open(&url).await.unwrap());
    // Hold the writer lock on another connection.
    let held = d.store().begin_write().await.unwrap();
    let r = tokio::time::timeout(Duration::from_millis(500), d.revoke_capability("zz")).await;
    assert!(
        matches!(r, Ok(Ok(false))),
        "malformed revoke must return immediately"
    );
    let r = tokio::time::timeout(
        Duration::from_millis(500),
        d.revoke_capability(&"A".repeat(64)),
    )
    .await;
    assert!(matches!(r, Ok(Ok(false))));
    drop(held);
}

async fn active_count(d: &Directory, acc: AccountId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM capabilities WHERE account_id = ? \
         AND revoked_at IS NULL AND expires_at > unixepoch()",
    )
    .bind(acc.0)
    .fetch_one(d.store().pool())
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_issues_keep_exactly_64_active() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("meta.sqlite").display());
    let d = Directory::new(SqliteStore::open(&url).await.unwrap());
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    let r = CapabilityRole::Imap;
    for _ in 0..63 {
        d.issue_capability(acc, r, 600).await.unwrap();
    }
    for _ in 0..5 {
        let mut hs = Vec::new();
        for _ in 0..16 {
            let d = d.clone();
            hs.push(tokio::spawn(async move {
                d.issue_capability(acc, r, 600).await.is_ok()
            }));
        }
        for h in hs {
            assert!(h.await.unwrap());
        }
        assert_eq!(active_count(&d, acc).await, 64);
    }
    // Locking races issues: whatever interleaving, nothing stays active.
    let mut hs = Vec::new();
    for i in 0..16 {
        let d = d.clone();
        hs.push(tokio::spawn(async move {
            if i == 8 {
                d.set_status(acc, AccountStatus::Locked).await.unwrap();
            } else {
                let _ = d.issue_capability(acc, r, 600).await;
            }
        }));
    }
    for h in hs {
        h.await.unwrap();
    }
    assert_eq!(active_count(&d, acc).await, 0);
}
