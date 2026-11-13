mod common;

use common::{for_each_backend, Backend, Bind};
use rusty_core::Error;
use rusty_directory::{Directory, Resolved};

async fn count(b: &Backend, table: &str) -> i64 {
    b.count(table).await
}

#[tokio::test]
async fn create_account_makes_mailbox_and_system_folders() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    let mb = d.mailbox_for(acc).await.unwrap();
    let rows: Vec<(i64, String, String, Option<i64>, i64)> = b
        .rows(
            "SELECT CAST(id AS TEXT) AS c1, name, role, CAST(parent_id AS TEXT) AS c4, \
             CAST(uid_validity AS TEXT) AS c5 FROM folders WHERE mailbox_id = ?1 ORDER BY id",
            &[Bind::I(mb.0)],
            5,
        )
        .await
        .into_iter()
        .map(|r| {
            (
                r[0].parse().unwrap(),
                r[1].clone(),
                r[2].clone(),
                (r[3] != "NULL").then(|| r[3].parse().unwrap()),
                r[4].parse().unwrap(),
            )
        })
        .collect();
    let got: Vec<(i64, &str, &str)> = rows
        .iter()
        .map(|r| (r.0, r.1.as_str(), r.2.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "Inbox", "inbox"),
            (2, "Sent", "sent"),
            (3, "Drafts", "drafts"),
            (4, "Trash", "trash"),
            (5, "Spam", "spam")
        ]
    );
    assert!(rows.iter().all(|r| r.3.is_none() && r.4 > 0));
    let uv = b.scalar("SELECT uid_validity FROM mailboxes WHERE id = ?1", &[Bind::I(mb.0)]).await;
    assert!(uv > 0);
    })
    .await;
}

#[tokio::test]
async fn address_is_normalized_to_lowercase() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("Example.COM").await.unwrap();
    let acc = d.create_account("Alice", dom, None, None).await.unwrap();
    assert_eq!(
        d.resolve("ALICE@Example.com").await.unwrap(),
        Resolved::Account(acc)
    );
    assert!(matches!(
        d.create_account("alice", dom, None, None).await,
        Err(Error::Conflict(_))
    ));
    })
    .await;
}

#[tokio::test]
async fn empty_local_part_is_invalid() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    for bad in ["", "  ", "a@b", "a b", "a\tb", "a\u{7}b"] {
        assert!(
            matches!(
                d.create_account(bad, dom, None, None).await,
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(d.create_domain("").await, Err(Error::Invalid(_))));
    assert!(matches!(
        d.create_domain("a@b.com").await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        d.resolve("nodomain").await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        d.resolve("@example.com").await,
        Err(Error::Invalid(_))
    ));
    })
    .await;
}

#[tokio::test]
async fn alias_cannot_reuse_account_address() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    assert!(matches!(
        d.add_alias("alice", dom, acc).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        d.create_list("alice", dom).await,
        Err(Error::Conflict(_))
    ));
    })
    .await;
}

#[tokio::test]
async fn resolve_alias_returns_account() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    d.add_alias("Ally", dom, acc).await.unwrap();
    assert_eq!(
        d.resolve("ally@example.com").await.unwrap(),
        Resolved::Account(acc)
    );
    })
    .await;
}

#[tokio::test]
async fn resolve_list_expands_members() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let a = d.create_account("alice", dom, None, None).await.unwrap();
    let list = d.create_list("team", dom).await.unwrap();
    d.add_list_member(list, Some(a), None).await.unwrap();
    d.add_list_member(list, None, Some("Bob@Other.org"))
        .await
        .unwrap();
    assert!(matches!(
        d.add_list_member(list, Some(a), Some("x@y.z")).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        d.add_list_member(list, None, None).await,
        Err(Error::Invalid(_))
    ));
    let Resolved::List(m) = d.resolve("team@example.com").await.unwrap() else {
        panic!()
    };
    assert_eq!(m.len(), 2);
    assert!(m.contains(&Resolved::Account(a)));
    assert!(m.contains(&Resolved::External("bob@other.org".into())));
    })
    .await;
}

#[tokio::test]
async fn resolve_unknown_is_not_found() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    d.create_domain("example.com").await.unwrap();
    assert!(matches!(
        d.resolve("nobody@example.com").await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        d.resolve("nobody@nowhere.com").await,
        Err(Error::NotFound(_))
    ));
    })
    .await;
}


