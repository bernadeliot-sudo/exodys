//! A separate process owns the SystemConfiguration transaction. The parent's
//! only write end of the pipe is close-on-exec: EOF also covers a GUI crash.
use crate::Result;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
};

const FLAG: &str = "--exodys-system-proxy-guard";
const JOURNAL: &str = "previous.plist";
const FAILED_RESTORE: &str =
    "无法恢复原系统代理设置。请在本地代理设置中重试恢复，或在 macOS 网络设置中检查代理。";

fn directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or("无法定位系统代理恢复目录。")?;
    Ok(home.join("Library/Application Support/io.github.bernadeliot-sudo.exodys.system-proxy"))
}

fn lock(directory: &Path) -> Result<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|_| "无法创建系统代理恢复目录。")?;
    let meta = fs::symlink_metadata(directory).map_err(|_| "无法检查系统代理恢复目录。")?;
    if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
        return Err("系统代理恢复目录必须是私有文件夹。".into());
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(directory.join("guard.lock"))
        .map_err(|_| "无法锁定系统代理设置。")?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|_| "另一 Exodys 会话正在管理系统代理。".to_owned())?;
    Ok(lock)
}

trait Backend {
    fn apply(&self, port: u16) -> Result<()>;
    fn restore(&self) -> Result<()>;
}

/// Protocol: one READY or ERROR line, then restore on EOF. If the parent
/// vanishes during startup, a failed READY write must also restore.
fn serve(
    backend: &impl Backend,
    port: u16,
    input: &mut impl Read,
    output: &mut impl Write,
) -> Result<()> {
    if let Err(error) = backend.apply(port) {
        backend.restore()?;
        return Err(error);
    }
    if output
        .write_all(b"READY\n")
        .and_then(|_| output.flush())
        .is_ok()
    {
        let mut byte = [0];
        loop {
            match input.read(&mut byte) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(0) | Err(_) => break,
                Ok(_) => (),
            }
        }
    }
    backend.restore()
}

struct Native(std::ffi::CString);
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn exodys_proxy_apply(path: *const std::ffi::c_char, port: u16) -> std::ffi::c_int;
    fn exodys_proxy_restore(path: *const std::ffi::c_char) -> std::ffi::c_int;
}
impl Backend for Native {
    fn apply(&self, port: u16) -> Result<()> {
        let code = unsafe { exodys_proxy_apply(self.0.as_ptr(), port) };
        match code {
            0 => Ok(()),
            2 | 3 => Err("未能取得修改网络设置的授权或锁。检查 macOS 授权后重试。".into()),
            4 => Err("未找到可设置系统代理的主网络服务。请检查 Wi-Fi 或以太网连接。".into()),
            5 => Err(
                "当前网络服务已有不兼容的代理配置。请保留原设置，关闭系统代理模式后重连。".into(),
            ),
            _ => Err(format!("无法启用系统代理（错误 {code}）。")),
        }
    }
    fn restore(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        if unsafe { exodys_proxy_restore(self.0.as_ptr()) } == 0 {
            return Ok(());
        }
        Err(FAILED_RESTORE.into())
    }
}
fn backend(dir: &Path) -> Result<Native> {
    Ok(Native(
        std::ffi::CString::new(dir.join(JOURNAL).as_os_str().as_encoded_bytes())
            .map_err(|_| "系统代理恢复路径无效。")?,
    ))
}

pub fn recover() -> Result<()> {
    let dir = directory()?;
    if !dir.join(JOURNAL).try_exists().map_err(|_| FAILED_RESTORE)? {
        return Ok(());
    }
    let _lock = lock(&dir)?;
    backend(&dir)?.restore()
}

/// Called before opening AppKit. Do not accept a path to a recovery file on the command line.
pub fn helper_entry() -> Option<std::process::ExitCode> {
    let mut args = std::env::args_os().skip(1);
    if args.next()?.to_str()? != FLAG {
        return None;
    }
    let result = (|| {
        let port = args
            .next()
            .and_then(|v| v.to_str()?.parse::<u16>().ok())
            .filter(|p| *p >= 1024)
            .ok_or("无效端口。")?;
        let dir = directory()?;
        let _lock = lock(&dir)?;
        let native = backend(&dir)?;
        if dir.join(JOURNAL).try_exists().map_err(|_| FAILED_RESTORE)? {
            native.restore()?;
        }
        serve(
            &native,
            port,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        )
    })();
    Some(match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stdout(), "ERROR {error}");
            std::process::ExitCode::FAILURE
        }
    })
}

pub(crate) struct Lease {
    child: Option<Child>,
    pipe: Option<ChildStdin>,
}
impl Lease {
    pub fn acquire(port: u16) -> Result<Self> {
        let mut child = Command::new(std::env::current_exe().map_err(|_| "找不到 Exodys 程序。")?)
            .arg(FLAG)
            .arg(port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "无法启动系统代理守护进程。")?;
        let pipe = child.stdin.take();
        let mut reply = String::new();
        let ready = BufReader::new(child.stdout.take().unwrap())
            .take(1024)
            .read_line(&mut reply)
            .is_ok()
            && reply == "READY\n";
        if !ready {
            drop(pipe);
            let _ = child.wait();
            return Err(reply
                .trim()
                .strip_prefix("ERROR ")
                .unwrap_or("系统代理守护进程启动失败。")
                .to_owned());
        }
        Ok(Self {
            child: Some(child),
            pipe,
        })
    }
    pub fn alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }
    pub fn release(mut self) -> Result<()> {
        self.pipe.take();
        self.child
            .take()
            .expect("live lease owns guard")
            .wait()
            .ok()
            .filter(|s| s.success())
            .map(|_| ())
            .ok_or_else(|| FAILED_RESTORE.to_owned())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.pipe.take();
        if let Some(mut child) = self.child.take() {
            // A window may close on AppKit's thread. The helper can finish and
            // retry restoration independently, even if this process terminates.
            let _ = std::thread::Builder::new()
                .name("proxy-guard-reaper".into())
                .spawn(move || child.wait());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::net::UnixStream;
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<&'static str>>,
        fail: bool,
    }
    impl Backend for Fake {
        fn apply(&self, _: u16) -> Result<()> {
            self.calls.borrow_mut().push("apply");
            if self.fail {
                Err("apply".into())
            } else {
                Ok(())
            }
        }
        fn restore(&self) -> Result<()> {
            self.calls.borrow_mut().push("restore");
            Ok(())
        }
    }
    #[test]
    fn loss_of_parent_pipe_restores_after_readiness() {
        let (mut parent, mut guard) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let b = Fake::default();
            let mut writer = guard.try_clone().unwrap();
            serve(&b, 2080, &mut guard, &mut writer).unwrap();
            b.calls.into_inner()
        });
        let mut ready = [0; 6];
        parent.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"READY\n");
        drop(parent);
        assert_eq!(worker.join().unwrap(), ["apply", "restore"]);
    }
    #[test]
    fn native_merge_preserves_unowned_values() {
        unsafe extern "C" {
            fn exodys_proxy_test() -> std::ffi::c_int;
        }
        assert_eq!(unsafe { exodys_proxy_test() }, 0);
    }
    #[test]
    fn failure_does_not_signal_readiness() {
        let b = Fake {
            fail: true,
            ..Fake::default()
        };
        let mut out = Vec::new();
        assert!(serve(&b, 2080, &mut std::io::empty(), &mut out).is_err());
        assert!(out.is_empty());
        assert_eq!(*b.calls.borrow(), ["apply", "restore"]);
    }
}
