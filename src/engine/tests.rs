use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};

struct Fixture {
    port: u16,
    alive: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    fn start(body: &'static str) -> Self {
        let listener = TcpListener::bind(address(0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        let running = alive.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let thread = thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        if body == "second" {
                            thread::sleep(Duration::from_millis(200));
                        }
                        if serve_socks(&mut stream, body).is_ok() {
                            count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(POLL)
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }
        });
        Self {
            port,
            alive,
            requests,
            thread: Some(thread),
        }
    }
    fn stop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
    fn node(&self) -> Node {
        Node::new(
            "same-id".into(),
            "same-name".into(),
            json!({"type":"socks", "server":"127.0.0.1", "server_port":self.port})
                .as_object()
                .unwrap()
                .clone(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
        }
    }
}

fn serve_socks(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let mut header = [0; 2];
    stream.read_exact(&mut header)?;
    let mut methods = vec![0; header[1] as usize];
    stream.read_exact(&mut methods)?;
    stream.write_all(&[5, 0])?;
    let mut request = [0; 4];
    stream.read_exact(&mut request)?;
    let length = match request[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0];
            stream.read_exact(&mut len)?;
            len[0] as usize
        }
        _ => return Err(std::io::ErrorKind::InvalidData.into()),
    };
    let mut destination = vec![0; length + 2];
    stream.read_exact(&mut destination)?;
    stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])?;
    let mut headers = Vec::new();
    let mut byte = [0];
    while !headers.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        headers.push(byte[0]);
        if headers.len() >= 8192 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
    }
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    Ok(())
}

fn fetch(port: u16) -> Option<String> {
    fetch_url(port, "http://127.0.0.1:1/traffic")
}

fn fetch_url(port: u16, url: &str) -> Option<String> {
    let proxy = ureq::Proxy::new(&format!("http://{}", address(port))).ok()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(500)))
        .proxy(Some(proxy))
        .build()
        .into();
    agent.get(url).call().ok()?.body_mut().read_to_string().ok()
}

