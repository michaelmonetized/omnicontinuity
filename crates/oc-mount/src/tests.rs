use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn tailnet(peers: &str) -> Tailnet {
    Tailnet::from_json(
        format!(
            r#"{{
                "BackendState":"Running",
                "Self":{{"HostName":"local","DNSName":"local.tail.ts.net.","OS":"linux","TailscaleIPs":["100.64.0.1"],"Online":true}},
                "Peer":{{{peers}}}
            }}"#
        )
        .as_bytes(),
    )
    .unwrap()
}

struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-homes");
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakeRunner {
    existing: Vec<PathBuf>,
    sources: BTreeMap<PathBuf, String>,
    mounts: Vec<(String, PathBuf)>,
    unmounts: Vec<PathBuf>,
    discovery_error: Option<String>,
    source_error: Option<String>,
    mount_error: Option<String>,
    unmount_error: Option<String>,
}

impl FakeRunner {
    fn owned(entries: Vec<(PathBuf, &str)>) -> Self {
        let sources: BTreeMap<_, _> = entries
            .into_iter()
            .map(|(path, source)| (path, source.to_owned()))
            .collect();
        Self {
            existing: sources.keys().cloned().collect(),
            sources,
            ..Default::default()
        }
    }
}

impl Runner for FakeRunner {
    fn mounted(&self) -> Result<Vec<PathBuf>, String> {
        if let Some(error) = &self.discovery_error {
            return Err(error.clone());
        }
        Ok(self.existing.clone())
    }

    fn source(&self, mountpoint: &Path) -> Result<Option<String>, String> {
        if let Some(error) = &self.source_error {
            return Err(error.clone());
        }
        Ok(self.sources.get(mountpoint).cloned())
    }

    fn mount(&mut self, source: &str, mountpoint: &Path) -> Result<(), String> {
        assert!(mountpoint.is_dir(), "mount directory must already exist");
        assert!(!self.existing.iter().any(|path| path == mountpoint));
        if let Some(error) = &self.mount_error {
            return Err(error.clone());
        }
        self.mounts.push((source.to_owned(), mountpoint.to_owned()));
        self.existing.push(mountpoint.to_owned());
        self.sources
            .insert(mountpoint.to_owned(), source.to_owned());
        Ok(())
    }

    fn unmount(&mut self, mountpoint: &Path) -> Result<(), String> {
        if let Some(error) = &self.unmount_error {
            return Err(error.clone());
        }
        self.unmounts.push(mountpoint.to_owned());
        self.existing.retain(|path| path != mountpoint);
        self.sources.remove(mountpoint);
        Ok(())
    }
}

#[test]
fn linux_gets_only_the_remote_home() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"linux","OS":"linux","TailscaleIPs":["fd7a::2","100.64.0.2"],"Online":true}"#,
    );
    assert_eq!(
        desired(&home.0, "michael", &tailnet).unwrap(),
        vec![Desired {
            host: "linux".into(),
            source: "michael@100.64.0.2:".into(),
            mountpoint: home.0.join("Machines/linux"),
            kind: MountKind::Home,
        }]
    );
}

#[test]
fn mac_gets_home_and_mobile_documents() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"studio","OS":"MaCoS","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    assert_eq!(
        desired(&home.0, "michael", &tailnet).unwrap(),
        vec![
            Desired {
                host: "studio".into(),
                source: "michael@100.64.0.2:".into(),
                mountpoint: home.0.join("Machines/studio"),
                kind: MountKind::Home,
            },
            Desired {
                host: "studio".into(),
                source: "michael@100.64.0.2:Library/Mobile Documents".into(),
                mountpoint: home.0.join("Cloud/studio/michael"),
                kind: MountKind::Cloud,
            },
        ]
    );
}

