use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use exodys::{
    catalog::NodeRef,
    engine::{address, engine_path},
    import::Source,
    session::{Job, Session},
};
use serde_json::json;

struct Upstream(Child);

impl Drop for Upstream {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unused_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
#[ignore = "requires a locally installed sing-box; run with --ignored"]
fn routing_applies_the_first_match_and_uses_selected_for_unmatched_requests() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::open(directory.path().into()).unwrap();
    // An unavailable upstream makes direct and proxied traffic distinguishable.
    let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let dead = upstream.local_addr().unwrap();
    session
        .run(Job::Import(Source::Text(format!(
            "socks5://{dead}#fixture"
        ))))
        .unwrap();
    session.run(Job::SetPort(unused_port())).unwrap();
    drop(upstream);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/proof", listener.local_addr().unwrap());
    session
        .run(Job::SetRouting(
            "IP-CIDR,127.0.0.1/32,DIRECT\nMATCH,REJECT".into(),
        ))
        .unwrap();
    session.run(Job::Connect).unwrap();
    assert!(session.run(Job::SetRouting(String::new())).is_err());
    listener.set_nonblocking(true).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut buffer = [0; 1024];
                    assert!(stream.read(&mut buffer).unwrap() > 0);
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .unwrap();
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "direct route never reached loopback"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        session.run(Job::Probe(url.clone())).unwrap();
    });
    session.run(Job::Disconnect).unwrap();
    for rules in [
        "IP-CIDR,127.0.0.1/32,REJECT\nMATCH,DIRECT",
        "DOMAIN,unmatched.example,DIRECT",
    ] {
        session.run(Job::SetRouting(rules.into())).unwrap();
        session.run(Job::Connect).unwrap();
        assert!(session.run(Job::Probe(url.clone())).is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        session.run(Job::Disconnect).unwrap();
    }
}

fn wait_for_socks(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(mut socket) =
            TcpStream::connect_timeout(&address(port), Duration::from_millis(100))
        {
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            socket.write_all(&[5, 1, 0]).unwrap();
            let mut reply = [0, 0];
            if socket.read_exact(&mut reply).is_ok() && reply == [5, 0] {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "upstream SOCKS port did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "requires a locally installed sing-box; run with --ignored"]
fn requests_traverse_sing_box_and_disconnect_releases_port() {
    let directory = tempfile::tempdir().unwrap();
    let upstream_port = unused_port();
    let upstream_config = directory.path().join("upstream.json");
    let config = json!({
        "inbounds": [{"type":"mixed", "listen":"127.0.0.1", "listen_port":upstream_port}],
        "outbounds": [{"type":"direct", "tag":"direct"}]
    });
    std::fs::write(&upstream_config, config.to_string()).unwrap();
    let engine = engine_path().unwrap();
    let _upstream = Upstream(
        Command::new(&engine)
            .args(["run", "-c"])
            .arg(&upstream_config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_socks(upstream_port);

    let mut session =
        Session::open_with_engine(directory.path().join("client"), Some(engine)).unwrap();
    let nodes = json!({"outbounds": [
        {"type":"socks", "tag":"Desktop relay", "server":"127.0.0.1", "server_port":upstream_port},
        {"type":"vless", "tag":"Invalid UUID", "server":"127.0.0.1", "server_port":upstream_port, "uuid":"broken"},
        {"type":"socks", "tag":"Second relay", "server":"127.0.0.1", "server_port":upstream_port}
    ]});
    let id = session
        .run(Job::Import(Source::Text(nodes.to_string())))
        .unwrap()
        .focus
        .unwrap();
    let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = occupied.local_addr().unwrap().port();
    session.run(Job::SetPort(port)).unwrap();
    assert!(session.run(Job::Connect).is_err());
    assert!(!session.snapshot().connected);
    drop(occupied);
    session.run(Job::Connect).unwrap();
    assert!(session.snapshot().connected);
    let catalog = session.snapshot().catalog;
    let profile = catalog.profile(id).unwrap();
    assert_eq!(
        profile.nodes.len(),
        2,
        "invalid UUID is skipped during import"
    );
    assert_eq!(profile.nodes[0].name, "Desktop relay");
    assert_eq!(profile.nodes[1].name, "Second relay");
    assert_eq!(session.snapshot().node().unwrap().name, "Desktop relay");
    assert!(session.snapshot().connected);
    let next = NodeRef {
        profile: id,
        node: profile.nodes[1].id.clone(),
    };
    session.run(Job::Select(next)).unwrap();
    assert_eq!(session.snapshot().node().unwrap().name, "Second relay");

    let endpoint = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    endpoint.set_nonblocking(true).unwrap();
    let url = format!("http://{}/proof", endpoint.local_addr().unwrap());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                if let Ok((mut stream, _)) = endpoint.accept() {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut req = [0; 2048];
                    let length = stream.read(&mut req).unwrap();
                    assert!(req[..length].starts_with(b"GET /proof HTTP/1.1"));
                    stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    return;
                }
                assert!(Instant::now() < deadline, "proxied request never reached destination");
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        session.run(Job::Probe(url)).unwrap();
        assert!(session.snapshot().probe_time.is_some());
    });
    session.run(Job::Disconnect).unwrap();
    assert!(!session.snapshot().connected);
    assert!(TcpStream::connect_timeout(&address(port), Duration::from_millis(200)).is_err());
}
