use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use exodys::{
    import::{MAX_BYTES, Source, parse},
    session::{Job, Session},
};

const CLASH: &str = "proxies:\n  - {name: Singapore, type: socks5, server: socks.example.test, port: 1080}\n  - {name: Tokyo, type: http, server: http.example.test, port: 8080}\n  - {name: Unsupported, type: wireguard, server: vpn.example.test, port: 51820}\n";

#[test]
fn formats_normalize_to_independent_nodes() {
    let cases = [
        ("Clash YAML", CLASH.to_owned(), "Singapore", "SOCKS5", 2, 1),
        ("sing-box JSON", r#"{"outbounds":[{"type":"direct","tag":"direct"},{"type":"socks","tag":"Local","server":"::1","server_port":1080},{"type":"trojan","tag":"a","server":"test.invalid","server_port":443,"tls":{"certificate_path":"/tmp/unread"}}]}"#.into(), "Local", "SOCKS5", 1, 2),
        ("URI batch", "socks5://127.0.0.1:1080#Local%20gateway\nunsupported://abc".into(), "Local gateway", "SOCKS5", 1, 1),
        ("Base64 batch", STANDARD.encode("trojan://fixture@proxy.example.test:443?sni=proxy.example.test#TLS"), "TLS", "Trojan", 1, 0),
        ("Shadowsocks", "ss://YWVzLTEyOC1nY206Zml4dHVyZQ@proxy.example.test:443#SIP002".into(), "SIP002", "Shadowsocks", 1, 0),
        ("VLESS", "vless://dcbbc7c9-7dbc-4da3-8105-9d3e89e38bc2@proxy.example.test:443?security=tls&type=ws&path=%2Fsocket#WebSocket".into(), "WebSocket", "VLESS", 1, 0),
    ];
    for (format, text, name, protocol, count, skipped) in cases {
        let result = parse(text.as_bytes(), format.into(), None)
            .unwrap_or_else(|error| panic!("{format}: {error}"));
        assert_eq!(result.nodes.len(), count, "{format}");
        assert_eq!(result.nodes[0].name, name, "{format}");
        assert_eq!(result.nodes[0].protocol(), protocol, "{format}");
        assert_eq!(result.skipped, skipped, "{format}");
    }
}

#[test]
fn malformed_and_oversize_inputs_are_rejected() {
    for text in [
        "",
        "{",
        "proxies: []",
        "socks5://localhost:0",
        "vless://fixture@localhost:443?type=xhttp",
    ] {
        assert!(
            parse(text.as_bytes(), "Fixture".into(), None).is_err(),
            "{text}"
        );
    }
    assert!(parse(&vec![b'x'; MAX_BYTES + 1], "Fixture".into(), None).is_err());
}

#[test]
fn workspace_persists_profile_selection_and_port() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::open(directory.path().into()).unwrap();
    assert!(
        Session::open(directory.path().into()).is_err(),
        "duplicate store writer"
    );
    let id = session
        .run(Job::Import(Source::Text(CLASH.into())))
        .unwrap()
        .focus
        .unwrap();
    let snapshot = session.snapshot();
    let profile = snapshot.catalog.profile(id).unwrap();
    assert_eq!(profile.search("socks Singapore").at(0), Some(0));
    assert_eq!(profile.search("tokyo HTTP").at(0), Some(1));
    assert!(profile.search("absent").is_empty());
    session
        .run(Job::Select(exodys::catalog::NodeRef {
            profile: id,
            node: profile.nodes[1].id.clone(),
        }))
        .unwrap();
    session
        .run(Job::Rename(id, "Example routes".into()))
        .unwrap();
    session.run(Job::SetPort(21991)).unwrap();
    session
        .run(Job::SetRouting("IP-CIDR,10.0.0.0/8,DIRECT".into()))
        .unwrap();
    assert!(
        session
            .run(Job::SetRouting("IP-CIDR,broken,DIRECT".into()))
            .is_err()
    );
    drop(session);

    let mut reopened = Session::open(directory.path().into()).unwrap();
    let snapshot = reopened.snapshot();
    assert_eq!(snapshot.preferences.port, 21991);
    assert_eq!(
        snapshot.preferences.routing_rules,
        "IP-CIDR,10.0.0.0/8,DIRECT"
    );
    assert_eq!(snapshot.catalog.profile(id).unwrap().name, "Example routes");
    assert_eq!(snapshot.node().unwrap().name, "Tokyo");
    assert!(!snapshot.connected);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in ["profiles.json", "preferences.json"] {
            let mode = std::fs::metadata(directory.path().join(path))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{path}");
        }
    }
    reopened.run(Job::Remove(id)).unwrap();
    assert!(reopened.snapshot().node().is_none());
}

