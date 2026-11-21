#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use exodys::{
    catalog::NodeRef,
    import::Source,
    session::{Job, Session},
};

fn import(session: &mut Session, text: &str) -> NodeRef {
    let profile = session
        .run(Job::Import(Source::Text(text.into())))
        .unwrap()
        .focus
        .unwrap();
    let snapshot = session.snapshot();
    NodeRef {
        profile,
        node: snapshot.catalog.profile(profile).unwrap().nodes[0]
            .id
            .clone(),
    }
}

#[test]
fn preflight_rejects_unusable_resolver_addresses_and_egress_overrides() {
    let workspace = tempfile::tempdir().unwrap();
    let engine = workspace.path().join("no-engine");
    let mut session = Session::open_with_engine(workspace.path().into(), Some(engine)).unwrap();
    import(&mut session, "socks5://192.0.2.12:1080#Fixture");
    for address in ["0.0.0.0", "::1", "::ffff:127.0.0.1", "fe80::1", "224.0.0.1"] {
        let error = session
            .check_tun_config(address.parse().unwrap())
            .unwrap_err();
        assert!(error.contains("DNS"), "{address}: {error}");
    }
    let bound = import(
        &mut session,
        r#"{"outbounds":[{"type":"socks","server":"192.0.2.13","server_port":1080,"bind_interface":"en0"}]}"#,
    );
    session.run(Job::SetAutoMember(bound, true)).unwrap();
    session.run(Job::SetAutoEnabled(true)).unwrap();
    let error = session
        .check_tun_config("192.0.2.53".parse().unwrap())
        .unwrap_err();
    assert!(error.contains("网卡"), "{error}");
}

fn tun_command(workspace: &Path, flag: &str, address: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_exodys"))
        .env("EXODYS_DATA_DIR", workspace)
        .args([flag, address])
        .output()
        .unwrap()
}

#[test]
#[ignore = "requires locally installed sing-box 1.14 or later; calls only check"]
fn cli_checks_tun_policy_with_real_sing_box() {
    let workspace = tempfile::tempdir().unwrap();
    let mut session = Session::open(workspace.path().into()).unwrap();
    let a = import(&mut session, "socks5://proxy-a.example:1080#Fixture-A");
    let b = import(&mut session, "socks5://[2001:db8::12]:1080#Fixture-B");
    session.run(Job::SetAutoMember(a, true)).unwrap();
    session.run(Job::SetAutoEnabled(true)).unwrap();
    session.run(Job::CreatePolicy("office".into())).unwrap();
    session
        .run(Job::SetPolicyMember("office".into(), b, true))
        .unwrap();
    session
        .run(Job::SetPolicyMode(
            "office".into(),
            exodys::catalog::PolicyMode::Automatic,
        ))
        .unwrap();
    session
        .run(Job::SetRouting(
            "DOMAIN,fixture.test,GROUP:office\nIP-CIDR6,2001:db8::/32,DIRECT\nMATCH,PROXY".into(),
        ))
        .unwrap();
    drop(session);
    for address in ["192.0.2.53", "2001:db8::53"] {
        let result = tun_command(workspace.path(), "--check-tun-config", address);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("未启动隧道"));
    }
    assert!(!workspace.path().join("tun-dns.json").exists());
    assert!(!fs::read_dir(workspace.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tun-candidate-")
    }));
}

#[test]
fn connect_requires_device_dns_and_leaves_the_session_disconnected() {
    let workspace = tempfile::tempdir().unwrap();
    let mut session = Session::open(workspace.path().into()).unwrap();
    import(&mut session, "socks5://192.0.2.12:1080#Fixture");
    assert!(
        session
            .run(Job::ConnectTun)
            .err()
            .unwrap()
            .contains("--prepare-tun-config")
    );
    assert!(!session.snapshot().connected);
    assert!(!session.snapshot().tun_active);
}

fn compile_srs(directory: &Path, name: &str) -> PathBuf {
    let source = directory.join(format!("{name}.json"));
    let output = directory.join(format!("{name}.srs"));
    fs::write(
        &source,
        serde_json::json!({
            "version": 3,
            "rules": [{"domain": [format!("{name}.test")]}]
        })
        .to_string(),
    )
    .unwrap();
    assert!(
        Command::new(exodys::engine::engine_path().unwrap())
            .args(["rule-set", "compile"])
            .arg(&source)
            .arg("-o")
            .arg(&output)
            .status()
            .unwrap()
            .success()
    );
    output
}

