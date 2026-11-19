use std::hint::black_box;
use std::time::{Duration, Instant};

use exodys::{
    import::Source,
    session::{Job, Session},
};
use serde_json::{Value, json};

fn percentiles(values: &mut [Duration]) -> (f64, f64) {
    values.sort_unstable();
    let milliseconds = |sample: Duration| sample.as_secs_f64() * 1000.0;
    (
        milliseconds(values[values.len() / 2]),
        milliseconds(values[(values.len() * 9).div_ceil(10) - 1]),
    )
}

#[test]
#[ignore = "repeatable performance sample; run in release mode with --ignored --nocapture"]
fn import_and_filter_10000_nodes() {
    const COUNT: usize = 10_000;
    let payload: Vec<Value> = (0..COUNT)
        .map(|i| {
            json!({
                "type": "socks", "tag": format!("relay-{i:05}"),
                "server": format!("node-{i}.example.test"), "server_port": 1080
            })
        })
        .collect();
    let payload = json!({"outbounds": payload}).to_string();
    let mut imports = Vec::new();
    let mut searches = Vec::new();
    let mut reads = Vec::new();
    for _ in 0..15 {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::open(dir.path().into()).unwrap();
        let source = Source::Text(payload.clone());
        let start = Instant::now();
        let id = session.run(Job::Import(source)).unwrap().focus.unwrap();
        imports.push(start.elapsed());
        let state = session.snapshot();
        let profile = state.catalog.profile(id).unwrap();
        assert_eq!(profile.nodes.len(), COUNT);
        for query in ["relay-01", "relay-08 socks", "example.test", "no match", ""] {
            let start = Instant::now();
            black_box(profile.search(black_box(query)));
            searches.push(start.elapsed());
        }
        drop(session);
        let start = Instant::now();
        let reopened = Session::open(dir.path().into()).unwrap();
        reads.push(start.elapsed());
        assert_eq!(
            reopened.snapshot().catalog.profile(id).unwrap().nodes.len(),
            COUNT
        );
    }
    for (label, samples) in [
        ("import + disk commit", &mut imports),
        ("profile reload", &mut reads),
        ("node filter", &mut searches),
    ] {
        let (p50, p90) = percentiles(samples);
        println!(
            "{label}: p50 {p50:.3} ms, p90 {p90:.3} ms ({} samples, {COUNT} nodes)",
            samples.len()
        );
    }
}