#[test]
fn settings_transfer_preserves_selection_and_recovers_from_invalid_files() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("preferences.json"),
        r#"{"selected":null,"port":21992}"#,
    )
    .unwrap();
    let mut session = Session::open(workspace.clone()).unwrap();
    assert!(session.snapshot().preferences.routing_rules.is_empty());
    assert!(
        !session.snapshot().preferences.system_proxy,
        "old settings must not opt in"
    );
    assert_eq!(
        session.run(Job::SetSystemProxy(true)).is_ok(),
        cfg!(target_os = "macos"),
    );
    session
        .run(Job::Import(Source::Text(CLASH.into())))
        .unwrap();
    let rules = "DOMAIN,example.org,DIRECT";
    session.run(Job::SetRouting(rules.into())).unwrap();
    let state = session.snapshot();
    let backup = dir.path().join("portable.json");
    session.run(Job::ExportSettings(backup.clone())).unwrap();
    assert!(
        session
            .run(Job::ExportSettings(workspace.join("profiles.json")))
            .is_err()
    );
    session.run(Job::SetRouting(String::new())).unwrap();
    session.run(Job::SetPort(21993)).unwrap();
    session.run(Job::RestoreSettings(backup.clone())).unwrap();
    std::fs::write(&backup, "{}").unwrap();
    assert!(session.run(Job::RestoreSettings(backup)).is_err());
    drop(session);
    let restored = Session::open(workspace).unwrap().snapshot();
    assert!(state.preferences == restored.preferences && state.catalog == restored.catalog);
}

fn respond(listener: &TcpListener, status: &str, body: &str) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                let length = stream.read(&mut request).unwrap();
                assert!(request[..length].starts_with(b"GET "));
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                return;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(error) => panic!("subscription request did not arrive: {error}"),
        }
    }
}