#[test]
fn skips_self_mobile_offline_and_peers_without_tailnet_ipv4() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#"
        "self":{"HostName":"local","OS":"linux","TailscaleIPs":["100.64.0.1"],"Online":true},
        "ipad":{"HostName":"tablet","OS":"macOS iPadOS","TailscaleIPs":["100.64.0.2"],"Online":true},
        "ios":{"HostName":"phone","OS":"macOS iOS","TailscaleIPs":["100.64.0.3"],"Online":true},
        "iphone":{"HostName":"iphone","OS":"iPhone","TailscaleIPs":["100.64.0.4"],"Online":true},
        "offline":{"HostName":"sleeping","OS":"macOS","TailscaleIPs":["100.64.0.5"],"Online":false},
        "v6":{"HostName":"v6","OS":"linux","TailscaleIPs":["fd7a::2"],"Online":true},
        "lan":{"HostName":"lan","OS":"linux","TailscaleIPs":["192.168.1.2","100.63.255.255","100.128.0.1"],"Online":true},
        "missing":{"HostName":"missing","OS":"linux","Online":true}
        "#,
    );
    assert!(desired(&home.0, "michael", &tailnet).unwrap().is_empty());
}

#[test]
fn duplicate_hostnames_use_the_first_dns_label() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#"
        "a":{"HostName":"mac","DNSName":"mac.tail.ts.net.","OS":"macOS","TailscaleIPs":["100.64.0.2"],"Online":true},
        "b":{"HostName":"mac","DNSName":"mac-2.tail.ts.net.","OS":"macOS","TailscaleIPs":["100.64.0.3"],"Online":true}
        "#,
    );
    let mounts = desired(&home.0, "michael", &tailnet).unwrap();
    assert_eq!(mounts.len(), 4);
    for (name, ip) in [("mac", "100.64.0.2"), ("mac-2", "100.64.0.3")] {
        for (kind, path, remote) in [
            (MountKind::Home, format!("Machines/{name}"), ""),
            (
                MountKind::Cloud,
                format!("Cloud/{name}/michael"),
                "Library/Mobile Documents",
            ),
        ] {
            assert!(mounts.contains(&Desired {
                host: name.into(),
                source: format!("michael@{ip}:{remote}"),
                mountpoint: home.0.join(path),
                kind,
            }));
        }
    }
}

#[test]
fn duplicate_names_stay_stable_when_a_peer_goes_offline() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#"
        "a":{"HostName":"mac","DNSName":"mac-1.tail.ts.net","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":false},
        "b":{"HostName":"mac","DNSName":"mac-2.tail.ts.net","OS":"linux","TailscaleIPs":["100.64.0.3"],"Online":true}
        "#,
    );
    assert_eq!(
        desired(&home.0, "michael", &tailnet).unwrap()[0].mountpoint,
        home.0.join("Machines/mac-2")
    );
}

#[test]
fn rejects_unsafe_names_and_users() {
    let home = TestHome::new();
    for name in [
        r#""""#,
        r#"".""#,
        r#""..""#,
        r#""bad/name""#,
        r#""bad\u0000name""#,
    ] {
        let tailnet = tailnet(&format!(
            r#""a":{{"HostName":{name},"OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true}}"#
        ));
        assert!(desired(&home.0, "michael", &tailnet).is_err());
    }
    let tailnet = tailnet("");
    for user in [
        "",
        ".",
        "..",
        "../outside",
        "bad\0name",
        "-oProxyCommand=evil",
        "a@evil",
        "a:b",
        "a,b",
        "a b",
        "a\nb",
        "a\tb",
        "a\\b",
        "a'b",
        "a\"b",
        "a$(id)",
        "a`id`",
        "é",
    ] {
        assert!(desired(&home.0, user, &tailnet).is_err());
    }
}