fn wait_for_body(port: u16, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if fetch(port).as_deref() == Some(expected) {
            return;
        }
        assert!(Instant::now() < deadline, "never selected {expected}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn compile_rule_set(binary: &Path, dir: &Path, name: &str, rule: Value) -> PathBuf {
    let input = dir.join(format!("{name}.json"));
    let output = dir.join(format!("{name}.srs"));
    std::fs::write(&input, json!({"version": 3, "rules": [rule]}).to_string()).unwrap();
    let result = Command::new(binary)
        .args(["rule-set", "compile"])
        .arg(input)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

#[test]
#[ignore = "requires locally installed sing-box; uses only synthetic loopback endpoints"]
fn installed_rule_sets_control_live_requests_and_keep_references_valid() {
    use crate::rule_sets::Source;
    use crate::session::{Job, Session};

    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let binary = engine_path().unwrap();
    let ip = compile_rule_set(
        &binary,
        fixture.path(),
        "ips",
        json!({"ip_cidr": ["127.0.0.1/32"]}),
    );
    let site = compile_rule_set(
        &binary,
        fixture.path(),
        "sites",
        json!({"domain": ["fixture.test"]}),
    );
    let replacement = compile_rule_set(
        &binary,
        fixture.path(),
        "new-sites",
        json!({"domain": ["new.test"]}),
    );
    let bad = fixture.path().join("bad.srs");
    let mut bytes = std::fs::read(&site).unwrap();
    bytes[3] = u8::MAX;
    std::fs::write(&bad, bytes).unwrap();

    let upstream = Fixture::start("matched");
    let port = TcpListener::bind(address(0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let rules = "GEOIP,fixture,PROXY\nGEOSITE,fixture,PROXY\nRULE-SET,blocked,REJECT\nMATCH,REJECT";
    let mut session = Session::open_with_engine(workspace.clone(), Some(binary.clone())).unwrap();
    session
        .run(Job::Import(crate::import::Source::Text(format!(
            "socks5://{}#fixture",
            address(upstream.port)
        ))))
        .unwrap();
    session.run(Job::SetPort(port)).unwrap();
    assert!(
        session
            .run(Job::SetRouting(rules.into()))
            .err()
            .unwrap()
            .contains("blocked")
    );
    for (name, path) in [
        ("geoip-fixture", ip),
        ("geosite-fixture", site.clone()),
        ("blocked", site.clone()),
    ] {
        session
            .run(Job::ImportRuleSet(name.into(), Source::File(path)))
            .unwrap();
    }
    assert!(
        session
            .run(Job::ImportRuleSet(
                "geosite-fixture".into(),
                Source::File(site)
            ))
            .is_err()
    );
    session.run(Job::SetRouting(rules.into())).unwrap();
    assert!(
        session
            .run(Job::RemoveRuleSet("geosite-fixture".into()))
            .is_err()
    );
    assert!(
        session
            .run(Job::ReplaceRuleSet(
                "geosite-fixture".into(),
                Source::File(bad)
            ))
            .is_err()
    );
    let settings = fixture.path().join("settings.json");
    session.run(Job::ExportSettings(settings.clone())).unwrap();
    assert!(
        session
            .run(Job::ExportSettings(
                workspace.join("rule-sets/geosite-fixture.srs")
            ))
            .is_err()
    );
    let mut other = Session::open(fixture.path().join("other")).unwrap();
    assert!(other.run(Job::RestoreSettings(settings)).is_err());
    drop(session);

    let mut session = Session::open_with_engine(workspace.clone(), Some(binary)).unwrap();
    let unavailable = TcpListener::bind(address(0)).unwrap();
    unavailable.set_nonblocking(true).unwrap();
    let update_url = format!("https://{}/sites.srs", unavailable.local_addr().unwrap());
    assert!(
        session
            .run(Job::TrackRuleSet(
                "geosite-fixture".into(),
                Some(update_url.replace("https:", "http:"))
            ))
            .is_err()
    );
    session
        .run(Job::TrackRuleSet(
            "geosite-fixture".into(),
            Some(update_url),
        ))
        .unwrap();
    let installed = workspace.join("rule-sets/geosite-fixture.srs");
    let previous = std::fs::read(&installed).unwrap();
    thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                match unavailable.accept() {
                    Ok(_) => break,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(POLL)
                    }
                    Err(e) => panic!("remote rule-set refresh did not connect: {e}"),
                }
            }
        });
        assert!(
            session
                .run(Job::RefreshDue)
                .err()
                .unwrap()
                .contains("geosite-fixture")
        );
    });
    assert_eq!(std::fs::read(&installed).unwrap(), previous);
    assert!(session.run(Job::RefreshDue).unwrap().message.is_none());
    session
        .run(Job::TrackRuleSet("geosite-fixture".into(), None))
        .unwrap();
    assert_eq!(
        &*session.snapshot().rule_sets,
        &["blocked", "geoip-fixture", "geosite-fixture"]
    );
    session.run(Job::Connect).unwrap();
    assert!(session.run(Job::RemoveRuleSet("blocked".into())).is_err());
    assert_eq!(fetch(port).as_deref(), Some("matched"));
    assert_eq!(
        fetch_url(port, "http://fixture.test/").as_deref(),
        Some("matched")
    );
    assert_eq!(fetch_url(port, "http://new.test/"), None);
    session.run(Job::Disconnect).unwrap();
    session
        .run(Job::ReplaceRuleSet(
            "geosite-fixture".into(),
            Source::File(replacement),
        ))
        .unwrap();
    session.run(Job::Connect).unwrap();
    assert_eq!(fetch_url(port, "http://fixture.test/"), None);
    assert_eq!(
        fetch_url(port, "http://new.test/").as_deref(),
        Some("matched")
    );
    let backup = fixture.path().join("shared.exodysbackup");
    let passphrase = || zeroize::Zeroizing::new("test-only archive password".to_owned());
    session
        .run(Job::ExportBackup(backup.clone(), passphrase()))
        .unwrap();
    assert!(
        session
            .run(Job::RestoreBackup(backup.clone(), passphrase()))
            .is_err()
    );
    session.run(Job::Disconnect).unwrap();
    other.run(Job::RestoreBackup(backup, passphrase())).unwrap();
    assert_eq!(other.snapshot().rule_sets, session.snapshot().rule_sets);
    assert_eq!(
        std::fs::read(other.data_directory().join("rule-sets/geosite-fixture.srs")).unwrap(),
        std::fs::read(&installed).unwrap(),
    );
    other.run(Job::Connect).unwrap();
    assert_eq!(fetch_url(port, "http://fixture.test/"), None);
    assert_eq!(
        fetch_url(port, "http://new.test/").as_deref(),
        Some("matched")
    );
    other.run(Job::Disconnect).unwrap();
    std::fs::remove_file(workspace.join("rule-sets/geosite-fixture.srs")).unwrap();
    assert!(
        session
            .run(Job::Connect)
            .err()
            .unwrap()
            .contains("geosite-fixture")
    );
    session.run(Job::SetRouting(String::new())).unwrap();
    session.run(Job::RemoveRuleSet("blocked".into())).unwrap();
}

#[test]
#[ignore = "requires locally installed sing-box; uses only synthetic loopback endpoints"]
fn urltest_switches_between_explicit_members_after_failure() {
    let dir = tempfile::tempdir().unwrap();
    let binary = engine_path().unwrap();
    let mut first = Fixture::start("first");
    let second = Fixture::start("second");
    let one = first.node();
    let two = second.node();
    let reservation = TcpListener::bind(address(0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let prepared = prepare_auto_with(
        &binary,
        dir.path(),
        &[&one, &two],
        port,
        "MATCH,PROXY",
        "http://127.0.0.1:1/probe",
        "1s",
    )
    .unwrap();
    let _proxy = prepared.start().unwrap();
    wait_for_body(port, "first");
    // Let both initial tests complete before failing the preferred upstream.
    let deadline = Instant::now() + Duration::from_secs(5);
    while second.requests.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "initial probe did not finish");
        thread::sleep(POLL);
    }
    first.stop();
    wait_for_body(port, "second");
}