#[test]
fn encrypted_backup_restores_an_independent_workspace_atomically() {
    use std::fs;

    use exodys::catalog::{NodeRef, PolicyMode};
    use zeroize::Zeroizing;

    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source");
    let target_path = directory.path().join("target");
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let archive = shared.join("transfer.exodysbackup");
    let second_archive = shared.join("second.exodysbackup");
    let passphrase = || Zeroizing::new("test-only backup passphrase".to_owned());
    let token = "synthetic-subscription-token";
    let login = "synthetic-node-password";
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!(
        "http://{}/fixture?token={token}",
        listener.local_addr().unwrap()
    );
    let content = format!("socks5://user:{login}@localhost:9#Backup");
    let mut source = Session::open(source_path.clone()).unwrap();
    let profile = std::thread::scope(|scope| {
        scope.spawn(|| respond(&listener, "200 OK", &content));
        source
            .run(Job::Import(Source::Subscription(url)))
            .unwrap()
            .focus
            .unwrap()
    });
    let node = NodeRef {
        profile,
        node: source.snapshot().node().unwrap().id.clone(),
    };
    source.run(Job::CreatePolicy("work".into())).unwrap();
    source
        .run(Job::SetPolicyMember("work".into(), node.clone(), true))
        .unwrap();
    source
        .run(Job::SetPolicyMode(
            "work".into(),
            PolicyMode::Manual(Some(node.clone())),
        ))
        .unwrap();
    source.run(Job::SetAutoMember(node, true)).unwrap();
    source.run(Job::SetRefreshHours(profile, 6)).unwrap();
    source.run(Job::SetScheduledRefresh(true)).unwrap();
    source.run(Job::SetPort(22300)).unwrap();
    source
        .run(Job::SetRouting("MATCH,GROUP:work".into()))
        .unwrap();
    let expected = source.snapshot();

    for path in [&archive, &second_archive] {
        source
            .run(Job::ExportBackup(path.clone(), passphrase()))
            .unwrap();
    }
    assert!(
        source
            .run(Job::ExportBackup(
                source_path.join("nested.exodysbackup"),
                passphrase()
            ))
            .is_err()
    );
    let bytes = fs::read(&archive).unwrap();
    let second = fs::read(&second_archive).unwrap();
    assert_eq!(bytes.len(), second.len());
    assert_ne!(&bytes[8..36], &second[8..36]);
    for fixture in [token, login] {
        assert!(
            !bytes
                .windows(fixture.len())
                .any(|window| window == fixture.as_bytes())
        );
    }
    let mut damaged = second;
    *damaged.last_mut().unwrap() ^= 1;
    fs::write(&second_archive, damaged).unwrap();

    let mut target = Session::open(target_path.clone()).unwrap();
    #[cfg(target_os = "macos")]
    target.run(Job::SetSystemProxy(true)).unwrap();
    target.run(Job::Import(Source::Text(CLASH.into()))).unwrap();
    let original = target.snapshot();
    let marker = target_path.join("active-workspace");
    assert!(
        target
            .run(Job::RestoreBackup(
                archive.clone(),
                Zeroizing::new("incorrect passphrase".to_owned())
            ))
            .is_err()
    );
    assert!(
        target
            .run(Job::RestoreBackup(second_archive, passphrase()))
            .is_err()
    );
    assert!(!marker.exists());

    // A directory at the marker path forces the final atomic publish to fail.
    fs::create_dir(&marker).unwrap();
    assert!(
        target
            .run(Job::RestoreBackup(archive.clone(), passphrase()))
            .is_err()
    );
    assert!(target.snapshot().catalog == original.catalog);
    assert!(target.snapshot().preferences == original.preferences);
    assert!(!fs::read_dir(&target_path).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".restored-")
    }));
    fs::remove_dir(&marker).unwrap();
    assert!(Session::open(target_path.clone()).is_err());

    target
        .run(Job::RestoreBackup(archive.clone(), passphrase()))
        .unwrap();
    let first_generation = target.data_directory().to_owned();
    target
        .run(Job::RestoreBackup(archive, passphrase()))
        .unwrap();
    assert_ne!(first_generation, target.data_directory());
    for managed in [
        marker,
        target_path.join("workspace.lock"),
        target_path.join("profiles.json"),
        first_generation.join("profiles.json"),
        target.data_directory().join("preferences.json"),
    ] {
        assert!(target.run(Job::ExportSettings(managed)).is_err());
    }
    let case_alias = target_path.with_file_name("TARGET");
    if case_alias.is_dir() {
        assert!(
            target
                .run(Job::ExportSettings(case_alias.join("ACTIVE-WORKSPACE")))
                .is_err()
        );
        assert!(
            target
                .run(Job::ExportBackup(
                    case_alias.join("nested.exodysbackup"),
                    passphrase()
                ))
                .is_err()
        );
    }
    assert!(Session::open(target_path.clone()).is_err());
    drop(target);

    let reopened = Session::open(target_path).unwrap().snapshot();
    let mut preferences = expected.preferences;
    preferences.system_proxy = cfg!(target_os = "macos");
    assert!(reopened.preferences == preferences);
    assert!(reopened.catalog == expected.catalog);
    assert_eq!(reopened.node().unwrap().name, "Backup");
    assert!(!reopened.connected);
}