async fn domain_name(b: &Backend, id: rusty_core::DomainId) -> String {
    b.strings("SELECT name FROM domains WHERE id = ?1", &[Bind::I(id.0)])
        .await
        .remove(0)
}

#[tokio::test]
async fn unicode_domain_stored_as_punycode() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let id = d.create_domain("münchen.de").await.unwrap();
    assert_eq!(domain_name(&b, id).await, "xn--mnchen-3ya.de");
    let id2 = d.create_domain(" EXAMPLE.COM. ").await.unwrap();
    assert_eq!(domain_name(&b, id2).await, "example.com");
    })
    .await;
}

#[tokio::test]
async fn uppercase_unicode_domain_same_as_lowercase() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    d.create_domain("münchen.de").await.unwrap();
    assert!(matches!(
        d.create_domain("MÜNCHEN.DE").await,
        Err(Error::Conflict(_))
    ));
    })
    .await;
}

#[tokio::test]
async fn nfd_and_nfc_local_part_are_same_account() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let a = d.create_account("\u{e9}", dom, None, None).await.unwrap();
    assert!(matches!(
        d.create_account("e\u{301}", dom, None, None).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        d.resolve("e\u{301}@example.com").await.unwrap(),
        Resolved::Account(a)
    );
    })
    .await;
}

#[tokio::test]
async fn resolve_accepts_unicode_and_punycode_forms() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("münchen.de").await.unwrap();
    let a = d.create_account("bob", dom, None, None).await.unwrap();
    for addr in [
        "Bob@München.DE",
        "bob@xn--mnchen-3ya.de",
        "bob@mu\u{308}nchen.de",
    ] {
        assert_eq!(
            d.resolve(addr).await.unwrap(),
            Resolved::Account(a),
            "{addr}"
        );
    }
    })
    .await;
}

#[tokio::test]
async fn label_longer_than_63_is_invalid() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let long = format!("{}.com", "a".repeat(64));
    assert!(matches!(
        d.create_domain(&long).await,
        Err(Error::Invalid(_))
    ));
    let total = format!("{0}.{0}.{0}.{0}.com", "a".repeat(62));
    assert!(matches!(
        d.create_domain(&total).await,
        Err(Error::Invalid(_))
    ));
    d.create_domain(&format!("{}.com", "a".repeat(63)))
        .await
        .unwrap();
    })
    .await;
}

#[tokio::test]
async fn local_part_over_64_octets_is_invalid() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    assert!(matches!(
        d.create_account(&"a".repeat(65), dom, None, None).await,
        Err(Error::Invalid(_))
    ));
    d.create_account(&"a".repeat(64), dom, None, None)
        .await
        .unwrap();
    })
    .await;
}

#[tokio::test]
async fn quoted_or_whitespace_local_part_is_invalid() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    for bad in ["\"a b\"", "a(c)b", "a b", "a\nb", "a\x5cb"] {
        assert!(
            matches!(
                d.create_account(bad, dom, None, None).await,
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    })
    .await;
}

#[tokio::test]
async fn domain_idna_invalid_is_invalid() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    for bad in [
        "xn--a.com",
        "a_b.com",
        "exa mple.com",
        "a..com",
        "-.com",
        "a b@c.com",
    ] {
        assert!(
            matches!(d.create_domain(bad).await, Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
    })
    .await;
}

// backend-neutral: pure unit test, no database
#[test]
fn display_domain_roundtrip() {
    assert_eq!(
        rusty_directory::display_domain("xn--mnchen-3ya.de"),
        "münchen.de"
    );
    assert_eq!(
        rusty_directory::display_domain("example.com"),
        "example.com"
    );
    let (_, p) = rusty_directory::parse_address("x@münchen.de").unwrap();
    assert_eq!(rusty_directory::display_domain(&p), "münchen.de");
}

