use std::fs;
use std::path::{Path, PathBuf};

use exodys::{
    import::Source,
    session::{Job, Session},
};
use zeroize::Zeroizing;

fn passphrase() -> Zeroizing<String> {
    Zeroizing::new("synthetic sync passphrase".into())
}

fn revisions(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("exodys-sync-")
        })
        .collect()
}

fn mirror(source: &Path, target: &Path) {
    for revision in revisions(source) {
        let destination = target.join(revision.file_name().unwrap());
        if !destination.exists() {
            fs::copy(revision, destination).unwrap();
        }
    }
}

#[test]
fn independent_devices_merge_offline_edits_and_resolve_overlapping_changes() {
    let temp = tempfile::tempdir().unwrap();
    let a_folder = temp.path().join("cloud-a");
    let b_folder = temp.path().join("cloud-b");
    fs::create_dir(&a_folder).unwrap();
    fs::create_dir(&b_folder).unwrap();
    let a_data = temp.path().join("a");
    let b_data = temp.path().join("b");
    let mut a = Session::open(a_data).unwrap();
    let mut b = Session::open(b_data.clone()).unwrap();
    a.run(Job::Import(Source::Text(
        "socks5://localhost:9#Original".into(),
    )))
    .unwrap();
    let system_proxy_supported = cfg!(target_os = "macos");
    if system_proxy_supported {
        b.run(Job::SetSystemProxy(true)).unwrap();
    }
    a.run(Job::StartSync(a_folder.clone(), passphrase()))
        .unwrap();
    mirror(&a_folder, &b_folder);
    assert!(
        b.run(Job::StartSync(
            b_folder.clone(),
            Zeroizing::new("incorrect test passphrase".into())
        ))
        .is_err()
    );
    assert!(b.snapshot().catalog.profiles.is_empty());
    b.run(Job::StartSync(b_folder.clone(), passphrase()))
        .unwrap();
    assert_eq!(b.snapshot().node().unwrap().name, "Original");
    assert_eq!(
        b.snapshot().preferences.system_proxy,
        system_proxy_supported
    );

    a.run(Job::SetPort(22401)).unwrap();
    b.run(Job::SetRouting("DOMAIN,fixture.test,DIRECT".into()))
        .unwrap();
    for (device, label) in [(&mut a, "Device-A"), (&mut b, "Device-B")] {
        device
            .run(Job::Import(Source::Text(format!(
                "socks5://localhost:9#{label}"
            ))))
            .unwrap();
        device.run(Job::SyncTick).unwrap();
    }
    mirror(&a_folder, &b_folder);
    mirror(&b_folder, &a_folder);
    a.run(Job::SyncTick).unwrap();
    b.run(Job::SyncTick).unwrap();
    mirror(&a_folder, &b_folder);
    mirror(&b_folder, &a_folder);
    assert_eq!(
        revisions(&a_folder).len(),
        4,
        "peers publish the same merge ID"
    );
    for device in [&mut a, &mut b] {
        assert!(device.run(Job::SyncTick).unwrap().message.is_none());
        let snapshot = device.snapshot();
        assert!(snapshot.sync.conflict.is_none());
        assert_eq!(snapshot.preferences.port, 22401);
        assert_eq!(
            snapshot.preferences.routing_rules,
            "DOMAIN,fixture.test,DIRECT"
        );
        assert_eq!(snapshot.catalog.profiles.len(), 3);
        assert_eq!(snapshot.node().unwrap().name, "Original");
    }
    assert!(!a.snapshot().preferences.system_proxy);
    assert_eq!(
        b.snapshot().preferences.system_proxy,
        system_proxy_supported
    );

    a.run(Job::SetPort(22402)).unwrap();
    b.run(Job::SetPort(22403)).unwrap();
    a.run(Job::SyncTick).unwrap();
    b.run(Job::SyncTick).unwrap();
    mirror(&a_folder, &b_folder);
    mirror(&b_folder, &a_folder);
    for (device, port) in [(&mut a, 22402), (&mut b, 22403)] {
        device.run(Job::SyncTick).unwrap();
        assert!(
            device
                .snapshot()
                .sync
                .conflict
                .unwrap()
                .detail
                .contains("端口")
        );
        assert_eq!(device.snapshot().preferences.port, port);
    }
    a.run(Job::ResolveSync {
        conflict: a.snapshot().sync.conflict.unwrap(),
        keep_local: false,
    })
    .unwrap();
    let stale = b.snapshot().sync.conflict.unwrap();
    mirror(&a_folder, &b_folder);
    assert!(
        b.run(Job::ResolveSync {
            conflict: stale.clone(),
            keep_local: true,
        })
        .is_err()
    );
    b.run(Job::SyncTick).unwrap();
    assert!(
        b.run(Job::ResolveSync {
            conflict: stale,
            keep_local: true,
        })
        .is_err()
    );
    assert_eq!(a.snapshot().preferences.port, 22403);
    assert_eq!(b.snapshot().preferences.port, 22403);
    assert!(a.snapshot().sync.conflict.is_none());
    assert!(b.snapshot().sync.conflict.is_none());
    assert_eq!(
        revisions(&a_folder).len(),
        7,
        "both conflicting versions are retained"
    );

    drop(b);
    let mut reopened = Session::open(b_data).unwrap();
    assert!(!reopened.snapshot().sync.unlocked);
    assert_eq!(
        reopened.snapshot().sync.directory.as_deref(),
        Some(b_folder.canonicalize().unwrap().as_path())
    );
    assert!(reopened.run(Job::SyncTick).is_err());
    assert!(
        reopened
            .run(Job::StartSync(b_folder.clone(), passphrase()))
            .unwrap()
            .message
            .is_none()
    );
    assert_eq!(revisions(&b_folder).len(), 7);
    assert_eq!(
        reopened.snapshot().preferences.system_proxy,
        system_proxy_supported
    );
    reopened.run(Job::PauseSync).unwrap();
    assert!(!reopened.snapshot().sync.unlocked);
}

#[test]
fn out_of_order_file_delivery_waits_for_complete_history() {
    let temp = tempfile::tempdir().unwrap();
    let shared = temp.path().join("shared");
    let incoming = temp.path().join("incoming");
    fs::create_dir(&shared).unwrap();
    fs::create_dir(&incoming).unwrap();
    let mut sender = Session::open(temp.path().join("sender")).unwrap();
    sender
        .run(Job::StartSync(shared.clone(), passphrase()))
        .unwrap();
    let parent = revisions(&shared).pop().unwrap();
    sender.run(Job::SetPort(22404)).unwrap();
    sender.run(Job::SyncTick).unwrap();
    let child = revisions(&shared)
        .into_iter()
        .find(|path| path != &parent)
        .unwrap();
    fs::copy(&child, incoming.join(child.file_name().unwrap())).unwrap();
    let mut receiver = Session::open(temp.path().join("receiver")).unwrap();
    assert!(
        receiver
            .run(Job::StartSync(incoming.clone(), passphrase()))
            .is_err()
    );
    assert_eq!(revisions(&incoming).len(), 1);
    assert_eq!(
        receiver.snapshot().preferences.port,
        exodys::catalog::DEFAULT_PORT
    );
    mirror(&shared, &incoming);
    receiver
        .run(Job::StartSync(incoming, passphrase()))
        .unwrap();
    assert_eq!(receiver.snapshot().preferences.port, 22404);
}