#[test]
fn rejects_missing_unsafe_or_conflicting_dns_labels() {
    let home = TestHome::new();
    for dns in [
        "",
        ".tail.ts.net",
        "../tail",
        "bad/name.tail.ts.net",
        "one.tail.ts.net",
    ] {
        let tailnet = tailnet(&format!(
            r#"
            "a":{{"HostName":"same","DNSName":"one.tail.ts.net","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true}},
            "b":{{"HostName":"same","DNSName":"{dns}","OS":"linux","TailscaleIPs":["100.64.0.3"],"Online":true}}
            "#
        ));
        assert!(desired(&home.0, "michael", &tailnet).is_err());
    }
}

#[test]
fn offline_peer_loses_home_and_cloud_mounts() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"studio","OS":"macOS","TailscaleIPs":["100.64.0.2"],"Online":false}"#,
    );
    let mut runner = FakeRunner::owned(vec![
        (home.0.join("Machines/studio"), "michael@100.64.0.2:"),
        (
            home.0.join("Cloud/studio/michael"),
            "michael@100.64.0.2:Library/Mobile Documents",
        ),
    ]);
    let report = reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap();
    assert_eq!(
        report.unmounted,
        vec![
            home.0.join("Cloud/studio/michael"),
            home.0.join("Machines/studio"),
        ]
    );
    assert!(report.mounted.is_empty());
    assert!(report.unchanged.is_empty());
    assert!(runner.existing.is_empty());
    assert!(runner.mounts.is_empty());
}

#[test]
fn unrelated_mounts_and_managed_roots_are_kept() {
    let home = TestHome::new();
    let mut runner = FakeRunner {
        existing: vec![
            home.0.clone(),
            home.0.join("Machines"),
            home.0.join("Cloud"),
            home.0.join("Machines-backup/peer"),
            home.0.join("Cloud-backup/peer/michael"),
            home.0.join("Other/peer"),
            home.0.join("Machines/../Other/peer"),
        ],
        ..Default::default()
    };
    let existing = runner.existing.clone();
    assert_eq!(
        reconcile(&home.0, "michael", &tailnet(""), &mut runner).unwrap(),
        Report::default()
    );
    assert_eq!(runner.existing, existing);
    assert!(runner.unmounts.is_empty());
    assert!(runner.mounts.is_empty());
}

#[test]
fn mounted_desired_entries_are_unchanged() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"studio","OS":"macOS","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    let mut runner = FakeRunner::owned(vec![
        (home.0.join("Machines/studio"), "michael@100.64.0.2:"),
        (
            home.0.join("Cloud/studio/michael"),
            "michael@100.64.0.2:Library/Mobile Documents",
        ),
    ]);
    runner.existing.push(home.0.join("Machines/studio"));
    let report = reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap();
    assert_eq!(
        report.unchanged,
        vec![
            home.0.join("Cloud/studio/michael"),
            home.0.join("Machines/studio"),
        ]
    );
    assert!(report.mounted.is_empty());
    assert!(report.unmounted.is_empty());
    assert!(runner.mounts.is_empty());
    assert!(runner.unmounts.is_empty());
    assert!(!home.0.join("Machines").exists());
    assert!(!home.0.join("Cloud").exists());
}

#[test]
fn creates_directories_mounts_missing_entries_and_sorts_reports() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#"
        "a":{"HostName":"z-linux","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true},
        "b":{"HostName":"a-mac","OS":"macOS","TailscaleIPs":["100.64.0.3"],"Online":true}
        "#,
    );
    let mut runner = FakeRunner::default();
    let report = reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap();
    assert_eq!(
        report.mounted,
        vec![
            home.0.join("Cloud/a-mac/michael"),
            home.0.join("Machines/a-mac"),
            home.0.join("Machines/z-linux"),
        ]
    );
    assert_eq!(
        runner.mounts,
        vec![
            ("michael@100.64.0.3:".into(), home.0.join("Machines/a-mac")),
            (
                "michael@100.64.0.3:Library/Mobile Documents".into(),
                home.0.join("Cloud/a-mac/michael"),
            ),
            (
                "michael@100.64.0.2:".into(),
                home.0.join("Machines/z-linux")
            ),
        ]
    );
    assert!(report.unmounted.is_empty());
    assert!(report.unchanged.is_empty());
    assert_eq!(
        reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap(),
        Report {
            unchanged: report.mounted,
            ..Default::default()
        }
    );
    assert_eq!(runner.mounts.len(), 3);
}

