//! Directory conformance: ONE body per behaviour, run on Postgres
//! (`RUSTY_TEST_PG_URL`). Error kinds are asserted exactly.

mod common;

use std::collections::HashSet;

use common::{for_each_backend, Backend, Bind};
use rusty_core::{AccountId, DomainId, Error, ListId};
use rusty_directory::{
    AccountStatus, CapabilityRole, Directory, Resolved, MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT,
    MAX_LIST_MEMBERS,
};
use serde_json::json;

macro_rules! kind {
    ($r:expr, $pat:pat) => {
        assert!(matches!($r, Err($pat)), "unexpected: {:?}", $r)
    };
}

async fn domain_account(b: &Backend, tag: &str) -> (DomainId, AccountId) {
    let dom = b.dir.create_domain(&format!("{tag}.test")).await.unwrap();
    let acc = b.dir.create_account("alice", dom, None, None).await.unwrap();
    (dom, acc)
}

// ---------------------------------------------------------------- domains, plans, accounts

#[tokio::test]
async fn domain_create_normalizes_and_duplicates_conflict() {
    for_each_backend(|b| async move {
        let id = b.dir.create_domain(" Example.COM. ").await.unwrap();
        assert_eq!(
            b.scalar("SELECT COUNT(*) FROM domains WHERE name = ?1", &[Bind::S("example.com")]).await,
            1
        );
        kind!(b.dir.create_domain("example.com").await, Error::Conflict(_));
        kind!(b.dir.create_domain("EXAMPLE.com").await, Error::Conflict(_));
        let _ = id;
    })
    .await;
}

#[tokio::test]
async fn invalid_domains_are_invalid() {
    for_each_backend(|b| async move {
        for bad in ["", " ", "a b.com", "a@b.com", "-bad-.com", &format!("{}.com", "a".repeat(64))] {
            kind!(b.dir.create_domain(bad).await, Error::Invalid(_));
        }
    })
    .await;
}

#[tokio::test]
async fn unicode_domain_is_stored_as_punycode() {
    for_each_backend(|b| async move {
        b.dir.create_domain("bücher.example").await.unwrap();
        assert_eq!(
            b.scalar(
                "SELECT COUNT(*) FROM domains WHERE name = ?1",
                &[Bind::S("xn--bcher-kva.example")]
            )
            .await,
            1
        );
        kind!(b.dir.create_domain("BÜCHER.example").await, Error::Conflict(_));
    })
    .await;
}

#[tokio::test]
async fn plan_create_duplicate_reserved_and_non_object() {
    for_each_backend(|b| async move {
        b.dir.create_plan("basic", json!({"quota": 1})).await.unwrap();
        kind!(b.dir.create_plan("basic", json!({})).await, Error::Conflict(_));
        kind!(b.dir.create_plan("__global__", json!({})).await, Error::Invalid(_));
        kind!(b.dir.create_plan("x", json!([1])).await, Error::Invalid(_));
        kind!(b.dir.create_plan("", json!({})).await, Error::Invalid(_));
    })
    .await;
}

#[tokio::test]
async fn account_creates_mailbox_and_five_system_folders() {
    for_each_backend(|b| async move {
        let (_, acc) = domain_account(&b, "acc").await;
        let mb = b.dir.mailbox_for(acc).await.unwrap();
        assert_eq!(
            b.scalar("SELECT COUNT(*) FROM folders WHERE mailbox_id = ?1", &[Bind::I(mb.0)]).await,
            5
        );
        assert_eq!(
            b.scalar(
                "SELECT next_folder_id FROM mailboxes WHERE id = ?1",
                &[Bind::I(mb.0)]
            )
            .await,
            6
        );
        assert_eq!(
            b.scalar(
                "SELECT COUNT(*) FROM folders WHERE mailbox_id = ?1 AND id BETWEEN 1 AND 5 \
                 AND role IN ('inbox','sent','drafts','trash','spam')",
                &[Bind::I(mb.0)]
            )
            .await,
            5
        );
        let hw = b
            .scalar("SELECT uid_validity FROM mailboxes WHERE id = ?1", &[Bind::I(mb.0)])
            .await;
        for id in 1..=5i64 {
            let uv = b
                .scalar(
                    "SELECT uid_validity FROM folders WHERE mailbox_id = ?1 AND id = ?2",
                    &[Bind::I(mb.0), Bind::I(id)],
                )
                .await;
            assert!(uv <= hw && uv > 0, "folder {id}: {uv} vs high-water {hw}");
        }
    })
    .await;
}

#[tokio::test]
async fn account_address_is_normalized_and_duplicates_conflict() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("norm.test").await.unwrap();
        b.dir.create_account("Alice.Smith", dom, None, None).await.unwrap();
        kind!(b.dir.create_account("alice.smith", dom, None, None).await, Error::Conflict(_));
        // NFD and NFC spellings are the same account.
        b.dir.create_account("e\u{301}milie", dom, None, None).await.unwrap();
        kind!(b.dir.create_account("\u{e9}milie", dom, None, None).await, Error::Conflict(_));
    })
    .await;
}

