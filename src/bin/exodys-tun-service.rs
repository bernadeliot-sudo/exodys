//! Socket-activated, single-lease macOS TUN supervisor.
//!
//! The installer protects the executable and approves immutable, content-addressed
//! configurations. Clients can select an approved version by its digest.

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CString, c_char, c_int, c_void};

    use libc::{fcntl, free, geteuid, getpeereid, kill};
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use exodys::tun_config::{
        CONFIG_FILE, MAX_CONFIG_BYTES, REQUEST_LENGTH, REQUEST_PREFIX, UNAPPROVED, config_id,
        requested_id,
    };

    const DIR: &str = "/Library/PrivilegedHelperTools/io.github.bernadeliot-sudo.exodys.tun";
    const ENGINE: &str =
        "/Library/PrivilegedHelperTools/io.github.bernadeliot-sudo.exodys.tun/sing-box";
    const UID: &str = "/Library/PrivilegedHelperTools/io.github.bernadeliot-sudo.exodys.tun/uid";
    const MARKER: &[u8] = b"sing-box started (";
    const TICK: Duration = Duration::from_millis(100);
    const START_TIMEOUT: Duration = Duration::from_secs(60);
    const STOP_TIMEOUT: Duration = Duration::from_secs(5);
    const ACL_TYPE_EXTENDED: c_int = 0x100;

    static STOP: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const c_char,
            fds: *mut *mut c_int,
            count: *mut usize,
        ) -> c_int;
        fn acl_get_link_np(path: *const c_char, kind: c_int) -> *mut c_void;
        fn acl_free(acl: *mut c_void) -> c_int;
    }

    extern "C" fn stop_signal(_: c_int) {
        STOP.store(true, Ordering::Relaxed);
    }

    fn invalid() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "service validation failed")
    }

    fn check_metadata(meta: &fs::Metadata, directory: bool, private: bool) -> io::Result<()> {
        let mode = meta.mode();
        if meta.uid() != 0
            || (directory && !meta.file_type().is_dir())
            || (!directory && !meta.file_type().is_file())
            || mode & 0o6022 != 0
            || (!directory && (mode & 0o1000 != 0 || (private && meta.nlink() != 1)))
            || (private && mode & 0o077 != 0)
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn reject_acl(path: &Path) -> io::Result<()> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid())?;
        // Reject every extended ACL, including deny-only ACLs, rather than
        // attempting to interpret macOS's ordered allow/deny entries.
        let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
        if !acl.is_null() {
            unsafe {
                acl_free(acl);
            }
            return Err(invalid());
        }
        // macOS reports ENOENT when an existing object has no extended ACL.
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT) => Ok(()),
            _ => Err(invalid()),
        }
    }

    fn validate_paths() -> io::Result<u32> {
        for path in ["/", "/Library", "/Library/PrivilegedHelperTools", DIR] {
            let meta = fs::symlink_metadata(path)?;
            check_metadata(&meta, true, path == DIR)?;
            reject_acl(Path::new(path))?;
        }
        check_metadata(&fs::symlink_metadata(ENGINE)?, false, false)?;
        reject_acl(Path::new(ENGINE))?;
        parse_uid(&read_private_file(Path::new(UID), 32)?)
    }

    fn read_private_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
        check_metadata(&fs::symlink_metadata(path)?, false, true)?;
        reject_acl(path)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        check_metadata(&file.metadata()?, false, true)?;
        read_bounded(file, limit)
    }

    fn read_bounded(file: File, limit: usize) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(invalid());
        }
        Ok(bytes)
    }

    fn verify_candidate(
        directory: &Path,
        id: &str,
        mut read: impl FnMut(&Path, usize) -> io::Result<Vec<u8>>,
    ) -> io::Result<()> {
        let bytes = read(&directory.join(CONFIG_FILE), MAX_CONFIG_BYTES)?;
        let verified = config_id(&bytes, |file, max| read(&directory.join(file), max))?;
        if verified != id {
            return Err(invalid());
        }
        Ok(())
    }

    fn approved_directory(root: &Path, id: &str) -> io::Result<std::path::PathBuf> {
        let approved = root.join("approved");
        let directory = approved.join(id);
        for path in [&approved, &directory] {
            check_metadata(&fs::symlink_metadata(path)?, true, true)?;
            reject_acl(path)?;
        }
        verify_candidate(&directory, id, read_private_file)?;
        Ok(directory)
    }

    fn parse_uid(bytes: &[u8]) -> io::Result<u32> {
        let digits = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        if digits.is_empty() || digits.len() > 10 || !digits.iter().all(u8::is_ascii_digit) {
            return Err(invalid());
        }
        let uid = std::str::from_utf8(digits)
            .map_err(|_| invalid())?
            .parse::<u32>()
            .map_err(|_| invalid())?;
        if uid == 0 {
            return Err(invalid());
        }
        Ok(uid)
    }

    fn listener() -> io::Result<UnixListener> {
        let mut fds = std::ptr::null_mut();
        let mut count = 0usize;
        let result = unsafe { launch_activate_socket(c"Client".as_ptr(), &mut fds, &mut count) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        if fds.is_null() {
            return Err(invalid());
        }
        let descriptors = unsafe { std::slice::from_raw_parts(fds, count) };
        if count != 1 {
            for &fd in descriptors {
                drop(unsafe { File::from_raw_fd(fd) });
            }
            unsafe {
                free(fds.cast());
            }
            return Err(invalid());
        }
        let fd = descriptors[0];
        unsafe {
            free(fds.cast());
        }
        let listener = unsafe { UnixListener::from_raw_fd(fd) };
        if unsafe { fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        listener.set_nonblocking(true)?;
        Ok(listener)
    }

    fn authenticated(stream: &UnixStream, approved: u32) -> bool {
        let mut uid = 0;
        let mut gid = 0;
        unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == approved }
    }

    fn handshake(stream: &mut UnixStream) -> io::Result<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut request = [0u8; REQUEST_LENGTH];
        let mut n = 0;
        while n < request.len() {
            if STOP.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Err(invalid());
            }
            match stream.read(&mut request[n..]) {
                Ok(0) => return Err(invalid()),
                Ok(size) => {
                    n += size;
                    let prefix = n.min(REQUEST_PREFIX.len());
                    if request[..prefix] != REQUEST_PREFIX[..prefix]
                        || (n < request.len() && request[..n].ends_with(b"\n"))
                    {
                        return Err(invalid());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        requested_id(&request)
            .map(str::to_owned)
            .ok_or_else(invalid)
    }

    // Drain both pipes without retaining output or allowing a long log line to
    // consume memory. The marker may be split across arbitrary reads.
    fn watch_output(mut output: impl Read + Send + 'static, sender: mpsc::SyncSender<()>) {
        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            let mut matched = 0;
            while let Ok(n) = output.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                for &byte in &buffer[..n] {
                    if byte == MARKER[matched] {
                        matched += 1;
                    } else {
                        matched = usize::from(byte == MARKER[0]);
                    }
                    if matched == MARKER.len() {
                        let _ = sender.try_send(());
                        matched = 0;
                    }
                }
            }
        });
    }

    struct Engine(Child);
    impl Engine {
        fn stop(&mut self) -> io::Result<()> {
            if let Some(status) = self.0.try_wait()? {
                return status.success().then_some(()).ok_or_else(invalid);
            }
            unsafe {
                kill(self.0.id() as c_int, libc::SIGTERM);
            }
            let deadline = Instant::now() + STOP_TIMEOUT;
            while Instant::now() < deadline {
                if let Some(status) = self.0.try_wait()? {
                    return status.success().then_some(()).ok_or_else(invalid);
                }
                thread::sleep(TICK);
            }
            let _ = self.0.kill();
            self.0.wait()?;
            Err(invalid())
        }
    }
    impl Drop for Engine {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    #[derive(PartialEq)]
    enum Input {
        Idle,
        Eof,
        Unexpected,
    }
    fn client_input(stream: &mut UnixStream) -> io::Result<Input> {
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => Ok(Input::Eof),
            Ok(_) => Ok(Input::Unexpected),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(Input::Idle)
            }
            Err(e) => Err(e),
        }
    }

    fn lease(stream: &mut UnixStream, directory: &Path) -> io::Result<()> {
        if STOP.load(Ordering::Relaxed) || client_input(stream)? != Input::Idle {
            return Err(invalid());
        }
        let child = Command::new(ENGINE)
            .args(["run", "-c", CONFIG_FILE])
            .current_dir(directory)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", DIR)
            .env("TMPDIR", DIR)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        supervise(stream, child)
    }

    fn supervise(stream: &mut UnixStream, child: Child) -> io::Result<()> {
        let mut engine = Engine(child);
        let stdout = engine.0.stdout.take().ok_or_else(invalid)?;
        let stderr = engine.0.stderr.take().ok_or_else(invalid)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        watch_output(stdout, sender.clone());
        watch_output(stderr, sender);
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if STOP.load(Ordering::Relaxed)
                || engine.0.try_wait()?.is_some()
                || client_input(stream)? != Input::Idle
            {
                return Err(invalid());
            }
            if receiver.try_recv().is_ok() {
                // Recheck after observing the marker to avoid advertising a dead child.
                if engine.0.try_wait()?.is_some() {
                    return Err(invalid());
                }
                stream.write_all(b"READY\n")?;
                break;
            }
            if Instant::now() >= deadline {
                return Err(invalid());
            }
        }
        loop {
            if STOP.load(Ordering::Relaxed) || !matches!(engine.0.try_wait(), Ok(None)) {
                return Ok(());
            }
            match client_input(stream) {
                Ok(Input::Idle) => (),
                Ok(Input::Eof) => {
                    if engine.stop().is_ok() {
                        let _ = stream.write_all(b"STOPPED\n");
                    }
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
    }

    pub fn run() -> io::Result<()> {
        let handler = stop_signal as *const () as libc::sighandler_t;
        if unsafe { libc::signal(libc::SIGTERM, handler) } == libc::SIG_ERR
            || unsafe { libc::signal(libc::SIGINT, handler) } == libc::SIG_ERR
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { geteuid() } != 0 {
            return Err(invalid());
        }
        let approved = validate_paths()?;
        let listener = listener()?;
        let mut stream = loop {
            if STOP.load(Ordering::Relaxed) {
                return Ok(());
            }
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => return Err(e),
            }
        };
        drop(listener);
        stream.set_read_timeout(Some(TICK))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        if !authenticated(&stream, approved) {
            return Err(invalid());
        }
        let id = handshake(&mut stream).inspect_err(|_| {
            let _ = stream.write_all(b"ERROR\n");
        })?;
        let directory = approved_directory(Path::new(DIR), &id).inspect_err(|_| {
            let _ = stream.write_all(UNAPPROVED);
        })?;
        lease(&mut stream, &directory).inspect_err(|_| {
            let _ = stream.write_all(b"ERROR\n");
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn uid_is_one_nonzero_decimal_number() {
            for good in [b"501".as_slice(), b"501\n", b"4294967295"] {
                assert!(parse_uid(good).is_ok());
            }
            for bad in [
                b"".as_slice(),
                b"0",
                b"-1",
                b"1\n2",
                b" 501",
                b"4294967296",
                b"1\n\n",
            ] {
                assert!(parse_uid(bad).is_err());
            }
        }
        #[test]
        fn fake_engine_ready_then_disconnect_stops_child() {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            server.set_read_timeout(Some(TICK)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let child = Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap 'exit 0' TERM; printf 'sing-box started (0.1s)\\n' >&2; while :; do /bin/sleep 0.05; done",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let pid = child.id() as c_int;
            let worker = thread::spawn(move || supervise(&mut server, child));
            let mut reply = [0; 6];
            client.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"READY\n");
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let mut stopped = [0; 8];
            client.read_exact(&mut stopped).unwrap();
            assert_eq!(&stopped, b"STOPPED\n");
            assert!(worker.join().unwrap().is_ok());
            assert_eq!(unsafe { kill(pid, 0) }, -1);
        }

        #[test]
        #[ignore = "requires local sing-box; runs an unprivileged loopback inbound only"]
        fn live_engine_acknowledges_start_and_graceful_stop() {
            let fixture = tempfile::tempdir().unwrap();
            let config = fixture.path().join("config.json");
            let port = std::net::TcpListener::bind(("127.0.0.1", 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            fs::write(
                &config,
                serde_json::json!({
                    "log": { "level": "info" },
                    "inbounds": [{ "type": "mixed", "listen": "127.0.0.1", "listen_port": port }],
                    "outbounds": [{ "type": "direct", "tag": "direct" }]
                })
                .to_string(),
            )
            .unwrap();
            let (mut client, mut server) = UnixStream::pair().unwrap();
            server.set_read_timeout(Some(TICK)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            let child = Command::new(exodys::engine::engine_path().unwrap())
                .arg("run")
                .arg("-c")
                .arg(config)
                .current_dir(fixture.path())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let worker = thread::spawn(move || supervise(&mut server, child));
            let mut ready = [0; 6];
            client.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"READY\n");
            assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let mut stopped = [0; 8];
            client.read_exact(&mut stopped).unwrap();
            assert_eq!(&stopped, b"STOPPED\n");
            worker.join().unwrap().unwrap();
            assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
        }

        #[test]
        fn approval_requires_matching_config_and_rule_set_bytes() {
            let root = tempfile::tempdir().unwrap();
            let mut config = serde_json::json!({
                "log":{"level":"info"},
                "inbounds":[{"type":"tun","auto_route":true}],
                "dns":{"servers":[{"type":"udp","server":"192.0.2.53"}]},
                "route":{"auto_detect_interface":true,"rule_set":[
                    {"type":"local","tag":"fixture","format":"binary","path":"fixture.srs"}
                ]}
            });
            let bytes = serde_json::to_vec(&config).unwrap();
            let id = config_id(&bytes, |_, _| Ok(b"original".to_vec())).unwrap();
            let directory = root.path().join("approved").join(&id);
            let verify = || {
                verify_candidate(&directory, &id, |path, max| {
                    read_bounded(File::open(path)?, max)
                })
            };
            assert!(verify().is_err());
            assert!(approved_directory(root.path(), &id).is_err());
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(CONFIG_FILE), &bytes).unwrap();
            assert!(verify().is_err());
            fs::write(directory.join("fixture.srs"), b"original").unwrap();
            assert!(verify().is_ok());
            fs::write(directory.join("fixture.srs"), b"changed").unwrap();
            assert!(verify().is_err());
            fs::write(directory.join("fixture.srs"), b"original").unwrap();
            config["dns"]["servers"][0]["server"] = serde_json::json!("192.0.2.54");
            let changed = serde_json::to_vec(&config).unwrap();
            assert!(config_id(&changed, |_, _| Ok(b"original".to_vec())).is_ok());
            fs::write(directory.join(CONFIG_FILE), &changed).unwrap();
            assert!(verify().is_err());
        }

        #[test]
        fn handshake_rejects_obsolete_and_malformed_requests() {
            let id = "f1".repeat(32);
            for (request, valid) in [
                (format!("EXODYS TUN 2 {id}\n"), true),
                ("EXODYS TUN 1\n".to_owned(), false),
                (format!("EXODYS TUN 2 ../{}\n", &id[3..]), false),
                (format!("EXODYS TUN 2 {}\n", &id[..63]), false),
            ] {
                let (mut client, mut server) = UnixStream::pair().unwrap();
                server.set_read_timeout(Some(TICK)).unwrap();
                client.write_all(request.as_bytes()).unwrap();
                assert_eq!(
                    handshake(&mut server).ok().as_deref(),
                    valid.then_some(id.as_str())
                );
            }
        }

        #[test]
        fn fake_engine_exit_before_start_fails() {
            let (_client, mut server) = UnixStream::pair().unwrap();
            server.set_read_timeout(Some(TICK)).unwrap();
            let child = Command::new("/usr/bin/false")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            assert!(supervise(&mut server, child).is_err());
        }

        #[test]
        fn peer_identity_is_checked() {
            let (client, _server) = UnixStream::pair().unwrap();
            let uid = unsafe { geteuid() };
            assert!(authenticated(&client, uid));
            assert!(!authenticated(&client, uid.wrapping_add(1)));
        }

        #[test]
        fn marker_is_detected_across_reads() {
            let (tx, rx) = mpsc::sync_channel(1);
            watch_output(
                b"prefix sin"
                    .as_slice()
                    .chain(b"g-box started (0.1s)".as_slice()),
                tx,
            );
            assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    if macos::run().is_err() {
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("exodys-tun-service is available only on macOS");
    std::process::exit(1);
}