#[tokio::test]
async fn create_account_rolls_back_on_failure() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    d.create_account("alice", dom, None, None).await.unwrap();
    assert!(d.create_account("alice", dom, None, None).await.is_err());
    // Fail on the LAST folder insert, after account, mailbox and 4 folders were inserted.
    b.run(
        "CREATE FUNCTION t_fn() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'boom'; END $$",
        &[],
    )
    .await;
    b.run(
        "CREATE TRIGGER t BEFORE INSERT ON folders FOR EACH ROW WHEN (NEW.role = 'spam') \
         EXECUTE FUNCTION t_fn()",
        &[],
    )
    .await;
    assert!(d.create_account("bob", dom, None, None).await.is_err());
    assert_eq!(count(&b, "accounts").await, 1);
    assert_eq!(count(&b, "mailboxes").await, 1);
    assert_eq!(count(&b, "folders").await, 5);
    assert!(matches!(
        d.resolve("bob@example.com").await,
        Err(Error::NotFound(_))
    ));
    // Positive control.
    b.run("DROP TRIGGER t ON folders", &[]).await;
    d.create_account("bob", dom, None, None).await.unwrap();
    assert_eq!(count(&b, "accounts").await, 2);
    assert_eq!(count(&b, "folders").await, 10);
    })
    .await;
}

#[tokio::test]
async fn local_part_must_be_atext() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    for c in "<>,;:[]\"()\\@ ".chars() {
        let bad = format!("a{c}b");
        assert!(
            matches!(
                d.create_account(&bad, dom, None, None).await,
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    for bad in [".a", "a.", "a..b", " alice", "alice "] {
        for r in [
            d.create_account(bad, dom, None, None).await.map(|_| ()),
            d.create_list(bad, dom).await.map(|_| ()),
        ] {
            assert!(matches!(r, Err(Error::Invalid(_))), "{bad:?}");
        }
    }
    let acc = d.create_account("x", dom, None, None).await.unwrap();
    assert!(matches!(
        d.add_alias("a>b", dom, acc).await,
        Err(Error::Invalid(_))
    ));
    let list = d.create_list("team", dom).await.unwrap();
    assert!(matches!(
        d.add_list_member(list, None, Some("x>,y@evil.com")).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        d.resolve("alice @example.com").await,
        Err(Error::Invalid(_)) | Err(Error::NotFound(_))
    ));
    // Positive controls.
    let a = d
        .create_account("A.b+tag_c", dom, None, None)
        .await
        .unwrap();
    assert_eq!(
        d.resolve("a.b+tag_c@example.com").await.unwrap(),
        Resolved::Account(a)
    );
    let u = d.create_account("josé.müller", dom, None, None).await.unwrap();
    assert_eq!(
        d.resolve("JOSÉ.müller@example.com").await.unwrap(),
        Resolved::Account(u)
    );
    })
    .await;
}

#[tokio::test]
async fn case_variant_external_members_collide() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let list = d.create_list("team", dom).await.unwrap();
    d.add_list_member(list, None, Some("Bob@Other.org"))
        .await
        .unwrap();
    assert!(matches!(
        d.add_list_member(list, None, Some("bob@OTHER.org")).await,
        Err(Error::Conflict(_))
    ));
    })
    .await;
}

#[tokio::test]
async fn folder_uid_validities_are_independent() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    for i in 0..20 {
        d.create_account(&format!("u{i}"), dom, None, None)
            .await
            .unwrap();
    }
    let rows: Vec<(i64, i64)> = b
        .rows(
            "SELECT CAST(mailbox_id AS TEXT) AS m, CAST(uid_validity AS TEXT) AS u FROM folders",
            &[],
            2,
        )
        .await
        .into_iter()
        .map(|r| (r[0].parse().unwrap(), r[1].parse().unwrap()))
        .collect();
    let varied = (1..=20)
        .filter(|m| {
            let mut v: Vec<i64> = rows.iter().filter(|r| r.0 == *m).map(|r| r.1).collect();
            v.sort();
            v.dedup();
            v.len() > 1
        })
        .count();
    assert!(varied >= 19, "only {varied} mailboxes had varied validities");
    })
    .await;
}

// ---------- uid_validity seeding (RFC 3501/9051 2.3.1.1, spec 4) ----------

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// (mailbox high-water mark, uid_validity of the 5 system folders)
async fn seeded(b: &Backend, acc: rusty_core::AccountId) -> (i64, Vec<i64>) {
    let mb = b.dir.mailbox_for(acc).await.unwrap();
    let hw = b
        .scalar("SELECT uid_validity FROM mailboxes WHERE id = ?1", &[Bind::I(mb.0)])
        .await;
    let folders = b
        .scalars(
            "SELECT uid_validity FROM folders WHERE mailbox_id = ?1 ORDER BY id",
            &[Bind::I(mb.0)],
        )
        .await;
    (hw, folders)
}