#[tokio::test]
async fn invalid_local_parts_are_invalid() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("lp.test").await.unwrap();
        let long = "a".repeat(65);
        for bad in ["", "a b", "\"quoted\"", ".lead", "trail.", "a..b", "a(b)", "a\u{200b}b", &long] {
            kind!(b.dir.create_account(bad, dom, None, None).await, Error::Invalid(_));
        }
    })
    .await;
}

#[tokio::test]
async fn account_with_unknown_domain_or_plan_conflicts() {
    for_each_backend(|b| async move {
        kind!(b.dir.create_account("a", DomainId(424242), None, None).await, Error::Conflict(_));
        let dom = b.dir.create_domain("fk.test").await.unwrap();
        kind!(
            b.dir
                .create_account("a", dom, None, Some(rusty_core::PlanId(424242)))
                .await,
            Error::Conflict(_)
        );
        // The failed attempts left nothing behind.
        assert_eq!(b.scalar("SELECT COUNT(*) FROM accounts", &[]).await, 0);
        assert_eq!(b.scalar("SELECT COUNT(*) FROM mailboxes", &[]).await, 0);
    })
    .await;
}

#[tokio::test]
async fn failed_account_creation_rolls_back_mailbox_and_folders() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("rb.test").await.unwrap();
        b.dir.create_account("bob", dom, None, None).await.unwrap();
        kind!(b.dir.create_account("bob", dom, None, None).await, Error::Conflict(_));
        assert_eq!(b.scalar("SELECT COUNT(*) FROM mailboxes", &[]).await, 1);
        assert_eq!(b.scalar("SELECT COUNT(*) FROM folders", &[]).await, 5);
    })
    .await;
}

#[tokio::test]
async fn mailbox_for_unknown_account_is_not_found() {
    for_each_backend(|b| async move {
        kind!(b.dir.mailbox_for(AccountId(9999)).await, Error::NotFound(_));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_account_yields_distinct_mailboxes() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("conc.test").await.unwrap();
        let mut tasks = Vec::new();
        for i in 0..16 {
            let d: Directory = b.dir.clone();
            tasks.push(tokio::spawn(async move {
                d.create_account(&format!("user{i}"), dom, None, None).await
            }));
        }
        let mut ids = HashSet::new();
        for t in tasks {
            ids.insert(t.await.unwrap().unwrap());
        }
        assert_eq!(ids.len(), 16);
        assert_eq!(b.scalar("SELECT COUNT(*) FROM mailboxes", &[]).await, 16);
        assert_eq!(b.scalar("SELECT COUNT(*) FROM folders", &[]).await, 80);
    })
    .await;
}

// ---------------------------------------------------------------- aliases, lists, resolve

#[tokio::test]
async fn alias_resolves_and_cannot_reuse_an_account_or_list_address() {
    for_each_backend(|b| async move {
        let (dom, acc) = domain_account(&b, "al").await;
        b.dir.add_alias("Ally", dom, acc).await.unwrap();
        assert_eq!(b.dir.resolve("ally@al.test").await.unwrap(), Resolved::Account(acc));
        kind!(b.dir.add_alias("alice", dom, acc).await, Error::Conflict(_)); // account address
        kind!(b.dir.add_alias("ally", dom, acc).await, Error::Conflict(_)); // alias address
        b.dir.create_list("team", dom).await.unwrap();
        kind!(b.dir.add_alias("team", dom, acc).await, Error::Conflict(_)); // list address
        kind!(b.dir.create_account("ally", dom, None, None).await, Error::Conflict(_));
        kind!(b.dir.create_list("alice", dom).await, Error::Conflict(_));
        kind!(b.dir.create_list("ally", dom).await, Error::Conflict(_));
        kind!(b.dir.create_list("team", dom).await, Error::Conflict(_));
        kind!(b.dir.add_alias("zed", dom, AccountId(9999)).await, Error::Conflict(_));
        kind!(b.dir.add_alias("a b", dom, acc).await, Error::Invalid(_));
    })
    .await;
}

#[tokio::test]
async fn same_local_part_in_two_domains_is_allowed() {
    for_each_backend(|b| async move {
        let d1 = b.dir.create_domain("one.test").await.unwrap();
        let d2 = b.dir.create_domain("two.test").await.unwrap();
        let a1 = b.dir.create_account("x", d1, None, None).await.unwrap();
        let a2 = b.dir.create_account("x", d2, None, None).await.unwrap();
        assert_ne!(a1, a2);
        assert_eq!(b.dir.resolve("x@one.test").await.unwrap(), Resolved::Account(a1));
        assert_eq!(b.dir.resolve("x@two.test").await.unwrap(), Resolved::Account(a2));
    })
    .await;
}