fn candidate(workspace: &Path) -> (String, PathBuf, serde_json::Value) {
    let result = tun_command(workspace, "--prepare-tun-config", "192.0.2.53");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8(result.stdout).unwrap();
    let mut lines = stdout.lines();
    let id = lines
        .next()
        .unwrap()
        .strip_prefix("配置编号 ")
        .unwrap()
        .to_owned();
    let path = PathBuf::from(
        lines
            .next()
            .unwrap()
            .strip_prefix("私有配置文件夹 ")
            .unwrap(),
    );
    assert_eq!(path.parent(), Some(workspace));
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
    let file = path.join("config.json");
    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    let bytes = fs::read(file).unwrap();
    let check = exodys::tun_config::config_id(&bytes, |name, _| fs::read(path.join(name))).unwrap();
    assert_eq!(id, check);
    (id, path, serde_json::from_slice(&bytes).unwrap())
}

#[test]
#[ignore = "requires local sing-box; prepares approved-config fixtures without starting TUN"]
fn preparing_tun_follows_node_group_routing_and_rule_set_changes() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let original = compile_srs(fixture.path(), "original");
    let updated = compile_srs(fixture.path(), "updated");
    let mut session = Session::open(workspace.clone()).unwrap();
    let a = import(&mut session, "socks5://192.0.2.12:1080#Fixture-A");
    let b = import(&mut session, "socks5://192.0.2.13:1080#Fixture-B");
    session.run(Job::Select(a.clone())).unwrap();
    drop(session);
    let (first, _, config) = candidate(&workspace);
    assert_eq!(config["outbounds"][0]["server"], "192.0.2.12");
    assert_eq!(candidate(&workspace).0, first);

    let mut session = Session::open(workspace.clone()).unwrap();
    session.run(Job::Select(b.clone())).unwrap();
    drop(session);
    let (second, _, config) = candidate(&workspace);
    assert_ne!(first, second);
    assert_eq!(config["outbounds"][0]["server"], "192.0.2.13");

    let mut session = Session::open(workspace.clone()).unwrap();
    session.run(Job::SetAutoMember(a.clone(), true)).unwrap();
    session.run(Job::SetAutoMember(b.clone(), true)).unwrap();
    session.run(Job::SetAutoEnabled(true)).unwrap();
    session.run(Job::CreatePolicy("office".into())).unwrap();
    session
        .run(Job::SetPolicyMember("office".into(), b.clone(), true))
        .unwrap();
    session
        .run(Job::SetPolicyMode(
            "office".into(),
            exodys::catalog::PolicyMode::Manual(Some(b)),
        ))
        .unwrap();
    session
        .run(Job::ImportRuleSet(
            "fixture".into(),
            exodys::rule_sets::Source::File(original),
        ))
        .unwrap();
    session
        .run(Job::SetRouting(
            "RULE-SET,fixture,GROUP:office\nMATCH,PROXY".into(),
        ))
        .unwrap();
    drop(session);
    let (third, old_candidate, config) = candidate(&workspace);
    assert_ne!(second, third);
    let outbounds = config["outbounds"].as_array().unwrap();
    assert!(
        outbounds
            .iter()
            .any(|out| out["type"] == "urltest" && out["tag"] == "selected")
    );
    let route = &config["route"];
    assert_eq!(route["rule_set"][0]["path"], "fixture.srs");
    let group = &route["rules"][2]["outbound"];
    assert!(
        outbounds
            .iter()
            .any(|out| &out["tag"] == group && out["server"] == "192.0.2.13")
    );
    assert_eq!(
        old_candidate
            .join("fixture.srs")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let mut session = Session::open(workspace.clone()).unwrap();
    session
        .run(Job::ReplaceRuleSet(
            "fixture".into(),
            exodys::rule_sets::Source::File(updated),
        ))
        .unwrap();
    drop(session);
    let (fourth, new_candidate, new_config) = candidate(&workspace);
    assert_ne!(third, fourth);
    assert_eq!(config, new_config);
    assert_ne!(
        fs::read(old_candidate.join("fixture.srs")).unwrap(),
        fs::read(new_candidate.join("fixture.srs")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(workspace.join("tun-dns.json")).unwrap(),
        "\"192.0.2.53\""
    );
}