#[test]
fn stale_nested_mounts_are_unmounted_before_their_parents() {
    let home = TestHome::new();
    let parent = home.0.join("Cloud/old");
    let child = parent.join("michael");
    let mut runner = FakeRunner::owned(vec![
        (parent.clone(), "michael@100.64.0.2:"),
        (child.clone(), "michael@100.64.0.2:Library/Mobile Documents"),
    ]);
    runner.existing.push(child.clone());
    let report = reconcile(&home.0, "michael", &tailnet(""), &mut runner).unwrap();
    assert_eq!(runner.unmounts, vec![child.clone(), parent.clone()]);
    assert_eq!(report.unmounted, vec![parent, child]);
}

#[test]
fn runner_errors_are_returned() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"linux","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    let mut runner = FakeRunner {
        discovery_error: Some("discovery failed".into()),
        ..Default::default()
    };
    assert_eq!(
        reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap_err(),
        "discovery failed"
    );
    assert!(!home.0.join("Machines").exists());

    let mut runner = FakeRunner {
        mount_error: Some("mount failed".into()),
        ..Default::default()
    };
    assert_eq!(
        reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap_err(),
        "mount failed"
    );
    let mut runner = FakeRunner::owned(vec![(
        home.0.join("Machines/offline"),
        "michael@100.64.0.2:",
    )]);
    runner.unmount_error = Some("unmount failed".into());
    assert_eq!(
        reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap_err(),
        "unmount failed"
    );
    assert!(runner.mounts.is_empty());
}

#[test]
fn directory_creation_failure_prevents_mount() {
    let home = TestHome::new();
    fs::write(home.0.join("Machines"), b"not a directory").unwrap();
    let tailnet = tailnet(
        r#""a":{"HostName":"linux","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    let mut runner = FakeRunner::default();
    assert!(
        reconcile(&home.0, "michael", &tailnet, &mut runner)
            .unwrap_err()
            .starts_with("Mount path must use real directories:")
    );
    assert!(runner.mounts.is_empty());
}

#[test]
fn sshfs_arguments_preserve_spaces_and_use_key_authentication() {
    let source = "michael@100.64.0.2:Library/Mobile Documents";
    let mountpoint = Path::new("/home/michael/Cloud/Mac Studio/michael");
    let command = sshfs_command(source, mountpoint);
    assert_eq!(command.get_program(), "sshfs");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        vec![
            "-o",
            "reconnect,ServerAliveInterval=15,ServerAliveCountMax=3,ConnectTimeout=8,BatchMode=yes",
            "-o",
            "fsname=oc-mount:michael@100.64.0.2:Library/Mobile Documents",
            "--",
            source,
            mountpoint.to_str().unwrap(),
        ]
    );
}

#[test]
fn mountinfo_decodes_spaces_tabs_newlines_and_backslashes() {
    assert_eq!(
        parse_mountinfo(
            br"12 1 0:1 / /home/michael/Machines/Mac\040Studio rw - fuse.sshfs peer: rw
13 1 0:2 / /home/michael/Cloud/a\011b\012c\134d rw - fuse.sshfs peer:path rw
"
        )
        .unwrap(),
        BTreeMap::from([
            (PathBuf::from("/home/michael/Machines/Mac Studio"), None),
            (PathBuf::from("/home/michael/Cloud/a\tb\nc\\d"), None),
        ])
    );
}

#[test]
fn malformed_mountinfo_is_an_error() {
    for line in [
        "malformed",
        "1 2 0:1 / /broken\\ rw - fuse.sshfs source rw",
        "1 2 0:1 / /broken\\00 rw - fuse.sshfs source rw",
        "1 2 0:1 / /broken\\999 rw - fuse.sshfs source rw",
    ] {
        assert!(parse_mountinfo(line.as_bytes()).is_err());
    }
}