#[tokio::test]
async fn resolve_list_expands_members_in_insertion_order() {
    for_each_backend(|b| async move {
        let (dom, acc) = domain_account(&b, "ls").await;
        let list = b.dir.create_list("team", dom).await.unwrap();
        b.dir.add_list_member(list, None, Some("Zed@Remote.Example")).await.unwrap();
        b.dir.add_list_member(list, Some(acc), None).await.unwrap();
        b.dir.add_list_member(list, None, Some("amy@remote.example")).await.unwrap();
        assert_eq!(
            b.dir.resolve("team@ls.test").await.unwrap(),
            Resolved::List(vec![
                Resolved::External("zed@remote.example".into()),
                Resolved::Account(acc),
                Resolved::External("amy@remote.example".into()),
            ])
        );
        // seq is dense 1..=3.
        assert_eq!(
            b.scalar("SELECT MAX(seq) FROM list_members WHERE list_id = ?1", &[Bind::I(list.0)]).await,
            3
        );
        assert_eq!(
            b.scalar("SELECT MIN(seq) FROM list_members WHERE list_id = ?1", &[Bind::I(list.0)]).await,
            1
        );
    })
    .await;
}

#[tokio::test]
async fn list_member_validation_and_duplicates() {
    for_each_backend(|b| async move {
        let (dom, acc) = domain_account(&b, "lm").await;
        let list = b.dir.create_list("team", dom).await.unwrap();
        kind!(b.dir.add_list_member(list, None, None).await, Error::Invalid(_));
        kind!(b.dir.add_list_member(list, Some(acc), Some("a@b.example")).await, Error::Invalid(_));
        kind!(b.dir.add_list_member(list, None, Some("no-at-sign")).await, Error::Invalid(_));
        b.dir.add_list_member(list, Some(acc), None).await.unwrap();
        kind!(b.dir.add_list_member(list, Some(acc), None).await, Error::Conflict(_));
        b.dir.add_list_member(list, None, Some("x@r.example")).await.unwrap();
        // Case variants of an external address collide after normalization.
        kind!(b.dir.add_list_member(list, None, Some("X@R.example")).await, Error::Conflict(_));
        // A list that does not exist.
        kind!(b.dir.add_list_member(ListId(9999), Some(acc), None).await, Error::Conflict(_));
        kind!(
            b.dir.add_list_member(list, Some(AccountId(9999)), None).await,
            Error::Conflict(_)
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_concurrent_add_member_get_dense_seq_and_no_spurious_conflict() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("seq.test").await.unwrap();
        let list = b.dir.create_list("team", dom).await.unwrap();
        let mut tasks = Vec::new();
        for i in 0..20 {
            let d = b.dir.clone();
            tasks.push(tokio::spawn(async move {
                d.add_list_member(list, None, Some(&format!("m{i}@r.example"))).await
            }));
        }
        for t in tasks {
            t.await.unwrap().expect("no spurious Conflict");
        }
        let n = b.scalar("SELECT COUNT(*) FROM list_members WHERE list_id = ?1", &[Bind::I(list.0)]).await;
        let max = b.scalar("SELECT MAX(seq) FROM list_members WHERE list_id = ?1", &[Bind::I(list.0)]).await;
        let min = b.scalar("SELECT MIN(seq) FROM list_members WHERE list_id = ?1", &[Bind::I(list.0)]).await;
        let distinct = b
            .scalar("SELECT COUNT(DISTINCT seq) FROM list_members WHERE list_id = ?1", &[Bind::I(list.0)])
            .await;
        assert_eq!((n, min, max, distinct), (20, 1, 20, 20));
        match b.dir.resolve("team@seq.test").await.unwrap() {
            Resolved::List(m) => assert_eq!(m.len(), 20),
            other => panic!("{other:?}"),
        }
    })
    .await;
}

#[tokio::test]
async fn list_member_cap_is_quota_on_add_and_on_resolve() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("cap.test").await.unwrap();
        let list = b.dir.create_list("big", dom).await.unwrap();
        // Bulk-fill with raw statements (10,000 API calls would dominate the test time).
        let fill = "INSERT INTO list_members (list_id, external) \
             SELECT ?1, 'm' || i || '@r.example' FROM generate_series(1, ?2::int) AS i";
        b.run(fill, &[Bind::I(list.0), Bind::I(MAX_LIST_MEMBERS as i64)]).await;
        kind!(b.dir.add_list_member(list, None, Some("one-more@r.example")).await, Error::Quota(_));
        match b.dir.resolve("big@cap.test").await.unwrap() {
            Resolved::List(m) => assert_eq!(m.len(), MAX_LIST_MEMBERS),
            other => panic!("{other:?}"),
        }
        // One beyond the cap (bypassing the API): resolve fails closed, never truncates.
        b.run(
            "INSERT INTO list_members (list_id, external) VALUES (?1, 'extra@r.example')",
            &[Bind::I(list.0)],
        )
        .await;
        kind!(b.dir.resolve("big@cap.test").await, Error::Quota(_));
    })
    .await;
}

#[tokio::test]
async fn resolve_unknown_and_malformed_addresses() {
    for_each_backend(|b| async move {
        domain_account(&b, "rs").await;
        kind!(b.dir.resolve("nobody@rs.test").await, Error::NotFound(_));
        kind!(b.dir.resolve("alice@unknown.test").await, Error::NotFound(_));
        kind!(b.dir.resolve("no-at-sign").await, Error::Invalid(_));
        kind!(b.dir.resolve("a b@rs.test").await, Error::Invalid(_));
    })
    .await;
}

