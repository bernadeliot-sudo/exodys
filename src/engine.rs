use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{Result, catalog::Node};

const CHECK_WAIT: Duration = Duration::from_secs(4);
const START_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

pub fn address(port: u16) -> SocketAddr {
    (Ipv4Addr::LOCALHOST, port).into()
}

pub fn engine_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("EXODYS_SING_BOX") {
        let path = PathBuf::from(path);
        return (path.is_absolute() && path.is_file())
            .then_some(path)
            .ok_or_else(|| "EXODYS_SING_BOX 必须指向 sing-box 可执行文件。".into());
    }
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths)
        .filter(|directory| directory.is_absolute())
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .map(|dir| dir.join("sing-box"))
        .find(|path| path.is_file())
        .ok_or_else(|| "找不到 sing-box。请运行 brew install sing-box，然后重试。".into())
}

pub(crate) struct Prepared {
    binary: PathBuf,
    file: tempfile::NamedTempFile,
    port: u16,
}

pub(crate) fn prepare(
    binary: &Path,
    directory: &Path,
    node: &Node,
    port: u16,
    routing_rules: &str,
) -> Result<Prepared> {
    let mut outbound = node.outbound.clone();
    outbound.insert("tag".into(), json!("selected"));
    let value = json!({
        "log": { "level": "warn" },
        "inbounds": [{ "type": "mixed", "listen": "127.0.0.1", "listen_port": port }],
        "outbounds": [Value::Object(outbound), { "type": "direct", "tag": "direct" }],
        "dns": { "servers": [{ "type": "local", "tag": "system-dns" }], "final": "system-dns" },
        "route": { "rules": crate::routing::parse(routing_rules)?, "final": "selected", "default_domain_resolver": "system-dns" }
    });
    let mut file =
        tempfile::NamedTempFile::new_in(directory).map_err(|_| "无法创建引擎配置。".to_owned())?;
    serde_json::to_writer(file.as_file_mut(), &value)
        .map_err(|_| "无法写入引擎配置。".to_owned())?;
    file.as_file_mut()
        .flush()
        .map_err(|_| "无法写完引擎配置。".to_owned())?;
    let mut check = ChildGuard::spawn(
        Command::new(binary)
            .arg("check")
            .arg("-c")
            .arg(file.path())
            .stdin(Stdio::null()),
    )?;
    let deadline = Instant::now() + CHECK_WAIT;
    loop {
        match check.0.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => {
                return Err("sing-box 无法使用此节点。请检查协议、认证和传输选项。".into());
            }
            Ok(None) if Instant::now() >= deadline => return Err("sing-box 校验超时。".into()),
            Ok(None) => std::thread::sleep(POLL),
        }
    }
    Ok(Prepared {
        binary: binary.to_owned(),
        file,
        port,
    })
}

impl Prepared {
    pub(crate) fn start(self) -> Result<Proxy> {
        let socket = address(self.port);
        let probe = TcpListener::bind(socket)
            .map_err(|_| format!("端口 {} 不可用。请更换本地端口。", self.port))?;
        drop(probe);
        let child = ChildGuard::spawn(
            Command::new(&self.binary)
                .arg("run")
                .arg("-c")
                .arg(self.file.path())
                .stdin(Stdio::null()),
        )?;
        let mut proxy = Proxy {
            child,
            port: self.port,
        };
        let deadline = Instant::now() + START_WAIT;
        while Instant::now() < deadline {
            if !proxy.alive() {
                return Err("sing-box 在打开本地代理前退出。请检查节点配置。".into());
            }
            if socks_ready(socket) && proxy.alive() {
                return Ok(proxy);
            }
            std::thread::sleep(POLL);
        }
        Err("本地代理启动超时。请检查端口后重试。".into())
    }
}

fn socks_ready(socket: SocketAddr) -> bool {
    let timeout = Duration::from_millis(100);
    let Ok(mut stream) = TcpStream::connect_timeout(&socket, timeout) else {
        return false;
    };
    let mut reply = [0, 0];
    stream.set_write_timeout(Some(timeout)).is_ok()
        && stream.set_read_timeout(Some(timeout)).is_ok()
        && stream.write_all(&[5, 1, 0]).is_ok()
        && stream.read_exact(&mut reply).is_ok()
        && reply == [5, 0]
}

struct ChildGuard(Child);

impl ChildGuard {
    fn spawn(command: &mut Command) -> Result<Self> {
        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(Self)
            .map_err(|_| "无法运行 sing-box。请检查引擎文件的权限。".to_owned())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub(crate) struct Proxy {
    child: ChildGuard,
    port: u16,
}

impl Proxy {
    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.child.0.try_wait(), Ok(None))
    }

    pub(crate) fn probe(&self, url: &str) -> Result<Duration> {
        let proxy = ureq::Proxy::new(&format!("http://{}", address(self.port)))
            .map_err(|_| "本地代理地址无效。".to_owned())?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .max_redirects(0)
            .proxy(Some(proxy))
            .build()
            .into();
        let start = Instant::now();
        let response = agent
            .get(url)
            .call()
            .map_err(|_| "探测未成功。请尝试其他节点。".to_owned())?;
        if !response.status().is_success() {
            return Err(format!(
                "探测服务返回 HTTP {}。",
                response.status().as_u16()
            ));
        }
        Ok(start.elapsed())
    }
}
