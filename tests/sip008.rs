use std::io::{Read, Write};
use std::net::TcpListener;

use exodys::{
    import::{MAX_NODES, Source, parse},
    session::{Job, Session},
};
use serde_json::{Value, json};

fn server(name: &str) -> Value {
    json!({"id":"27b8a625-4f4b-4428-9f0f-8a2317db7c79", "remarks":name,
        "server":"proxy.example.test", "server_port":8388,
        "password":"fixture", "method":"chacha20-ietf-poly1305"})
}

fn document(servers: Vec<Value>) -> String {
    json!({"version":1, "servers":servers, "bytes_used":123}).to_string()
}

#[test]
fn parses_envelope_and_legacy_array_without_importing_metadata() {
    for input in [
        document(vec![server("Example")]),
        json!([server("Example")]).to_string(),
    ] {
        let imported = parse(input.as_bytes(), "Fixture".into(), None).unwrap();
        assert_eq!(imported.skipped, 0);
        assert_eq!(imported.nodes[0].name, "Example");
        assert_eq!(imported.nodes[0].endpoint(), "proxy.example.test:8388");
        let node = serde_json::to_value(&imported.nodes[0]).unwrap();
        assert_eq!(
            node["outbound"],
            json!({"type":"shadowsocks", "server":"proxy.example.test", "server_port":8388, "password":"fixture", "method":"chacha20-ietf-poly1305"})
        );
    }
    let singbox = json!([{"type":"socks", "server":"localhost", "server_port":1080}]).to_string();
    assert_eq!(
        parse(singbox.as_bytes(), "Fixture".into(), None)
            .unwrap()
            .nodes[0]
            .kind(),
        "socks"
    );
}

#[test]
fn skips_bad_entries_and_rejects_unsupported_plugins() {
    let mut plugin = server("Plugin");
    plugin["plugin"] = json!("v2ray-plugin");
    plugin["plugin_opts"] = json!("mode=websocket;unknown=1");
    let mut dangling = server("Dangling");
    dangling["plugin_opts"] = json!("obfs=http");
    let mut empty = server("Empty plugin");
    empty["plugin"] = json!("");
    empty["plugin_opts"] = json!("");
    let mut invalid = server("Invalid port");
    invalid["server_port"] = json!(0);
    let input = document(vec![
        server("Valid"),
        plugin,
        dangling,
        empty,
        invalid,
        json!({}),
    ]);
    let imported = parse(input.as_bytes(), "Fixture".into(), None).unwrap();
    assert_eq!(imported.nodes.len(), 2);
    assert_eq!(imported.skipped, 4);
    for input in [
        json!({"version":2,"servers":[server("X")]}),
        json!({"version":1,"servers":{}}),
        json!({"servers":[server("X")]}),
        json!({"version":1,"servers":[]}),
    ] {
        assert!(parse(input.to_string().as_bytes(), "Fixture".into(), None).is_err());
    }
    let too_many = document(vec![server("X"); MAX_NODES + 1]);
    assert!(parse(too_many.as_bytes(), "Fixture".into(), None).is_err());
}

#[test]
fn imports_file_and_refreshes_subscription() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.json");
    std::fs::write(&path, document(vec![server("File")])).unwrap();
    assert_eq!(Source::File(path).load().unwrap().nodes[0].name, "File");

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/subscription", listener.local_addr().unwrap());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for name in ["Initial", "Updated"] {
                listener.set_nonblocking(true).unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(10))
                        }
                        Err(e) => panic!("subscription fixture accept failed: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut header = Vec::new();
                let mut byte = [0];
                while !header.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                    assert!(header.len() < 8192);
                }
                assert!(header.starts_with(b"GET /subscription HTTP/1.1\r\n"));
                let body = document(vec![server(name)]);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let mut session = Session::open(directory.path().join("session")).unwrap();
        let id = session
            .run(Job::Import(Source::Text(url)))
            .unwrap()
            .focus
            .unwrap();
        assert_eq!(
            session.snapshot().catalog.profile(id).unwrap().nodes[0].name,
            "Initial"
        );
        session.run(Job::Refresh(id)).unwrap();
        assert_eq!(
            session.snapshot().catalog.profile(id).unwrap().nodes[0].name,
            "Updated"
        );
    });
}