#[cfg(unix)]
#[test]
fn mountinfo_preserves_non_utf8_paths() {
    use std::os::unix::ffi::OsStrExt;
    let mounts =
        parse_mountinfo(b"1 2 0:1 / /home/michael/Machines/\xff rw - fuse.sshfs peer: rw\n")
            .unwrap();
    assert_eq!(
        mounts.keys().next().unwrap().as_os_str().as_bytes(),
        b"/home/michael/Machines/\xff"
    );
}

#[test]
fn unsupported_operating_systems_are_not_assumed_to_have_ssh_disks() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#"
        "android":{"HostName":"android","OS":"android","TailscaleIPs":["100.64.0.2"],"Online":true},
        "windows":{"HostName":"windows","OS":"windows","TailscaleIPs":["100.64.0.3"],"Online":true},
        "unknown":{"HostName":"unknown","OS":"","TailscaleIPs":["100.64.0.4"],"Online":true},
        "mixed":{"HostName":"mixed","OS":"macOS Android","TailscaleIPs":["100.64.0.5"],"Online":true}
        "#,
    );
    let mut runner = FakeRunner::default();
    assert_eq!(
        reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap(),
        Report::default()
    );
    assert!(runner.mounts.is_empty());
    assert!(!home.0.join("Machines").exists());
}

#[test]
fn changed_ip_is_remounted_and_never_reported_unchanged() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"linux","OS":"linux","TailscaleIPs":["100.64.0.3"],"Online":true}"#,
    );
    let path = home.0.join("Machines/linux");
    let mut runner = FakeRunner::owned(vec![(path.clone(), "michael@100.64.0.2:")]);
    let report = reconcile(&home.0, "michael", &tailnet, &mut runner).unwrap();
    assert_eq!(
        report,
        Report {
            mounted: vec![path.clone()],
            unmounted: vec![path.clone()],
            unchanged: vec![],
        }
    );
    assert_eq!(runner.unmounts, vec![path.clone()]);
    assert_eq!(runner.mounts, vec![("michael@100.64.0.3:".into(), path)]);
}

#[test]
fn unowned_mounts_inside_managed_directories_are_kept() {
    let home = TestHome::new();
    let owned = home.0.join("Machines/offline");
    let foreign = home.0.join("Machines/manual-backup");
    let other = home.0.join("Other/owned-but-outside");
    let mut runner = FakeRunner::owned(vec![
        (owned.clone(), "michael@100.64.0.2:"),
        (other.clone(), "michael@100.64.0.3:"),
    ]);
    runner.existing.push(foreign.clone());
    let report = reconcile(&home.0, "michael", &tailnet(""), &mut runner).unwrap();
    assert_eq!(report.unmounted, vec![owned]);
    assert_eq!(runner.existing, vec![other, foreign]);
}

#[test]
fn path_only_runners_cannot_claim_unchanged_or_remove_unknown_mounts() {
    struct PathsOnly(Vec<PathBuf>);
    impl Runner for PathsOnly {
        fn mounted(&self) -> Result<Vec<PathBuf>, String> {
            Ok(self.0.clone())
        }
        fn mount(&mut self, _: &str, _: &Path) -> Result<(), String> {
            panic!("unknown mounts must not be replaced");
        }
        fn unmount(&mut self, _: &Path) -> Result<(), String> {
            panic!("unknown mounts must not be removed");
        }
    }
    let home = TestHome::new();
    let mut runner = PathsOnly(vec![home.0.join("Machines/linux")]);
    assert_eq!(
        reconcile(&home.0, "michael", &tailnet(""), &mut runner).unwrap(),
        Report::default()
    );
    let tailnet = tailnet(
        r#""a":{"HostName":"linux","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    assert!(
        reconcile(&home.0, "michael", &tailnet, &mut runner)
            .unwrap_err()
            .starts_with("Cannot verify mount ownership and source")
    );
}

#[test]
fn source_discovery_failure_prevents_changes() {
    let home = TestHome::new();
    let mut runner = FakeRunner {
        existing: vec![home.0.join("Machines/old")],
        source_error: Some("source lookup failed".into()),
        ..Default::default()
    };
    assert_eq!(
        reconcile(&home.0, "michael", &tailnet(""), &mut runner).unwrap_err(),
        "source lookup failed"
    );
    assert!(runner.mounts.is_empty());
    assert!(runner.unmounts.is_empty());
}

