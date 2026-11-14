//! The command layer of `rustyctl` against the Directory facade, on every backend.

mod common;

use std::time::Duration;

use common::{for_each_backend, Backend};
use rusty_ctl::{Command, PasswordSource};
use rusty_directory::{Authenticator, LocalAuthenticator, Resolved};

fn dom_add(n: &str) -> Command {
    Command::DomainAdd { name: n.into() }
}
fn acc_add(a: &str, p: PasswordSource) -> Command {
    Command::AccountAdd { addr: a.into(), password: p }
}
fn set_pw(a: &str) -> Command {
    Command::AccountSetPassword { addr: a.into(), password: PasswordSource::Stdin }
}

async fn auth(b: &Backend) -> LocalAuthenticator {
    LocalAuthenticator::new(b.dir.clone()).await.unwrap()
}

#[tokio::test]
async fn domain_and_account_then_login_works_and_a_wrong_password_fails() {
    for_each_backend(|b| async move {
        b.run(dom_add("example.com"), &[]).await.unwrap();
        b.run(acc_add("Alice@Example.COM", PasswordSource::Stdin), &["correct-horse"]).await.unwrap();
        let a = auth(&b).await;
        let id = a.verify("alice@example.com", "correct-horse").await.unwrap();
        assert!(id.is_some());
        assert_eq!(a.verify("alice@example.com", "wrong-horse").await.unwrap(), None);
        // The mailbox and the five system folders exist (the account is usable at once).
        assert_eq!(b.count("mailboxes").await, 1);
    })
    .await;
}

#[tokio::test]
async fn passwords_never_appear_in_any_output_or_error() {
    for_each_backend(|b| async move {
        let secret = "Sup3r-Secret-Value";
        let mut seen = Vec::new();
        b.run(dom_add("example.com"), &[]).await.unwrap();
        for r in [
            b.run(acc_add("a@example.com", PasswordSource::Prompt), &[secret]).await,
            b.run(acc_add("a@example.com", PasswordSource::Prompt), &[secret]).await, // duplicate
            b.run(acc_add("a@nodomain.test", PasswordSource::Prompt), &[secret]).await,
            b.run(set_pw("a@example.com"), &[secret]).await,
            b.run(set_pw("zz@example.com"), &[secret]).await,
        ] {
            match r {
                Ok(o) => seen.push(format!("{} {}", o.text, o.json)),
                Err(e) => seen.push(format!("{e} {e:?}")),
            }
        }
        for s in seen {
            assert!(!s.contains(secret), "password echoed: {s}");
        }
    })
    .await;
}

#[tokio::test]
async fn too_short_a_password_is_refused_without_creating_the_account() {
    for_each_backend(|b| async move {
        b.run(dom_add("example.com"), &[]).await.unwrap();
        let e = b.run(acc_add("a@example.com", PasswordSource::Stdin), &["short"]).await.unwrap_err();
        assert_eq!(e.exit, 1);
        assert!(!e.to_string().contains("short\""), "{e}");
        assert_eq!(b.count("accounts").await, 0);
    })
    .await;
}

#[tokio::test]
async fn idn_domains_and_addresses_are_normalised() {
    for_each_backend(|b| async move {
        let o = b.run(dom_add("BÜCHER.example"), &[]).await.unwrap();
        assert!(o.text.contains("xn--bcher-kva.example"), "{}", o.text);
        b.run(acc_add("Ünal@bücher.example", PasswordSource::None), &[]).await.unwrap();
        let l = b.run(Command::AccountList { domain: Some("BÜCHER.example".into()) }, &[]).await.unwrap();
        assert!(l.text.starts_with("ünal@xn--bcher-kva.example\tactive"), "{}", l.text);
        // The same address in another spelling is a duplicate, not a second account.
        assert!(b.run(acc_add("ÜNAL@xn--bcher-kva.example", PasswordSource::None), &[]).await.is_err());
        assert_eq!(b.count("accounts").await, 1);
    })
    .await;
}