#[tokio::test]
async fn resolve_accepts_case_unicode_and_punycode_forms() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("bücher.example").await.unwrap();
        let acc = b.dir.create_account("Zoë", dom, None, None).await.unwrap();
        for addr in ["zoë@bücher.example", "ZOË@BÜCHER.EXAMPLE", "zoë@xn--bcher-kva.example"] {
            assert_eq!(b.dir.resolve(addr).await.unwrap(), Resolved::Account(acc), "{addr}");
        }
    })
    .await;
}

#[tokio::test]
async fn account_beats_alias_beats_list_priority_is_unreachable_by_the_unique_registry() {
    // The registry makes the three kinds mutually exclusive per address; resolution is
    // therefore unambiguous on every backend.
    for_each_backend(|b| async move {
        let (dom, acc) = domain_account(&b, "pri").await;
        b.dir.add_alias("a2", dom, acc).await.unwrap();
        b.dir.create_list("l2", dom).await.unwrap();
        assert!(matches!(b.dir.resolve("alice@pri.test").await.unwrap(), Resolved::Account(_)));
        assert!(matches!(b.dir.resolve("a2@pri.test").await.unwrap(), Resolved::Account(_)));
        assert!(matches!(b.dir.resolve("l2@pri.test").await.unwrap(), Resolved::List(_)));
    })
    .await;
}

// ---------------------------------------------------------------- config

#[tokio::test]
async fn config_precedence_account_plan_domain_global() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("cfg.test").await.unwrap();
        let plan = b.dir.create_plan("basic", json!({})).await.unwrap();
        let acc = b.dir.create_account("alice", dom, None, Some(plan)).await.unwrap();
        assert_eq!(b.dir.effective_config(acc, "quota").await.unwrap(), None);
        b.dir.set_global_config(json!({"quota": "global"})).await.unwrap();
        assert_eq!(b.dir.effective_config(acc, "quota").await.unwrap(), Some(json!("global")));
        b.dir.set_domain_config(dom, json!({"quota": "domain"})).await.unwrap();
        assert_eq!(b.dir.effective_config(acc, "quota").await.unwrap(), Some(json!("domain")));
        b.dir.set_plan_config(plan, json!({"quota": "plan"})).await.unwrap();
        assert_eq!(b.dir.effective_config(acc, "quota").await.unwrap(), Some(json!("plan")));
        b.dir.set_account_config(acc, json!({"quota": 5})).await.unwrap();
        assert_eq!(b.dir.effective_config(acc, "quota").await.unwrap(), Some(json!(5)));
        // Setting the global config twice replaces it.
        b.dir.set_global_config(json!({"other": true})).await.unwrap();
        b.dir.set_global_config(json!({"other": false})).await.unwrap();
        assert_eq!(b.dir.effective_config(acc, "other").await.unwrap(), Some(json!(false)));
    })
    .await;
}

#[tokio::test]
async fn config_null_inherits_missing_key_is_none_non_object_invalid() {
    for_each_backend(|b| async move {
        let (dom, acc) = domain_account(&b, "cfg2").await;
        b.dir.set_domain_config(dom, json!({"k": "d"})).await.unwrap();
        b.dir.set_account_config(acc, json!({"k": null})).await.unwrap();
        assert_eq!(b.dir.effective_config(acc, "k").await.unwrap(), Some(json!("d")));
        assert_eq!(b.dir.effective_config(acc, "absent").await.unwrap(), None);
        for v in [json!([1]), json!("s"), json!(null), json!(3)] {
            kind!(b.dir.set_account_config(acc, v.clone()).await, Error::Invalid(_));
            kind!(b.dir.set_global_config(v.clone()).await, Error::Invalid(_));
        }
    })
    .await;
}

#[tokio::test]
async fn config_for_unknown_rows_is_not_found() {
    for_each_backend(|b| async move {
        kind!(b.dir.set_domain_config(DomainId(99), json!({})).await, Error::NotFound(_));
        kind!(b.dir.set_account_config(AccountId(99), json!({})).await, Error::NotFound(_));
        kind!(b.dir.set_plan_config(rusty_core::PlanId(99), json!({})).await, Error::NotFound(_));
        kind!(b.dir.effective_config(AccountId(99), "k").await, Error::NotFound(_));
    })
    .await;
}

// ---------------------------------------------------------------- status, password, auth

#[tokio::test]
async fn set_status_unknown_account_is_not_found_and_status_is_stored() {
    for_each_backend(|b| async move {
        kind!(b.dir.set_status(AccountId(99), AccountStatus::Locked).await, Error::NotFound(_));
        let (_, acc) = domain_account(&b, "st").await;
        for (s, text) in [
            (AccountStatus::Locked, "locked"),
            (AccountStatus::Disabled, "disabled"),
            (AccountStatus::Active, "active"),
        ] {
            b.dir.set_status(acc, s).await.unwrap();
            let n = b
                .scalar(
                    "SELECT COUNT(*) FROM accounts WHERE id = ?1 AND status = ?2",
                    &[Bind::I(acc.0), Bind::S(text)],
                )
                .await;
            assert_eq!(n, 1, "{text}");
        }
    })
    .await;
}

#[tokio::test]
async fn set_password_unknown_account_is_not_found() {
    for_each_backend(|b| async move {
        kind!(b.dir.set_password(AccountId(99), "pw").await, Error::NotFound(_));
        kind!(b.dir.set_password(AccountId(99), "").await, Error::Invalid(_));
    })
    .await;
}