#[test]
#[ignore = "requires locally installed sing-box; uses only synthetic loopback endpoints"]
fn independent_policy_routes_reach_their_own_upstreams() {
    use crate::catalog::{NodeRef, PolicyMode};
    use crate::session::{Job, Session};

    let dir = tempfile::tempdir().unwrap();
    let a = Fixture::start("office");
    let b = Fixture::start("home");
    let mut session = Session::open(dir.path().into()).unwrap();
    for (name, fixture) in [("office", &a), ("home", &b)] {
        let profile = session
            .run(Job::Import(crate::import::Source::Text(format!(
                "socks5://{}#{name}",
                address(fixture.port)
            ))))
            .unwrap()
            .focus
            .unwrap();
        let node = NodeRef {
            profile,
            node: session.snapshot().catalog.profile(profile).unwrap().nodes[0]
                .id
                .clone(),
        };
        session.run(Job::CreatePolicy(name.into())).unwrap();
        session
            .run(Job::SetPolicyMember(name.into(), node.clone(), true))
            .unwrap();
        session
            .run(Job::SetPolicyMode(
                name.into(),
                PolicyMode::Manual(Some(node)),
            ))
            .unwrap();
    }
    session.run(Job::CreatePolicy("unused".into())).unwrap();
    let port = TcpListener::bind(address(0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    session.run(Job::SetPort(port)).unwrap();
    session
        .run(Job::SetRouting(
            "DOMAIN,office.test,GROUP:office\nMATCH,GROUP:home".into(),
        ))
        .unwrap();
    session.run(Job::Connect).unwrap();
    assert_eq!(
        fetch_url(port, "http://office.test/").as_deref(),
        Some("office")
    );
    assert_eq!(
        fetch_url(port, "http://home.test/").as_deref(),
        Some("home")
    );
    let home_profile = session.snapshot().preferences.policy_groups[1].members[0].profile;
    assert!(session.snapshot().profile_in_use(home_profile));
    assert!(session.run(Job::Remove(home_profile)).is_err());
    session.run(Job::Disconnect).unwrap();
    assert!(!session.snapshot().profile_in_use(home_profile));
}

#[cfg(unix)]
#[test]
fn auto_uses_unique_tags_and_routes_proxy_through_group() {
    let node = Node::new(
        "id".into(),
        "name".into(),
        json!({"type":"socks", "tag":"selected", "server":"127.0.0.1", "server_port":1})
            .as_object()
            .unwrap()
            .clone(),
    );
    let dir = tempfile::tempdir().unwrap();
    for nodes in [vec![&node], vec![&node, &node]] {
        let prepared = prepare_auto(
            Path::new("/usr/bin/true"),
            dir.path(),
            &nodes,
            1234,
            "MATCH,PROXY",
        )
        .unwrap();
        let value: Value =
            serde_json::from_reader(std::fs::File::open(prepared.file.path()).unwrap()).unwrap();
        let outbounds = value["outbounds"].as_array().unwrap();
        let group = &outbounds[nodes.len()];
        assert_eq!(group["tag"], "selected");
        assert_eq!(group["type"], "urltest");
        assert_eq!(group["url"], "https://www.cloudflare.com/cdn-cgi/trace");
        assert_eq!(group["interval"], "1m");
        for (index, outbound) in outbounds.iter().take(nodes.len()).enumerate() {
            let tag = format!("selected:member:{index}");
            assert_eq!(outbound["tag"], tag);
            assert_eq!(group["outbounds"][index], tag);
        }
        assert_eq!(value["route"]["final"], "selected");
        assert_eq!(value["route"]["rules"][0]["outbound"], "selected");
    }
}

#[test]
fn auto_rejects_empty_and_excess_members_before_invoking_engine() {
    let node = Node::new(
        "id".into(),
        "name".into(),
        json!({"type":"socks", "server":"127.0.0.1", "server_port":1})
            .as_object()
            .unwrap()
            .clone(),
    );
    let dir = tempfile::tempdir().unwrap();
    let nonexistent = Path::new("/nonexistent/sing-box");
    assert!(
        prepare_auto(nonexistent, dir.path(), &[], 1234, "")
            .err()
            .unwrap()
            .contains("1 至 32")
    );
    let nodes = vec![&node; crate::catalog::MAX_AUTO_MEMBERS + 1];
    assert!(
        prepare_auto(nonexistent, dir.path(), &nodes, 1234, "")
            .err()
            .unwrap()
            .contains("1 至 32")
    );
}