#[test]
fn scheduled_refresh_is_opt_in_and_backs_off_without_discarding_nodes() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let link = format!("http://{}/scheduled", listener.local_addr().unwrap());
    let path = directory.path().to_owned();
    let mut session = Session::open(path.clone()).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| respond(&listener, "200 OK", CLASH));
        session
            .run(Job::Import(Source::Subscription(link)))
            .unwrap();
    });
    let initial = session.snapshot();
    let profile = &initial.catalog.profiles[0];
    let id = profile.id;
    assert!(session.run(Job::SetRefreshHours(id, 721)).is_err());
    assert!(profile.last_subscription_update.is_some());
    session.run(Job::SetRefreshHours(id, 0)).unwrap();
    session
        .run(Job::Rename(id, "Scheduled fixture".into()))
        .unwrap();
    assert_eq!(
        session.snapshot().catalog.profiles[0].last_subscription_update,
        profile.last_subscription_update
    );
    drop(session);
    let file = path.join("profiles.json");
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    old["profiles"][0]
        .as_object_mut()
        .unwrap()
        .remove("last_subscription_update");
    std::fs::write(&file, old.to_string()).unwrap();
    let mut session = Session::open(path.clone()).unwrap();
    assert!(
        session.run(Job::RefreshDue).unwrap().message.is_none(),
        "off by default"
    );
    session.run(Job::SetScheduledRefresh(true)).unwrap();
    assert_eq!(
        session.snapshot().catalog.profiles[0].refresh_hours,
        Some(0)
    );
    assert!(session.run(Job::RefreshDue).unwrap().message.is_none());
    session.run(Job::SetRefreshHours(id, 6)).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| respond(&listener, "503 Unavailable", ""));
        assert!(session.run(Job::RefreshDue).is_err());
    });
    assert_eq!(session.snapshot().catalog.profiles[0].nodes, profile.nodes);
    assert!(
        session.run(Job::RefreshDue).unwrap().message.is_none(),
        "failed refresh backs off"
    );
    std::thread::scope(|scope| {
        scope.spawn(|| respond(&listener, "200 OK", "socks5://localhost:9099#Scheduled"));
        session.run(Job::Refresh(id)).unwrap();
    });
    drop(session);
    let mut session = Session::open(path).unwrap();
    assert!(session.snapshot().preferences.scheduled_refresh);
    assert_eq!(
        session.snapshot().catalog.profiles[0].refresh_hours,
        Some(6)
    );
    assert_eq!(session.snapshot().node().unwrap().name, "Scheduled");
    assert!(
        session.run(Job::RefreshDue).unwrap().message.is_none(),
        "recent successful update survives reopen"
    );
}

#[test]
fn refresh_keeps_last_good_subscription_and_selected_node() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::open(directory.path().into()).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let link = format!("http://{}/fixture", listener.local_addr().unwrap());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            respond(&listener, "200 OK", CLASH);
            respond(&listener, "503 Unavailable", "");
            let payload = "proxies:\n  - {name: Sydney, type: socks5, server: sydney.example.test, port: 1080}\n  - {name: Singapore, type: socks5, server: socks.example.test, port: 2080}\n";
            respond(&listener, "200 OK", payload);
        });
        let id = session
            .run(Job::Import(Source::Text(link)))
            .unwrap()
            .focus
            .unwrap();
        let member = session.snapshot().preferences.selected.unwrap();
        session
            .run(Job::SetAutoMember(member.clone(), true))
            .unwrap();
        let before = session.snapshot();
        assert!(session.run(Job::Refresh(id)).is_err());
        assert_eq!(session.snapshot().preferences.auto_members, [member]);
        assert!(std::sync::Arc::ptr_eq(
            &before.catalog,
            &session.snapshot().catalog
        ));
        session.run(Job::Refresh(id)).unwrap();
        let after = session.snapshot();
        assert_eq!(after.node().unwrap().name, "Singapore");
        assert_eq!(after.node().unwrap().endpoint(), "socks.example.test:2080");
        assert!(
            after.preferences.auto_members.is_empty(),
            "replaced endpoint is not an approved group member"
        );
        assert_eq!(after.catalog.profile(id).unwrap().nodes[0].name, "Sydney");
    });
}