#[test]
fn retained_child_mount_prevents_removal_of_its_owned_parent() {
    let home = TestHome::new();
    let parent = home.0.join("Machines/old");
    let child = parent.join("backup");
    let mut runner = FakeRunner::owned(vec![(parent, "michael@100.64.0.2:")]);
    runner.existing.push(child);
    assert!(
        reconcile(&home.0, "michael", &tailnet(""), &mut runner)
            .unwrap_err()
            .contains("retained mount inside")
    );
    assert!(runner.unmounts.is_empty());
}

#[test]
fn retained_parent_mount_prevents_mounting_inside_it() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"mac","OS":"macOS","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    let mut runner = FakeRunner {
        existing: vec![home.0.join("Cloud/mac")],
        ..Default::default()
    };
    assert!(
        reconcile(&home.0, "michael", &tailnet, &mut runner)
            .unwrap_err()
            .starts_with("Retained mount contains")
    );
    assert!(runner.mounts.is_empty());
    assert!(runner.unmounts.is_empty());
    assert!(!home.0.join("Machines").exists());
}

#[cfg(unix)]
#[test]
fn symlinked_roots_parents_and_mount_directories_are_rejected() {
    use std::os::unix::fs::symlink;
    let tailnet = tailnet(
        r#"
        "a":{"HostName":"linux","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true},
        "b":{"HostName":"mac","OS":"macOS","TailscaleIPs":["100.64.0.3"],"Online":true}
        "#,
    );
    for relative in [
        "Machines",
        "Machines/linux",
        "Cloud",
        "Cloud/mac",
        "Cloud/mac/michael",
    ] {
        let home = TestHome::new();
        let outside = TestHome::new();
        fs::write(outside.0.join("keep"), b"untouched").unwrap();
        let link = home.0.join(relative);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&outside.0, link).unwrap();
        let mut runner = FakeRunner::default();
        assert!(
            reconcile(&home.0, "michael", &tailnet, &mut runner)
                .unwrap_err()
                .starts_with("Mount path must use real directories:")
        );
        assert!(runner.mounts.is_empty());
        assert!(runner.unmounts.is_empty());
        assert_eq!(fs::read(outside.0.join("keep")).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 1);
    }
}

#[cfg(unix)]
#[test]
fn symlinked_home_and_stale_mount_ancestors_are_rejected() {
    use std::os::unix::fs::symlink;
    let home = TestHome::new();
    let outside = TestHome::new();
    symlink(&outside.0, home.0.join("alias")).unwrap();
    let tailnet_with_peer = tailnet(
        r#""a":{"HostName":"linux","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    let mut runner = FakeRunner::default();
    assert!(
        reconcile(
            &home.0.join("alias"),
            "michael",
            &tailnet_with_peer,
            &mut runner
        )
        .is_err()
    );
    assert!(!outside.0.join("Machines").exists());
    symlink(&outside.0, home.0.join("Machines")).unwrap();
    let mut runner = FakeRunner::owned(vec![(home.0.join("Machines/old"), "michael@100.64.0.2:")]);
    assert!(reconcile(&home.0, "michael", &tailnet(""), &mut runner).is_err());
    assert!(runner.unmounts.is_empty());
}

#[test]
fn mount_paths_reject_relative_paths_and_parent_traversal() {
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"linux","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    for path in [
        PathBuf::from("relative-home"),
        home.0.join("missing/../other"),
    ] {
        let mut runner = FakeRunner::default();
        assert!(reconcile(&path, "michael", &tailnet, &mut runner).is_err());
        assert!(runner.mounts.is_empty());
        assert!(runner.unmounts.is_empty());
    }
    assert!(!home.0.join("missing").exists());
}