#[tokio::test]
async fn authenticator_verifies_password_status_alias_and_unknown() {
    use rusty_directory::{Authenticator, LocalAuthenticator};
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("auth.test").await.unwrap();
        let acc = b.dir.create_account("alice", dom, Some("correct horse"), None).await.unwrap();
        b.dir.add_alias("al", dom, acc).await.unwrap();
        let nohash = b.dir.create_account("nohash", dom, None, None).await.unwrap();
        let auth = LocalAuthenticator::new(b.dir.clone()).await.unwrap();
        assert_eq!(auth.verify("alice@auth.test", "correct horse").await.unwrap(), Some(acc));
        assert_eq!(auth.verify("al@auth.test", "correct horse").await.unwrap(), Some(acc));
        assert_eq!(auth.verify("alice@auth.test", "wrong").await.unwrap(), None);
        assert_eq!(auth.verify("nobody@auth.test", "x").await.unwrap(), None);
        assert_eq!(auth.verify("nohash@auth.test", "x").await.unwrap(), None);
        b.dir.set_status(acc, AccountStatus::Locked).await.unwrap();
        assert_eq!(auth.verify("alice@auth.test", "correct horse").await.unwrap(), None);
        b.dir.set_status(acc, AccountStatus::Active).await.unwrap();
        assert_eq!(auth.verify("alice@auth.test", "correct horse").await.unwrap(), Some(acc));
        b.dir.set_password(acc, "new secret").await.unwrap();
        assert_eq!(auth.verify("alice@auth.test", "correct horse").await.unwrap(), None);
        assert_eq!(auth.verify("alice@auth.test", "new secret").await.unwrap(), Some(acc));
        let _ = nohash;
    })
    .await;
}

// ---------------------------------------------------------------- capabilities

async fn cap_setup(b: &Backend, tag: &str) -> (DomainId, AccountId) {
    domain_account(b, tag).await
}

#[tokio::test]
async fn capability_issue_validate_roundtrip_and_hash_only() {
    for_each_backend(|b| async move {
        let (dom, acc) = cap_setup(&b, "cap1").await;
        let cap = b.dir.issue_capability_at(acc, CapabilityRole::Imap, 3600, 1000).await.unwrap();
        let tok = cap.expose_secret().to_owned();
        let info = b
            .dir
            .validate_capability_at(&tok, CapabilityRole::Imap, 1000)
            .await
            .unwrap()
            .expect("valid");
        assert_eq!((info.account, info.domain, info.role, info.expires_at), (acc, dom, CapabilityRole::Imap, 4600));
        // Only a hash is stored; `_at` values are stored verbatim.
        assert_eq!(b.scalar("SELECT COUNT(*) FROM capabilities", &[]).await, 1);
        assert_eq!(b.scalar("SELECT issued_at FROM capabilities", &[]).await, 1000);
        assert_eq!(b.scalar("SELECT expires_at FROM capabilities", &[]).await, 4600);
        assert_eq!(
            b.scalar(
                "SELECT COUNT(*) FROM capabilities WHERE CAST(token_hash AS TEXT) = ?1",
                &[Bind::S(&tok)]
            )
            .await,
            0
        );
    })
    .await;
}

#[tokio::test]
async fn capability_wrong_role_expired_revoked_malformed_are_none() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap2").await;
        let cap = b.dir.issue_capability_at(acc, CapabilityRole::Imap, 100, 1000).await.unwrap();
        let tok = cap.expose_secret().to_owned();
        assert!(b.dir.validate_capability_at(&tok, CapabilityRole::Jmap, 1000).await.unwrap().is_none());
        assert!(b.dir.validate_capability_at(&tok, CapabilityRole::Imap, 1099).await.unwrap().is_some());
        // The boundary: valid while now < expires_at.
        assert!(b.dir.validate_capability_at(&tok, CapabilityRole::Imap, 1100).await.unwrap().is_none());
        for bad in ["", "zz", &"g".repeat(64), &"A".repeat(64), &"0".repeat(64)] {
            assert!(b.dir.validate_capability(bad, CapabilityRole::Imap).await.unwrap().is_none(), "{bad}");
        }
        assert!(b.dir.revoke_capability_at(&tok, 1050).await.unwrap());
        assert!(!b.dir.revoke_capability_at(&tok, 1050).await.unwrap());
        assert!(b.dir.validate_capability_at(&tok, CapabilityRole::Imap, 1051).await.unwrap().is_none());
        // revoked_at never precedes issued_at.
        assert!(b.scalar("SELECT revoked_at - issued_at FROM capabilities", &[]).await >= 0);
    })
    .await;
}

#[tokio::test]
async fn revoking_an_expired_token_is_false_and_revoke_all_counts_only_live() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap3").await;
        let old = b.dir.issue_capability_at(acc, CapabilityRole::Imap, 10, 1000).await.unwrap();
        b.dir.issue_capability_at(acc, CapabilityRole::Dav, 3600, 1000).await.unwrap();
        b.dir.issue_capability_at(acc, CapabilityRole::Jmap, 3600, 1000).await.unwrap();
        assert!(!b.dir.revoke_capability_at(old.expose_secret(), 2000).await.unwrap());
        assert_eq!(b.dir.revoke_all_capabilities_at(acc, 2000).await.unwrap(), 2);
        assert_eq!(b.dir.revoke_all_capabilities_at(acc, 2000).await.unwrap(), 0);
    })
    .await;
}