#[tokio::test]
async fn duplicates_are_clean_errors_not_panics() {
    for_each_backend(|b| async move {
        b.run(dom_add("example.com"), &[]).await.unwrap();
        let e = b.run(dom_add("EXAMPLE.com"), &[]).await.unwrap_err();
        assert_eq!((e.exit, e.message.as_str()), (1, "domain: already exists"));
        b.run(acc_add("a@example.com", PasswordSource::None), &[]).await.unwrap();
        let e = b.run(acc_add("A@example.com", PasswordSource::None), &[]).await.unwrap_err();
        assert_eq!(e.exit, 1);
        assert!(e.message.contains("already used"), "{}", e.message);
        assert_eq!(b.count("accounts").await, 1);
    })
    .await;
}

#[tokio::test]
async fn account_in_a_missing_domain_fails_and_creates_nothing() {
    for_each_backend(|b| async move {
        let e = b.run(acc_add("a@nowhere.test", PasswordSource::None), &[]).await.unwrap_err();
        assert_eq!(e.exit, 1);
        assert!(e.message.contains("domain: not found"), "{}", e.message);
        assert_eq!((b.count("accounts").await, b.count("mailboxes").await, b.count("aliases").await), (0, 0, 0));
        // Malformed addresses are refused before anything else.
        for bad in ["no-at-sign", "a b@example.com", "@example.com", "a@"] {
            assert!(b.run(acc_add(bad, PasswordSource::None), &[]).await.is_err(), "{bad}");
        }
    })
    .await;
}

#[tokio::test]
async fn first_account_gets_postmaster_and_abuse_aliases() {
    for_each_backend(|b| async move {
        b.run(dom_add("example.com"), &[]).await.unwrap();
        let o = b.run(acc_add("alice@example.com", PasswordSource::None), &[]).await.unwrap();
        assert!(o.text.contains("postmaster@example.com"), "{}", o.text);
        let alice = b.dir.find_account("alice@example.com").await.unwrap().unwrap().id;
        assert_eq!(b.dir.resolve("postmaster@example.com").await.unwrap(), Resolved::Account(alice));
        assert_eq!(b.dir.resolve("abuse@example.com").await.unwrap(), Resolved::Account(alice));
        // A second account does not touch them.
        let o = b.run(acc_add("bob@example.com", PasswordSource::None), &[]).await.unwrap();
        assert!(!o.text.contains("aliases created"), "{}", o.text);
        assert_eq!(b.dir.resolve("postmaster@example.com").await.unwrap(), Resolved::Account(alice));
    })
    .await;
}

#[tokio::test]
async fn postmaster_as_first_account_needs_no_alias_for_itself() {
    for_each_backend(|b| async move {
        b.run(dom_add("example.com"), &[]).await.unwrap();
        b.run(acc_add("postmaster@example.com", PasswordSource::None), &[]).await.unwrap();
        let pm = b.dir.find_account("postmaster@example.com").await.unwrap().unwrap().id;
        assert_eq!(b.dir.resolve("postmaster@example.com").await.unwrap(), Resolved::Account(pm));
        assert_eq!(b.dir.resolve("abuse@example.com").await.unwrap(), Resolved::Account(pm));
        assert_eq!(b.count("aliases").await, 1);
    })
    .await;
}

