#![cfg(target_os = "macos")]
use std::{
    fs::{DirBuilder, OpenOptions},
    os::unix::fs::DirBuilderExt,
    process::{Command, Stdio},
};

/// Launch the actual app's helper entry. A pre-owned lock in a private fake
/// home must stop it *before* it calls SystemConfiguration or opens a window.
#[test]
fn competing_helper_refuses_before_touching_network_settings() {
    let home = tempfile::tempdir().unwrap();
    let dir = home
        .path()
        .join("Library/Application Support/io.github.bernadeliot-sudo.exodys.system-proxy");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    let lock = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dir.join("guard.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_exodys"))
        .args(["--exodys-system-proxy-guard", "2080"])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("另一 Exodys 会话")
    );
    assert!(!dir.join("previous.plist").exists());
}