#[tokio::test]
async fn capability_for_missing_or_inactive_account_is_not_found() {
    for_each_backend(|b| async move {
        kind!(
            b.dir.issue_capability(AccountId(99), CapabilityRole::Imap, 60).await,
            Error::NotFound(_)
        );
        let (_, acc) = cap_setup(&b, "cap4").await;
        b.dir.set_status(acc, AccountStatus::Disabled).await.unwrap();
        kind!(b.dir.issue_capability(acc, CapabilityRole::Imap, 60).await, Error::NotFound(_));
        assert_eq!(b.scalar("SELECT COUNT(*) FROM capabilities", &[]).await, 0);
    })
    .await;
}

#[tokio::test]
async fn password_and_status_change_revoke_every_capability() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap5").await;
        let t1 = b.dir.issue_capability(acc, CapabilityRole::Imap, 600).await.unwrap();
        let t2 = b.dir.issue_capability(acc, CapabilityRole::Jmap, 600).await.unwrap();
        assert!(b.dir.validate_capability(t1.expose_secret(), CapabilityRole::Imap).await.unwrap().is_some());
        b.dir.set_password(acc, "pw").await.unwrap();
        assert!(b.dir.validate_capability(t1.expose_secret(), CapabilityRole::Imap).await.unwrap().is_none());
        assert!(b.dir.validate_capability(t2.expose_secret(), CapabilityRole::Jmap).await.unwrap().is_none());
        let t3 = b.dir.issue_capability(acc, CapabilityRole::Dav, 600).await.unwrap();
        b.dir.set_status(acc, AccountStatus::Locked).await.unwrap();
        assert!(b.dir.validate_capability(t3.expose_secret(), CapabilityRole::Dav).await.unwrap().is_none());
        // Re-activating does not resurrect it.
        b.dir.set_status(acc, AccountStatus::Active).await.unwrap();
        assert!(b.dir.validate_capability(t3.expose_secret(), CapabilityRole::Dav).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn locked_account_capability_validates_none_even_if_row_is_live() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap6").await;
        let t = b.dir.issue_capability(acc, CapabilityRole::Imap, 600).await.unwrap();
        // Flip the status behind the API's back: validation still requires `active`.
        b.run("UPDATE accounts SET status = 'locked' WHERE id = ?1", &[Bind::I(acc.0)]).await;
        assert!(b.dir.validate_capability(t.expose_secret(), CapabilityRole::Imap).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn capability_ttl_is_clamped() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap7").await;
        b.dir.issue_capability_at(acc, CapabilityRole::Imap, 0, 1000).await.unwrap();
        b.dir.issue_capability_at(acc, CapabilityRole::Imap, u32::MAX, 1000).await.unwrap();
        let min = b.scalar("SELECT MIN(expires_at - issued_at) FROM capabilities", &[]).await;
        let max = b.scalar("SELECT MAX(expires_at - issued_at) FROM capabilities", &[]).await;
        assert_eq!((min, max), (1, 86_400));
    })
    .await;
}

#[tokio::test]
async fn capability_time_overflow_is_invalid() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap8").await;
        kind!(
            b.dir.issue_capability_at(acc, CapabilityRole::Imap, 60, i64::MAX).await,
            Error::Invalid(_)
        );
    })
    .await;
}