#[tokio::test]
async fn mailbox_high_water_mark_covers_system_folders() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    for i in 0..5 {
        let acc = d.create_account(&format!("hw{i}"), dom, None, None).await.unwrap();
        let (hw, folders) = seeded(&b, acc).await;
        assert_eq!(folders.len(), 5);
        assert!(folders.iter().all(|v| *v <= hw), "{hw} vs {folders:?}");
    }
    })
    .await;
}

#[tokio::test]
async fn uid_validity_seed_is_the_clock() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let before = now_secs();
    let acc = d.create_account("clock", dom, None, None).await.unwrap();
    let (hw, folders) = seeded(&b, acc).await;
    for v in folders.iter().chain(std::iter::once(&hw)) {
        assert!(
            (before - 5..=before + 10).contains(v),
            "{v} not within [now-5s, now+10s] of {before}"
        );
        assert!(*v <= i64::from(u32::MAX));
    }
    })
    .await;
}

#[tokio::test]
async fn first_custom_folder_uid_validity_exceeds_system_folders() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("custom", dom, None, None).await.unwrap();
    let (_, system) = seeded(&b, acc).await;
    let mailbox = b.mailbox(acc).await;
    let f = mailbox.create_folder(None, "Projects").await.unwrap();
    let v = b
        .scalar(
            "SELECT uid_validity FROM folders WHERE mailbox_id = ?1 AND id = ?2",
            &[Bind::I(mailbox.id().0), Bind::I(f.0)],
        )
        .await;
    assert!(system.iter().all(|s| v > *s), "{v} vs {system:?}");
    })
    .await;
}

static PATH_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Run `local` through every entry path; returns true if ALL accept, false if ALL reject
/// with Invalid. Panics on mixed results or other errors.
async fn all_paths(b: &Backend, local: &str) -> bool {
    use rusty_directory::parse_address;
    // A fresh domain per path so a successful path cannot cause a Conflict in the next.
    async fn fresh(
        b: &Backend,
    ) -> (Directory, rusty_core::DomainId, rusty_core::AccountId, rusty_core::ListId) {
        let n = PATH_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = b.dir.clone();
        let dom = d.create_domain(&format!("p{n}.example.com")).await.unwrap();
        let owner = d.create_account("owner", dom, None, None).await.unwrap();
        let list = d.create_list("team", dom).await.unwrap();
        (d, dom, owner, list)
    }
    let (d, dom, _, _) = fresh(b).await;
    let r1 = d.create_account(local, dom, None, None).await.map(|_| ());
    let (d, dom, _, _) = fresh(b).await;
    let r2 = d.create_list(local, dom).await.map(|_| ());
    let (d, dom, owner, _) = fresh(b).await;
    let r3 = d.add_alias(local, dom, owner).await;
    let (d, _, _, list) = fresh(b).await;
    let r4 = d.add_list_member(list, None, Some(&format!("{local}@other.org"))).await;
    let r = [parse_address(&format!("{local}@example.com")).map(|_| ()), r1, r2, r3, r4];
    let ok = r.iter().filter(|x| x.is_ok()).count();
    let inv = r.iter().filter(|x| matches!(x, Err(Error::Invalid(_)))).count();
    match (ok, inv) {
        (5, 0) => true,
        (0, 5) => false,
        _ => panic!("{local:?}: mixed results {r:?}"),
    }
}

#[tokio::test]
async fn rejects_zero_width_and_bidi_override_in_local_part() {
    for_each_backend(|b| async move {
    // Positive controls next to the negatives.
    for good in ["josé.müller", "straße", "ørsted", "łukasz", "đạt", "tơ.ư", "nguyễn.văn.an", "ı", "a1_b-c+d"] {
        assert!(all_paths(&b, good).await, "{good:?}");
    }
    let invisible = [
        '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{202A}', '\u{202E}', '\u{2066}',
        '\u{2069}', '\u{FEFF}', '\u{00AD}', '\u{034F}', '\u{FE0F}', '\u{E0100}', '\u{3164}',
        '\u{115F}', '\u{1160}', '\u{FFA0}', '\u{2800}', '\u{E000}', '\u{0378}', '\u{00A0}',
        '\u{0000}', '\u{2028}', '\u{200F}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{2067}',
        '\u{2068}', '\u{F8FF}', '\u{10FFFD}', '\u{FFFF}',
    ];
    for c in invisible {
        for local in [format!("al{c}ice"), format!("{c}alice"), format!("alice{c}")] {
            // parse_address trims surrounding Unicode whitespace of the whole address (pre-existing
            // behavior), so a LEADING whitespace char is stripped rather than rejected there.
            if c.is_whitespace() && local.starts_with(c) {
                continue;
            }
            assert!(!all_paths(&b, &local).await, "{local:?}");
        }
    }
    })
    .await;
}

