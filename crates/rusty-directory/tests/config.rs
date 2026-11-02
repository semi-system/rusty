use rusty_core::Error;
use rusty_directory::Directory;
use rusty_store::SqliteStore;
use serde_json::json;

async fn dir() -> Directory {
    Directory::new(SqliteStore::open("sqlite::memory:").await.unwrap())
}

#[tokio::test]
async fn config_precedence_account_over_plan_over_domain_over_global() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let plan = d.create_plan("basic", json!({})).await.unwrap();
    let acc = d
        .create_account("alice", dom, None, Some(plan))
        .await
        .unwrap();
    assert_eq!(d.effective_config(acc, "quota").await.unwrap(), None);
    d.set_global_config(json!({"quota": "global"}))
        .await
        .unwrap();
    assert_eq!(
        d.effective_config(acc, "quota").await.unwrap(),
        Some(json!("global"))
    );
    d.set_domain_config(dom, json!({"quota": "domain"}))
        .await
        .unwrap();
    assert_eq!(
        d.effective_config(acc, "quota").await.unwrap(),
        Some(json!("domain"))
    );
    d.set_plan_config(plan, json!({"quota": "plan"}))
        .await
        .unwrap();
    assert_eq!(
        d.effective_config(acc, "quota").await.unwrap(),
        Some(json!("plan"))
    );
    d.set_account_config(acc, json!({"quota": 5}))
        .await
        .unwrap();
    assert_eq!(
        d.effective_config(acc, "quota").await.unwrap(),
        Some(json!(5))
    );
    d.set_global_config(json!({"other": true})).await.unwrap();
    assert_eq!(
        d.effective_config(acc, "other").await.unwrap(),
        Some(json!(true))
    );
}

#[tokio::test]
async fn config_non_object_is_invalid() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    for v in [json!([1]), json!("s"), json!(null), json!(3)] {
        assert!(matches!(
            d.set_account_config(acc, v.clone()).await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            d.set_domain_config(dom, v.clone()).await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            d.set_global_config(v.clone()).await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            d.create_plan("p", v).await,
            Err(Error::Invalid(_))
        ));
    }
}

#[tokio::test]
async fn config_missing_key_is_none() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    d.set_account_config(acc, json!({"a": 1})).await.unwrap();
    assert_eq!(d.effective_config(acc, "b").await.unwrap(), None);
    assert!(matches!(
        d.effective_config(rusty_core::AccountId(999), "a").await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn null_config_value_inherits() {
    let d = dir().await;
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    d.set_domain_config(dom, json!({"q": 7})).await.unwrap();
    d.set_account_config(acc, json!({"q": null})).await.unwrap();
    assert_eq!(d.effective_config(acc, "q").await.unwrap(), Some(json!(7)));
    d.set_domain_config(dom, json!({"q": null})).await.unwrap();
    assert_eq!(d.effective_config(acc, "q").await.unwrap(), None);
}