#[tokio::test]
async fn per_account_cap_revokes_the_oldest_and_expired_do_not_count() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap9").await;
        let max = MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT;
        // One already-expired token at t=0 must not count against the cap.
        b.dir.issue_capability_at(acc, CapabilityRole::Imap, 1, 100).await.unwrap();
        let mut tokens = Vec::new();
        for i in 0..max {
            let t = b
                .dir
                .issue_capability_at(acc, CapabilityRole::Imap, 3600, 1000 + i)
                .await
                .unwrap();
            tokens.push(t.expose_secret().to_owned());
        }
        let now = 1000 + max;
        for t in &tokens {
            assert!(b.dir.validate_capability_at(t, CapabilityRole::Imap, now).await.unwrap().is_some());
        }
        // One more: the oldest of the live ones is revoked, the new one is live.
        let newest = b.dir.issue_capability_at(acc, CapabilityRole::Imap, 3600, now).await.unwrap();
        assert!(b.dir.validate_capability_at(&tokens[0], CapabilityRole::Imap, now).await.unwrap().is_none());
        assert!(b.dir.validate_capability_at(&tokens[1], CapabilityRole::Imap, now).await.unwrap().is_some());
        assert!(b
            .dir
            .validate_capability_at(newest.expose_secret(), CapabilityRole::Imap, now)
            .await
            .unwrap()
            .is_some());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_issues_keep_exactly_the_cap_active() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap10").await;
        let mut tasks = Vec::new();
        for _ in 0..(MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT + 16) {
            let d = b.dir.clone();
            tasks.push(tokio::spawn(async move {
                d.issue_capability(acc, CapabilityRole::Imap, 3600).await.map(|_| ())
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        let active = b
            .scalar(
                "SELECT COUNT(*) FROM capabilities WHERE revoked_at IS NULL AND account_id = ?1",
                &[Bind::I(acc.0)],
            )
            .await;
        assert_eq!(active, MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT);
    })
    .await;
}

#[tokio::test]
async fn capability_never_validates_as_another_account_and_tokens_are_unique() {
    for_each_backend(|b| async move {
        let (_, a1) = cap_setup(&b, "cap11").await;
        let dom2 = b.dir.create_domain("cap11b.test").await.unwrap();
        let a2 = b.dir.create_account("bob", dom2, None, None).await.unwrap();
        let t1 = b.dir.issue_capability(a1, CapabilityRole::Imap, 600).await.unwrap();
        let t2 = b.dir.issue_capability(a2, CapabilityRole::Imap, 600).await.unwrap();
        assert_ne!(t1.expose_secret(), t2.expose_secret());
        let i1 = b.dir.validate_capability(t1.expose_secret(), CapabilityRole::Imap).await.unwrap().unwrap();
        let i2 = b.dir.validate_capability(t2.expose_secret(), CapabilityRole::Imap).await.unwrap().unwrap();
        assert_eq!((i1.account, i2.account), (a1, a2));
        assert_ne!(i1.domain, i2.domain);
    })
    .await;
}

#[tokio::test]
async fn capability_times_follow_the_database_clock() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap12").await;
        let lo = b.db_now().await;
        let t = b.dir.issue_capability(acc, CapabilityRole::Imap, 600).await.unwrap();
        b.dir.revoke_capability(t.expose_secret()).await.unwrap();
        let hi = b.db_now().await;
        let issued = b.scalar("SELECT issued_at FROM capabilities", &[]).await;
        let revoked = b.scalar("SELECT revoked_at FROM capabilities", &[]).await;
        assert!((lo - 2..=hi + 2).contains(&issued), "{issued} not in [{lo},{hi}]");
        assert!((lo - 2..=hi + 2).contains(&revoked), "{revoked} not in [{lo},{hi}]");
        assert_eq!(b.scalar("SELECT expires_at - issued_at FROM capabilities", &[]).await, 600);
    })
    .await;
}

#[tokio::test]
async fn deleting_an_account_cascades_capabilities_and_mailbox() {
    for_each_backend(|b| async move {
        let (_, acc) = cap_setup(&b, "cap13").await;
        b.dir.issue_capability(acc, CapabilityRole::Imap, 600).await.unwrap();
        b.run("DELETE FROM accounts WHERE id = ?1", &[Bind::I(acc.0)]).await;
        assert_eq!(b.scalar("SELECT COUNT(*) FROM capabilities", &[]).await, 0);
        assert_eq!(b.scalar("SELECT COUNT(*) FROM mailboxes", &[]).await, 0);
        assert_eq!(b.scalar("SELECT COUNT(*) FROM folders", &[]).await, 0);
    })
    .await;
}

// ---------------------------------------------------------------- error text

#[tokio::test]
async fn errors_never_echo_caller_data() {
    for_each_backend(|b| async move {
        let secret = "zzz-secret-local";
        let dom = b.dir.create_domain("leak.test").await.unwrap();
        b.dir.create_account(secret, dom, None, None).await.unwrap();
        let errs = [
            b.dir.create_account(secret, dom, None, None).await.map(|_| ()).unwrap_err(),
            b.dir.add_alias(secret, dom, AccountId(1)).await.unwrap_err(),
            b.dir.create_domain("leak.test").await.map(|_| ()).unwrap_err(),
            b.dir.resolve(&format!("{secret}@absent.test")).await.map(|_| ()).unwrap_err(),
        ];
        for e in errs {
            let shown = format!("{e} / {e:?}");
            assert!(!shown.contains(secret) && !shown.contains("leak.test"), "{shown}");
            assert!(!shown.to_lowercase().contains("select") && !shown.contains(concat!("sq", "lx")), "{shown}");
        }
    })
    .await;
}

/// The count and the insert of `add_list_member` are one transaction under the list row lock
/// (Postgres `FOR UPDATE`). Several adders racing at `cap - 1` must
/// yield exactly one success and `Quota` for the rest, never `cap + 1` members. Repeated over
/// fresh lists because the race window is narrow without the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_adders_at_the_list_cap_boundary_admit_exactly_one() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("boundary.test").await.unwrap();
        for round in 0..6 {
            let list = b.dir.create_list(&format!("big{round}"), dom).await.unwrap();
            let fill = "INSERT INTO list_members (list_id, external) \
                 SELECT ?1, 'm' || i || '@r.example' FROM generate_series(1, ?2::int) AS i";
            b.run(fill, &[Bind::I(list.0), Bind::I(MAX_LIST_MEMBERS as i64 - 1)]).await;
            let mut tasks = Vec::new();
            for i in 0..8 {
                let d = b.dir.clone();
                tasks.push(tokio::spawn(async move {
                    d.add_list_member(list, None, Some(&format!("racer{i}@r.example"))).await
                }));
            }
            let (mut ok, mut quota) = (0, 0);
            for t in tasks {
                match t.await.unwrap() {
                    Ok(()) => ok += 1,
                    Err(Error::Quota(_)) => quota += 1,
                    Err(e) => panic!("unexpected error: {e:?}"),
                }
            }
            let n = b
                .scalar("SELECT COUNT(*) FROM list_members WHERE list_id = ?1", &[Bind::I(list.0)])
                .await;
            assert_eq!(
                (ok, quota, n),
                (1, 7, MAX_LIST_MEMBERS as i64),
                "round {round}: the cap was exceeded or no adder won"
            );
        }
    })
    .await;
}