#[tokio::test]
async fn local_part_allow_list_accepts_latin_and_normalizes() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let nfc = "nguy\u{1EC5}n.v\u{103}n.an"; // ễ and ă precomposed
    let nfd = "nguye\u{302}\u{303}n.va\u{306}n.an"; // decomposed
    assert_ne!(nfc, nfd);
    d.create_account(nfc, dom, None, None).await.unwrap();
    assert!(matches!(
        d.create_account(nfd, dom, None, None).await,
        Err(Error::Conflict(_))
    ));
    // Uppercase is lowercased, then validated.
    let up = rusty_directory::parse_address("NGUY\u{1EC4}N@Example.com").unwrap();
    assert_eq!(up.0, "nguy\u{1EC5}n");
    // a + combining acute composes to a precomposed letter and is accepted.
    assert_eq!(rusty_directory::parse_address("a\u{301}@example.com").unwrap().0, "\u{E1}");
    // Exactly 64 octets ok, 65 not; 64 two-octet chars is over the octet limit.
    assert!(all_paths(&b, &"a".repeat(64)).await);
    assert!(!all_paths(&b, &"a".repeat(65)).await);
    assert!(!all_paths(&b, &"é".repeat(33)).await);
    assert!(all_paths(&b, &"é".repeat(32)).await);
    })
    .await;
}

#[tokio::test]
async fn local_part_allow_list_rejects_everything_else() {
    for_each_backend(|b| async move {
    let bad = [
        "\u{430}bc",            // Cyrillic a inside a Latin word
        "\u{430}\u{440}\u{435}", // all-Cyrillic
        "\u{3BF}",              // Greek omicron
        "\u{FF41}",             // fullwidth a
        "\u{FB01}",             // ligature fi
        "\u{261}",              // IPA g
        "\u{AA}",
        "\u{BA}",
        "x\u{302}\u{303}",      // no precomposed form: combining marks stay and are rejected
        "a\u{300}\u{301}\u{302}\u{303}\u{304}\u{305}", // Zalgo stack
        "\u{1F600}",            // emoji
        "\u{130}",              // I with dot: lowercases to i + U+0307
        "\u{663}",              // Arabic-Indic digit
        "\u{D7}",               // multiplication sign
        "\u{F7}",               // division sign
        "\u{C0}\u{2160}",       // roman numeral
        "\u{1E9E}\u{1D00}",     // small cap (Ll but outside blocks)
        "\u{17F}",             // long s: NFKC-unstable
        "\u{1C6}",             // dz digraph: NFKC-unstable
        "\u{133}",             // ij ligature: NFKC-unstable
        "\u{1E9A}",            // a with right half ring: NFKC-unstable
        ".a",
        "a.",
        "a..b",
        "a b",
        "a\tb",
    ];
    for w in bad {
        assert!(!all_paths(&b, w).await, "{w:?}");
    }
    // Positive controls with the same shape.
    for good in ["abc", "a.b", "\u{DF}", "\u{FF}", "\u{17E}", "\u{24F}", "\u{1EFF}", "\u{1B0}"] {
        assert!(all_paths(&b, good).await, "{good:?}");
    }
    })
    .await;
}

// ---------- folder id counter seed (migration 0003) and write transactions ----------

#[tokio::test]
async fn create_account_seeds_next_folder_id_after_system_folders() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("seed", dom, None, None).await.unwrap();
    let mb = d.mailbox_for(acc).await.unwrap();
    let next = b
        .scalar("SELECT next_folder_id FROM mailboxes WHERE id = ?1", &[Bind::I(mb.0)])
        .await;
    assert_eq!(next, 6);
    })
    .await;
}