#[tokio::test]
async fn a_postmaster_account_can_replace_the_automatic_alias() {
    for_each_backend(|b| async move {
        b.run(dom_add("example.com"), &[]).await.unwrap();
        b.run(acc_add("alice@example.com", PasswordSource::None), &[]).await.unwrap();
        let e = b.run(acc_add("postmaster@example.com", PasswordSource::None), &[]).await.unwrap_err();
        assert!(e.message.contains("alias remove"), "{}", e.message);
        b.run(Command::AliasRemove { alias: "postmaster@example.com".into() }, &[]).await.unwrap();
        b.run(acc_add("postmaster@example.com", PasswordSource::None), &[]).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn aliases_stay_inside_their_domain() {
    for_each_backend(|b| async move {
        b.run(dom_add("a.test"), &[]).await.unwrap();
        b.run(dom_add("b.test"), &[]).await.unwrap();
        b.run(acc_add("alice@a.test", PasswordSource::None), &[]).await.unwrap();
        b.run(acc_add("eve@b.test", PasswordSource::None), &[]).await.unwrap();
        // Pointing into another tenant is refused; nothing is created.
        let before = b.count("aliases").await;
        let e = b
            .run(Command::AliasAdd { alias: "info@a.test".into(), target: "eve@b.test".into() }, &[])
            .await
            .unwrap_err();
        assert!(e.message.contains("same domain"), "{}", e.message);
        assert_eq!(b.count("aliases").await, before);
        b.run(Command::AliasAdd { alias: "info@a.test".into(), target: "alice@a.test".into() }, &[]).await.unwrap();
        let o = b.run(Command::AliasList { domain: Some("a.test".into()) }, &[]).await.unwrap();
        assert!(o.text.contains("info@a.test\talice@a.test"), "{}", o.text);
        assert!(!o.text.contains("b.test"), "{}", o.text);
        // An alias of an unknown account / domain fails cleanly.
        assert!(b.run(Command::AliasAdd { alias: "x@a.test".into(), target: "nobody@a.test".into() }, &[]).await.is_err());
        assert!(b.run(Command::AliasAdd { alias: "x@nope.test".into(), target: "alice@a.test".into() }, &[]).await.is_err());
        b.run(Command::AliasRemove { alias: "info@a.test".into() }, &[]).await.unwrap();
        assert!(b.run(Command::AliasRemove { alias: "info@a.test".into() }, &[]).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn set_password_replaces_the_old_one_and_disable_blocks_login() {
    for_each_backend(|b| async move {
        b.run(dom_add("example.com"), &[]).await.unwrap();
        b.run(acc_add("a@example.com", PasswordSource::Stdin), &["first-password"]).await.unwrap();
        let a = auth(&b).await;
        b.run(set_pw("a@example.com"), &["second-password"]).await.unwrap();
        assert_eq!(a.verify("a@example.com", "first-password").await.unwrap(), None);
        assert!(a.verify("a@example.com", "second-password").await.unwrap().is_some());

        b.run(Command::AccountDisable { addr: "a@example.com".into() }, &[]).await.unwrap();
        assert_eq!(a.verify("a@example.com", "second-password").await.unwrap(), None);
        b.run(Command::AccountEnable { addr: "a@example.com".into() }, &[]).await.unwrap();
        assert!(a.verify("a@example.com", "second-password").await.unwrap().is_some());

        assert!(b.run(set_pw("ghost@example.com"), &["whatever-password"]).await.is_err());
        // An alias is not an account: set-password on it must not reach the target.
        b.run(Command::AliasAdd { alias: "al@example.com".into(), target: "a@example.com".into() }, &[]).await.unwrap();
        assert!(b.run(set_pw("al@example.com"), &["whatever-password"]).await.is_err());
        assert!(b.run(Command::AccountDisable { addr: "al@example.com".into() }, &[]).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn domain_disable_and_enable_are_visible_and_effective() {
    for_each_backend(|b| async move {
        b.run(dom_add("example.com"), &[]).await.unwrap();
        b.run(acc_add("a@example.com", PasswordSource::Stdin), &["a-password-1"]).await.unwrap();
        let a = auth(&b).await;
        b.run(Command::DomainDisable { name: "example.com".into() }, &[]).await.unwrap();
        let o = b.run(Command::DomainList, &[]).await.unwrap();
        assert_eq!(o.text, "example.com\tdisabled");
        assert_eq!(a.verify("a@example.com", "a-password-1").await.unwrap(), None);
        b.run(Command::DomainEnable { name: "example.com".into() }, &[]).await.unwrap();
        assert!(a.verify("a@example.com", "a-password-1").await.unwrap().is_some());
        assert!(b.run(Command::DomainDisable { name: "nope.test".into() }, &[]).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn listings_cover_every_page_and_have_a_json_form() {
    for_each_backend(|b| async move {
        b.run(dom_add("big.test"), &[]).await.unwrap();
        b.run(dom_add("small.test"), &[]).await.unwrap();
        let dom = b.dir.find_domain("big.test").await.unwrap().unwrap().id;
        for i in 0..205 {
            b.dir.create_account(&format!("u{i:03}"), dom, None, None).await.unwrap();
        }
        b.run(acc_add("only@small.test", PasswordSource::Stdin), &["only-password"]).await.unwrap();
        let o = b.run(Command::AccountList { domain: Some("big.test".into()) }, &[]).await.unwrap();
        assert_eq!(o.text.lines().count(), 205);
        assert!(o.text.lines().next().unwrap().starts_with("u000@big.test\tactive\tno-password"));
        let all = b.run(Command::AccountList { domain: None }, &[]).await.unwrap();
        assert_eq!(all.json["accounts"].as_array().unwrap().len(), 206);
        let last = all.json["accounts"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["address"], "only@small.test");
        assert_eq!(last["has_password"], true);
        let d = b.run(Command::DomainList, &[]).await.unwrap();
        assert_eq!(d.json["domains"].as_array().unwrap().len(), 2);
        assert_eq!(rusty_ctl::render(&d, true), d.json.to_string());
        assert_eq!(rusty_ctl::render(&d, false), d.text);
        assert!(b.run(Command::AccountList { domain: Some("missing.test".into()) }, &[]).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn check_lists_dns_hints_for_active_domains_only() {
    for_each_backend(|b| async move {
        b.run(dom_add("on.test"), &[]).await.unwrap();
        b.run(dom_add("off.test"), &[]).await.unwrap();
        b.run(Command::DomainDisable { name: "off.test".into() }, &[]).await.unwrap();
        let o = b.run(Command::Check, &[]).await.unwrap();
        assert!(o.text.contains("on.test. 3600 IN MX 10 mail.example.net."), "{}", o.text);
        assert!(o.text.contains("v=spf1 mx -all") && o.text.contains("_dmarc.on.test."), "{}", o.text);
        assert!(!o.text.contains("off.test"), "{}", o.text);
        let o = b.run_with_host(Command::Check, &[], None).await.unwrap();
        assert!(o.text.contains("hostname: not set"), "{}", o.text);
        assert!(!o.text.contains(" MX "), "{}", o.text);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_waits_for_a_busy_writer_instead_of_failing() {
    for_each_backend(|b| async move {
        let lock = b.fx.hold_write_lock("domains").await;
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            drop(lock);
        });
        let started = std::time::Instant::now();
        let r = b.run(dom_add("waited.test"), &[]).await;
        release.await.unwrap();
        r.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300), "did not wait: {:?}", started.elapsed());
        assert_eq!(b.count("domains").await, 1);
    })
    .await;
}

// ---- dkim generate / import (U5) ----

#[tokio::test]
async fn dkim_generate_writes_a_private_ed25519_key_and_prints_the_dns_record() {
    use std::os::unix::fs::PermissionsExt;
    for_each_backend(|b| async move {
        b.run(Command::DomainAdd { name: "example.org".into() }, &[]).await.unwrap();
        let keys = tempfile::tempdir().unwrap();
        let cmd = Command::DkimGenerate { domain: "example.org".into(), selector: "rusty".into() };
        // Without a configured key directory there is nowhere safe to write.
        assert!(b.run(cmd.clone(), &[]).await.is_err(), "{}", b.name);
        let out = b.run_with_dkim(cmd.clone(), keys.path()).await.unwrap();
        let file = keys.path().join("example.org.rusty-ed25519.pem");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "{}", b.name);
        assert!(out.text.contains("rusty-ed25519._domainkey.example.org") && out.text.contains("v=DKIM1; k=ed25519; p="), "{}", out.text);
        // The operator is told how to add the RSA key that large receivers need.
        assert!(out.text.contains("dkim import"), "{}", out.text);
        assert_eq!(out.json["algorithm"], "ed25519");
        assert!(!out.text.contains("PRIVATE KEY") && !out.json.to_string().contains("PRIVATE"), "the private key is never printed");
        // The public half is kept in the database (the admin web never reads a private key).
        let dom = b.dir.find_domain("example.org").await.unwrap().unwrap();
        let stored = b.dns.dkim_keys(dom.id).await.unwrap();
        assert_eq!(stored.len(), 1, "{}", b.name);
        assert_eq!((stored[0].selector.as_str(), stored[0].algorithm.as_str()), ("rusty-ed25519", "ed25519"), "{}", b.name);
        assert_eq!(out.json["created"], true);
        // A second run never replaces a key (that would break every published signature): it succeeds, changes nothing, and
        // prints the record of the key that is there, so an operator who lost the record can get it again.
        let before = std::fs::read(&file).unwrap();
        let again = b.run_with_dkim(cmd, keys.path()).await.unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), before, "{}: the key file is untouched", b.name);
        assert_eq!(again.json["created"], false, "{}", b.name);
        assert_eq!(again.json["record_value"], out.json["record_value"], "{}: the same key, the same record", b.name);
        assert!(again.text.contains("already exists"), "{}", again.text);
        assert!(!again.text.contains("PRIVATE KEY"), "{}", b.name);
        assert_eq!(b.dns.dkim_keys(dom.id).await.unwrap(), stored, "{}: nothing changed in the database", b.name);
        // An unknown domain gets no key.
        let unknown = Command::DkimGenerate { domain: "nope.test".into(), selector: "rusty".into() };
        assert!(b.run_with_dkim(unknown, keys.path()).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn dkim_import_keeps_an_operator_rsa_key_and_refuses_weak_or_foreign_files() {
    use std::os::unix::fs::PermissionsExt;
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../rusty-smtp/tests/data");
    let spki = std::fs::read_to_string(data.join("dkim-rsa2048.spki.b64")).unwrap();
    for_each_backend(|b| {
        let data = data.clone();
        let spki = spki.clone();
        async move {
            b.run(Command::DomainAdd { name: "example.org".into() }, &[]).await.unwrap();
            let keys = tempfile::tempdir().unwrap();
            let import = |file: &str| Command::DkimImport { domain: "example.org".into(), selector: "rusty".into(), key_file: data.join(file) };
            // Weak, encrypted, foreign or missing files are refused and nothing is written.
            for bad in ["dkim-rsa1024.pem", "dkim-rsa2048-encrypted.pem", "dkim-p256.pem", "no-such-file.pem", "."] {
                assert!(b.run_with_dkim(import(bad), keys.path()).await.is_err(), "{bad}");
            }
            assert_eq!(std::fs::read_dir(keys.path()).unwrap().count(), 0);
            let out = b.run_with_dkim(import("dkim-rsa2048.pem"), keys.path()).await.unwrap();
            let file = keys.path().join("example.org.rusty.pem");
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "{}", b.name);
            assert!(out.text.contains(&format!("rusty._domainkey.example.org. IN TXT \"v=DKIM1; k=rsa; p={}\"", spki.trim())), "{}", out.text);
            assert_eq!(out.json["algorithm"], "rsa");
            assert!(!out.text.contains("PRIVATE KEY") && !out.json.to_string().contains("PRIVATE"), "the private key is never printed");
            // Never overwritten; an unknown domain gets nothing.
            assert!(b.run_with_dkim(import("dkim-rsa2048.pem"), keys.path()).await.is_err());
            let unknown = Command::DkimImport { domain: "nope.test".into(), selector: "rusty".into(), key_file: data.join("dkim-rsa2048.pem") };
            assert!(b.run_with_dkim(unknown, keys.path()).await.is_err());
        }
    })
    .await;
}

#[tokio::test]
async fn the_dns_hints_list_every_stored_key_with_the_k_tag_of_its_algorithm() {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../rusty-smtp/tests/data");
    for_each_backend(|b| {
        let data = data.clone();
        async move {
            b.run(Command::DomainAdd { name: "example.org".into() }, &[]).await.unwrap();
            let keys = tempfile::tempdir().unwrap();
            // No key yet: one placeholder that says how to make one.
            let out = b.run(Command::Check, &[]).await.unwrap();
            assert!(out.text.contains("<selector>._domainkey.example.org"), "{}", out.text);
            b.run_with_dkim(Command::DkimGenerate { domain: "example.org".into(), selector: "rusty".into() }, keys.path()).await.unwrap();
            b.run_with_dkim(Command::DkimImport { domain: "example.org".into(), selector: "rusty".into(), key_file: data.join("dkim-rsa2048.pem") }, keys.path()).await.unwrap();
            let out = b.run(Command::Check, &[]).await.unwrap();
            assert!(!out.text.contains("<selector>"), "no placeholder once a key exists: {}", out.text);
            assert!(out.text.contains("rusty-ed25519._domainkey.example.org.") && out.text.contains("k=ed25519"), "{}", out.text);
            assert!(out.text.contains("rusty._domainkey.example.org.") && out.text.contains("k=rsa"), "{}", out.text);
            let records = out.json["domains"][0]["records"].as_array().unwrap();
            let dkim: Vec<&str> = records.iter().filter(|r| r["name"].as_str().unwrap().contains("._domainkey.")).map(|r| r["value"].as_str().unwrap()).collect();
            assert_eq!(dkim.len(), 2, "{dkim:?}");
            assert!(dkim.iter().any(|v| v.starts_with("v=DKIM1; k=ed25519; p=")) && dkim.iter().any(|v| v.starts_with("v=DKIM1; k=rsa; p=")), "{dkim:?}");
        }
    })
    .await;
}

// ---- seal keygen / seal show ----

#[tokio::test]
async fn seal_keygen_makes_a_private_key_file_and_prints_only_the_public_half() {
    use std::os::unix::fs::PermissionsExt;
    for_each_backend(|b| async move {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("unseal.key");
        let out = b.run(Command::SealKeygen { file: file.clone() }, &[]).await.unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "{}", b.name);
        let secret = std::fs::read_to_string(&file).unwrap();
        assert!(secret.trim().starts_with("rusty-unseal-v1:"), "{}", b.name);
        let hex_secret = secret.trim().strip_prefix("rusty-unseal-v1:").unwrap().to_owned();
        assert!(!out.text.contains(&hex_secret) && !out.json.to_string().contains(&hex_secret), "the private key is never printed");
        let public = out.json["public_key"].as_str().unwrap().to_owned();
        assert!(public.starts_with("rusty-seal-v1:") && out.text.contains(&public), "{}", out.text);
        assert_eq!(out.json["key_id"].as_str().unwrap().len(), 16);
        assert!(out.text.contains("[sealing]") && out.text.contains("[unsealing]"), "the configuration to add is shown: {}", out.text);
        // Never replaces a key (everything sealed to it would be lost).
        let before = std::fs::read(&file).unwrap();
        assert!(b.run(Command::SealKeygen { file: file.clone() }, &[]).await.is_err());
        assert_eq!(std::fs::read(&file).unwrap(), before, "{}", b.name);
        // Not a relative path, not in a directory that does not exist.
        assert!(b.run(Command::SealKeygen { file: "unseal.key".into() }, &[]).await.is_err());
        assert!(b.run(Command::SealKeygen { file: dir.path().join("no/such/dir/k") }, &[]).await.is_err());
        // `show` prints the same public key from the file.
        let shown = b.run(Command::SealShow { file: file.clone() }, &[]).await.unwrap();
        assert_eq!(shown.json["public_key"].as_str().unwrap(), public, "{}", b.name);
        assert!(!shown.text.contains(&hex_secret));
    })
    .await;
}

#[tokio::test]
async fn seal_show_refuses_a_loose_file_a_symlink_and_something_that_is_not_a_key() {
    use std::os::unix::fs::PermissionsExt;
    for_each_backend(|b| async move {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("k");
        b.run(Command::SealKeygen { file: good.clone() }, &[]).await.unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(b.run(Command::SealShow { file: good.clone() }, &[]).await.is_err(), "{}: a key file others can read is refused", b.name);
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        assert!(b.run(Command::SealShow { file: link }, &[]).await.is_err(), "{}: a symlink is not followed", b.name);
        let junk = dir.path().join("junk");
        std::fs::write(&junk, "not a key\n").unwrap();
        std::fs::set_permissions(&junk, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = b.run(Command::SealShow { file: junk }, &[]).await.unwrap_err();
        assert!(!err.to_string().contains("not a key"), "{err}");
        assert!(b.run(Command::SealShow { file: dir.path().join("missing") }, &[]).await.is_err());
        assert!(b.run(Command::SealShow { file: dir.path().to_owned() }, &[]).await.is_err(), "a directory");
    })
    .await;
}