/// The `*_capability_at` twins take a caller-supplied clock; absurd values are `Invalid`.
#[tokio::test]
async fn capability_at_twins_reject_absurd_clock_values() {
    for_each_backend(|b| async move {
        let (_, acc) = domain_account(&b, "bound").await;
        let tok = b.dir.issue_capability_at(acc, CapabilityRole::Imap, 60, 1_000).await.unwrap();
        for bad in [-1i64, i64::MIN, 4_102_444_801, i64::MAX] {
            kind!(b.dir.issue_capability_at(acc, CapabilityRole::Imap, 60, bad).await, Error::Invalid(_));
            kind!(b.dir.validate_capability_at(tok.expose_secret(), CapabilityRole::Imap, bad).await, Error::Invalid(_));
            kind!(b.dir.revoke_capability_at(tok.expose_secret(), bad).await, Error::Invalid(_));
            kind!(b.dir.revoke_all_capabilities_at(acc, bad).await, Error::Invalid(_));
        }
        // Still valid: nothing above revoked it.
        assert!(b.dir.validate_capability_at(tok.expose_secret(), CapabilityRole::Imap, 1_001).await.unwrap().is_some());
    })
    .await;
}

/// Eviction at the cap is deterministic and identical on both backends: among the active rows the
/// oldest `issued_at` goes first, ties (second granularity: a burst) are broken by `token_hash`
/// ascending, and the token just issued is never the victim. All tokens below share one second,
/// so the tie-break alone decides; several accounts make a chance pass negligible.
#[tokio::test]
async fn capability_eviction_tie_break_is_token_hash_order_on_both_backends() {
    for_each_backend(|b| async move {
        let dom = b.dir.create_domain("evict.test").await.unwrap();
        for n in 0..6 {
            let acc = b.dir.create_account(&format!("user{n}"), dom, None, None).await.unwrap();
            for _ in 0..MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT {
                b.dir.issue_capability_at(acc, CapabilityRole::Imap, 600, 5_000).await.unwrap();
            }
            b.dir.issue_capability_at(acc, CapabilityRole::Imap, 600, 5_000).await.unwrap();
            let active = b
                .scalar(
                    "SELECT COUNT(*) FROM capabilities WHERE account_id = ?1 AND revoked_at IS NULL",
                    &[Bind::I(acc.0)],
                )
                .await;
            assert_eq!(active, MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT, "account {n}");
            // Exactly one victim, and it has the smallest hash among the originals: at most one
            // active row (the new token) may sort below it.
            let victims = b
                .scalar(
                    "SELECT COUNT(*) FROM capabilities WHERE account_id = ?1 AND revoked_at IS NOT NULL",
                    &[Bind::I(acc.0)],
                )
                .await;
            assert_eq!(victims, 1, "account {n}");
            let smaller_active = b
                .scalar(
                    "SELECT COUNT(*) FROM capabilities a, capabilities v \
                     WHERE a.account_id = ?1 AND v.account_id = ?1 \
                       AND v.revoked_at IS NOT NULL AND a.revoked_at IS NULL \
                       AND a.token_hash < v.token_hash",
                    &[Bind::I(acc.0)],
                )
                .await;
            assert!(
                smaller_active <= 1,
                "account {n}: the victim is not the smallest token_hash ({smaller_active} active rows sort below it)"
            );
        }
    })
    .await;
}

// ---------------------------------------------------------------- registry drift (Postgres)

/// Drift state: the account row exists, its registry row does not (what `pg_restore
/// --disable-triggers`, `session_replication_role = replica`, logical-replication apply or an
/// operator `DISABLE TRIGGER` leave behind). The insert's own conflict handling must still turn a
/// duplicate into `Conflict`; an `ON CONFLICT DO NOTHING` would surface as a storage error.
#[tokio::test]
async fn account_create_conflicts_when_the_registry_row_is_missing() {
    for b in common::backends().await {
        let dom = b.dir.create_domain("drift.test").await.unwrap();
        b.run("ALTER TABLE accounts DISABLE TRIGGER accounts_addr_unique_ins", &[]).await;
        b.run(
            "INSERT INTO accounts(local_part, domain_id, status, created_at) VALUES ('dup', ?1, 'active', 0)",
            &[Bind::I(dom.0)],
        )
        .await;
        b.run("ALTER TABLE accounts ENABLE TRIGGER accounts_addr_unique_ins", &[]).await;
        kind!(b.dir.create_account("dup", dom, None, None).await, Error::Conflict(_));
    }
}