/// `create_account` runs in a `BEGIN IMMEDIATE` transaction: concurrent creations on a
/// file-backed database (pool of 4 connections) all succeed and none is half-written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_account_on_file_db() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..32 {
        let d = d.clone();
        tasks.push(tokio::spawn(async move {
            d.create_account(&format!("user{i}"), dom, None, None).await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(count(&b, "accounts").await, 32);
    assert_eq!(count(&b, "mailboxes").await, 32);
    assert_eq!(count(&b, "folders").await, 32 * 5);
    })
    .await;
}

/// Insert `n` external members straight into the table (bypassing the cap), as old data or
/// another writer could have.
async fn bulk_members(b: &Backend, list: rusty_core::ListId, n: usize) {
    b.run(
        "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < ?1) \
         INSERT INTO list_members (list_id, account_id, external) \
         SELECT ?2, NULL, 'user' || i || '@other.org' FROM s",
        &[Bind::I(n as i64), Bind::I(list.0)],
    )
    .await;
}

#[tokio::test]
async fn list_expansion_cap_enforced_on_add_and_resolve() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    use rusty_directory::MAX_LIST_MEMBERS;
    let dom = d.create_domain("example.com").await.unwrap();
    let list = d.create_list("big", dom).await.unwrap();
    bulk_members(&b, list, MAX_LIST_MEMBERS - 1).await;
    // One below the cap: the last slot can still be filled, and the list still resolves.
    d.add_list_member(list, None, Some("last@other.org"))
        .await
        .unwrap();
    let Resolved::List(m) = d.resolve("big@example.com").await.unwrap() else {
        panic!()
    };
    assert_eq!(m.len(), MAX_LIST_MEMBERS);
    // At the cap: adding refuses and nothing is written.
    assert!(matches!(
        d.add_list_member(list, None, Some("one-more@other.org"))
            .await,
        Err(Error::Quota(_))
    ));
    assert_eq!(count(&b, "list_members").await as usize, MAX_LIST_MEMBERS);
    // A list that already exceeds the cap (inserted behind our back) is Quota, never a
    // truncated list.
    b.run(
        "INSERT INTO list_members (list_id, account_id, external) VALUES (?1, NULL, 'x@y.z')",
        &[Bind::I(list.0)],
    )
    .await;
    assert!(matches!(
        d.resolve("big@example.com").await,
        Err(Error::Quota(_))
    ));
    // Other lists are unaffected.
    let small = d.create_list("small", dom).await.unwrap();
    d.add_list_member(small, None, Some("a@b.c")).await.unwrap();
    assert!(d.resolve("small@example.com").await.is_ok());
    })
    .await;
}

#[tokio::test]
async fn resolve_semantics_unchanged() {
    for_each_backend(|b| async move {
        let d = b.dir.clone();
    let one = d.create_domain("one.test").await.unwrap();
    let two = d.create_domain("two.test").await.unwrap();
    let a1 = d.create_account("pat", one, None, None).await.unwrap();
    let a2 = d.create_account("pat", two, None, None).await.unwrap();
    let a3 = d.create_account("member", one, None, None).await.unwrap();
    d.add_alias("al", one, a1).await.unwrap();
    let list = d.create_list("team", one).await.unwrap();
    d.add_list_member(list, Some(a3), None).await.unwrap();
    d.add_list_member(list, None, Some("x@ext.test"))
        .await
        .unwrap();
    // account, alias and list kinds
    assert_eq!(d.resolve("pat@one.test").await.unwrap(), Resolved::Account(a1));
    assert_eq!(d.resolve("pat@two.test").await.unwrap(), Resolved::Account(a2));
    assert_eq!(d.resolve("AL@One.test").await.unwrap(), Resolved::Account(a1));
    assert_eq!(
        d.resolve("team@one.test").await.unwrap(),
        Resolved::List(vec![
            Resolved::Account(a3),
            Resolved::External("x@ext.test".into())
        ]),
        "members in insertion order"
    );
    // Cross-domain isolation: a name that exists only in the other domain is not found.
    for addr in [
        "al@two.test",
        "team@two.test",
        "nobody@one.test",
        "pat@three.test",
    ] {
        assert!(
            matches!(d.resolve(addr).await, Err(Error::NotFound(_))),
            "{addr}"
        );
    }
    // An empty list resolves to an empty expansion.
    d.create_list("empty", one).await.unwrap();
    assert_eq!(
        d.resolve("empty@one.test").await.unwrap(),
        Resolved::List(vec![])
    );
    })
    .await;
}