#[test]
fn mountinfo_requires_tagged_full_sshfs_mounts_for_source_ownership() {
    let mounts = parse_mountinfo(
        br"12 1 0:1 / /owned-home rw shared:2 - fuse.sshfs oc-mount:michael@100.64.0.2: rw
13 1 0:2 / /owned-cloud rw - fuse.sshfs oc-mount:michael@100.64.0.3:Library/Mobile\040Documents rw
14 1 0:3 / /manual rw - fuse.sshfs michael@100.64.0.2: rw
15 1 0:4 / /wrong-fs rw - ext4 oc-mount:michael@100.64.0.2: rw
16 1 0:5 /subdir /bound-subdir rw - fuse.sshfs oc-mount:michael@100.64.0.2: rw
17 1 0:6 / /wrong-source rw - fuse.sshfs oc-mount:michael@192.168.1.2: rw
18 1 0:7 / /wrong-directory rw - fuse.sshfs oc-mount:michael@100.64.0.2:private rw
19 1 0:8 / /wrong-user rw - fuse.sshfs oc-mount:bad@user@100.64.0.2: rw
",
    )
    .unwrap();
    assert_eq!(
        mounts[Path::new("/owned-home")].as_deref(),
        Some("michael@100.64.0.2:")
    );
    assert_eq!(
        mounts[Path::new("/owned-cloud")].as_deref(),
        Some("michael@100.64.0.3:Library/Mobile Documents")
    );
    assert_eq!(mounts.values().filter(|source| source.is_some()).count(), 2);
}

#[test]
fn stacked_mounts_are_ambiguous_and_rejected() {
    assert!(
        parse_mountinfo(
            br"12 1 0:1 / /same rw - fuse.sshfs oc-mount:michael@100.64.0.2: rw
13 1 0:2 / /same rw - tmpfs unrelated rw
"
        )
        .unwrap_err()
        .starts_with("Multiple mounts at")
    );
}

#[test]
fn removal_guard_requires_the_same_current_owned_source() {
    let path = PathBuf::from("/home/michael/Machines/linux");
    let expected = BTreeMap::from([(path.clone(), Some("michael@100.64.0.2:".into()))]);
    assert!(verify_owned_source(&path, &expected, &expected).is_ok());
    for current in [
        BTreeMap::new(),
        BTreeMap::from([(path.clone(), None)]),
        BTreeMap::from([(path.clone(), Some("michael@100.64.0.3:".into()))]),
    ] {
        assert!(verify_owned_source(&path, &expected, &current).is_err());
    }
    let unknown = BTreeMap::from([(path.clone(), None)]);
    assert!(verify_owned_source(&path, &unknown, &unknown).is_err());
}

#[test]
fn source_changed_during_reconciliation_is_not_reported_unchanged() {
    use std::cell::Cell;
    struct ChangingRunner {
        path: PathBuf,
        reads: Cell<usize>,
    }
    impl Runner for ChangingRunner {
        fn mounted(&self) -> Result<Vec<PathBuf>, String> {
            Ok(vec![self.path.clone()])
        }
        fn source(&self, _: &Path) -> Result<Option<String>, String> {
            let reads = self.reads.get();
            self.reads.set(reads + 1);
            Ok(Some(if reads == 0 {
                "michael@100.64.0.2:".into()
            } else {
                "michael@100.64.0.3:".into()
            }))
        }
        fn mount(&mut self, _: &str, _: &Path) -> Result<(), String> {
            panic!("changed state must be reconciled from a fresh snapshot");
        }
        fn unmount(&mut self, _: &Path) -> Result<(), String> {
            panic!("changed state must be reconciled from a fresh snapshot");
        }
    }
    let home = TestHome::new();
    let tailnet = tailnet(
        r#""a":{"HostName":"linux","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":true}"#,
    );
    let mut runner = ChangingRunner {
        path: home.0.join("Machines/linux"),
        reads: Cell::new(0),
    };
    assert!(
        reconcile(&home.0, "michael", &tailnet, &mut runner)
            .unwrap_err()
            .starts_with("Mount source changed during reconciliation")
    );
}
