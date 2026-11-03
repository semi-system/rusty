use std::path::PathBuf;
use std::process::Command;

use compose_check::check_path;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn good_compose_passes() {
    assert_eq!(check_path(&fixture("good.yaml")).unwrap(), None);
}

#[test]
fn bad_fixtures_fail_with_the_expected_rule() {
    let cases = [
        ("bad_port_on_backend.yaml", "EDGE_ONLY_PORTS"),
        ("bad_docker_sock.yaml", "NO_RUNTIME_SOCKET"),
        ("bad_privileged.yaml", "NO_PRIVILEGED"),
        ("bad_host_network.yaml", "NO_HOST_NETWORK"),
        ("bad_root_user.yaml", "NON_ROOT_USER"),
        ("bad_writable_rootfs.yaml", "READ_ONLY_ROOTFS"),
        ("bad_missing_cap_drop.yaml", "CAP_DROP_ALL"),
        ("bad_no_new_privileges.yaml", "NO_NEW_PRIVILEGES"),
        ("bad_no_limits.yaml", "RESOURCE_LIMITS"),
        ("bad_image_not_pinned.yaml", "IMAGE_PINNED"),
        ("bad_stats_port_published.yaml", "NO_STATS_PORT"),
    ];
    for (file, rule) in cases {
        let violation = check_path(&fixture(file))
            .unwrap()
            .unwrap_or_else(|| panic!("{file} unexpectedly passed"));
        assert_eq!(violation.rule, rule, "fixture {file}");
    }
}

#[test]
fn cli_uses_documented_exit_codes_and_prints_the_rule() {
    let binary = env!("CARGO_BIN_EXE_compose-check");

    let good = Command::new(binary)
        .arg(fixture("good.yaml"))
        .output()
        .expect("run compose-check on good fixture");
    assert!(good.status.success());

    let bad = Command::new(binary)
        .arg(fixture("bad_privileged.yaml"))
        .output()
        .expect("run compose-check on bad fixture");
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("NO_PRIVILEGED"));
}
