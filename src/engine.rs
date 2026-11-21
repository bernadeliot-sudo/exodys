use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{Result, catalog::Node};

#[cfg(test)]
mod tests;
#[cfg(target_os = "windows")]
mod windows_job;

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
    #[cfg(target_os = "windows")]
    let (executable, extra, missing) = (
        "sing-box.exe",
        &[] as &[&str],
        "找不到 sing-box.exe。请将它的目录加入 PATH，或用 EXODYS_SING_BOX 指定绝对路径。",
    );
    #[cfg(not(target_os = "windows"))]
    let (executable, extra, missing) = (
        "sing-box",
        &["/opt/homebrew/bin", "/usr/local/bin"] as &[&str],
        "找不到 sing-box。请运行 brew install sing-box，然后重试。",
    );
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths)
        .filter(|directory| directory.is_absolute())
        .chain(extra.iter().map(PathBuf::from))
        .map(|dir| dir.join(executable))
        .find(|path| path.is_file())
        .ok_or_else(|| missing.into())
}

pub(crate) struct Prepared {
    binary: PathBuf,
    file: tempfile::NamedTempFile,
    port: u16,
}

pub(crate) enum Selection<'a> {
    Manual(&'a Node),
    Automatic(Vec<&'a Node>),
}

pub(crate) struct Policy {
    outbounds: Vec<Value>,
    rules: Vec<Value>,
}

pub(crate) fn connection_policy(
    rules: Vec<Value>,
    selected: Selection<'_>,
    groups: Vec<(&str, Selection<'_>)>,
) -> Result<Policy> {
    let mut outbounds = Vec::new();
    for (tag, selection) in std::iter::once(("selected".to_owned(), selected)).chain(
        groups
            .into_iter()
            .map(|(name, selection)| (crate::routing::group_tag(name), selection)),
    ) {
        match selection {
            Selection::Manual(node) => outbounds.push(tagged_node(node, &tag)),
            Selection::Automatic(nodes) => {
                append_auto_group(&mut outbounds, &tag, &nodes, PROBE_URL, "1m")?
            }
        }
    }
    Ok(Policy { outbounds, rules })
}

fn tagged_node(node: &Node, tag: &str) -> Value {
    let mut outbound = node.outbound.clone();
    outbound.insert("tag".into(), json!(tag));
    Value::Object(outbound)
}

const PROBE_URL: &str = "https://www.cloudflare.com/cdn-cgi/trace";

#[cfg(test)]
fn prepare_auto(
    binary: &Path,
    directory: &Path,
    nodes: &[&Node],
    port: u16,
    routing_rules: &str,
) -> Result<Prepared> {
    prepare_auto_with(
        binary,
        directory,
        nodes,
        port,
        routing_rules,
        PROBE_URL,
        "1m",
    )
}

#[cfg(test)]
fn prepare_auto_with(
    binary: &Path,
    directory: &Path,
    nodes: &[&Node],
    port: u16,
    routing_rules: &str,
    url: &str,
    interval: &str,
) -> Result<Prepared> {
    let mut outbounds = Vec::new();
    append_auto_group(&mut outbounds, "selected", nodes, url, interval)?;
    prepare_connection(
        binary,
        directory,
        port,
        Policy {
            outbounds,
            rules: crate::routing::parse(routing_rules)?,
        },
    )
}

fn append_auto_group(
    outbounds: &mut Vec<Value>,
    group: &str,
    nodes: &[&Node],
    url: &str,
    interval: &str,
) -> Result<()> {
    if nodes.is_empty() || nodes.len() > crate::catalog::MAX_AUTO_MEMBERS {
        return Err("自动组须包含 1 至 32 个节点。".into());
    }
    let mut tags = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        let tag = format!("{group}:member:{index}");
        outbounds.push(tagged_node(node, &tag));
        tags.push(tag);
    }
    outbounds.push(
        json!({"type":"urltest", "tag":group, "outbounds":tags, "url":url, "interval":interval}),
    );
    Ok(())
}

pub(crate) fn prepare_connection(
    binary: &Path,
    directory: &Path,
    port: u16,
    policy: Policy,
) -> Result<Prepared> {
    let rule_set = crate::rule_sets::definitions(directory, &policy.rules)?;
    let value = proxy_config(
        policy,
        rule_set,
        json!({ "type": "mixed", "listen": "127.0.0.1", "listen_port": port }),
        json!({ "type": "local", "tag": "system-dns" }),
    );
    let file = checked_config(
        binary,
        directory,
        &value,
        "sing-box 无法使用此连接配置。请检查节点和分流规则。",
    )?;
    Ok(Prepared {
        binary: binary.to_owned(),
        file,
        port,
    })
}

fn proxy_config(mut policy: Policy, rule_set: Vec<Value>, inbound: Value, dns: Value) -> Value {
    policy
        .outbounds
        .push(json!({ "type": "direct", "tag": "direct" }));
    let resolver = dns["tag"].clone();
    json!({
        "log": { "level": "warn" },
        "inbounds": [inbound],
        "outbounds": policy.outbounds,
        "dns": { "servers": [dns], "final": resolver },
        "route": { "rules": policy.rules, "rule_set": rule_set, "final": "selected", "default_domain_resolver": resolver }
    })
}

pub(crate) fn tun_candidate(
    directory: &Path,
    dns: IpAddr,
    policy: Policy,
    candidate: Option<&Path>,
) -> Result<String> {
    use crate::tun_config::{CONFIG_FILE, config_id};

    let value = tun_config(dns, policy)?;
    let bytes = serde_json::to_vec(&value).map_err(|_| "无法生成 TUN 配置。")?;
    let id = config_id(&bytes, |filename, _| {
        let name = filename.strip_suffix(".srs").expect("validated filename");
        let bytes =
            crate::rule_sets::backup_bytes(directory, name).map_err(std::io::Error::other)?;
        if let Some(candidate) = candidate {
            write_candidate_file(candidate, filename, &bytes)?;
        }
        Ok(bytes)
    })
    .map_err(|error| format!("无法生成 TUN 配置。{error}"))?;
    if let Some(candidate) = candidate {
        write_candidate_file(candidate, CONFIG_FILE, &bytes).map_err(|_| "无法写入 TUN 配置。")?;
    }
    Ok(id)
}

fn write_candidate_file(directory: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(directory.join(name))?;
    Ok(())
}

pub(crate) fn check_tun_candidate(binary: &Path, candidate: &Path) -> Result<()> {
    // `check` constructs and closes the config. This entry point cannot start it.
    let candidate = candidate
        .canonicalize()
        .map_err(|_| "无法定位 TUN 配置。")?;
    let mut command = Command::new(binary);
    command
        .arg("check")
        .arg("-c")
        .arg(crate::tun_config::CONFIG_FILE)
        .current_dir(candidate);
    check_config_file(
        &mut command,
        "sing-box 无法校验 TUN 配置。请确认使用 1.14 或更新版本，并检查节点和规则。",
    )
}

fn tun_config(dns: IpAddr, mut policy: Policy) -> Result<Value> {
    let dns = dns.to_canonical();
    if dns.is_unspecified()
        || dns.is_loopback()
        || dns.is_multicast()
        || match dns {
            IpAddr::V4(v4) => v4.is_broadcast() || v4.is_link_local(),
            IpAddr::V6(v6) => v6.is_unicast_link_local(),
        }
    {
        return Err("请提供物理网络可达的 DNS 服务器 IP 地址。".into());
    }
    for outbound in &policy.outbounds {
        if [
            "bind_interface",
            "inet4_bind_address",
            "inet6_bind_address",
            "network_strategy",
            "network_type",
            "fallback_network_type",
        ]
        .iter()
        .any(|key| outbound.get(*key).is_some())
        {
            return Err("TUN 需要检测物理网卡。请移除节点中的网卡或源地址绑定。".into());
        }
    }
    policy.rules.splice(
        ..0,
        [
            json!({ "action": "sniff" }),
            json!({ "protocol": "dns", "action": "hijack-dns" }),
        ],
    );
    let rule_set = crate::rule_sets::reference_names(&policy.rules)
        .into_iter()
        .map(|name| crate::rule_sets::definition(name, Path::new(&format!("{name}.srs"))))
        .collect();
    let mut config = proxy_config(
        policy,
        rule_set,
        json!({
            "type": "tun",
            "address": ["172.19.0.1/30", "fdfe:dcba:9876::1/126"],
            "auto_route": true,
            "dns_mode": "hijack"
        }),
        // A literal resolver avoids resolving the DNS server through the TUN itself.
        json!({ "type": "udp", "tag": "tun-dns", "server": dns.to_string() }),
    );
    config["log"]["level"] = json!("info");
    config["route"]["auto_detect_interface"] = json!(true);
    Ok(config)
}

pub(crate) fn validate_rule_set(binary: &Path, directory: &Path, path: &Path) -> Result<()> {
    let path = path
        .canonicalize()
        .map_err(|_| "无法定位待校验的规则集。")?;
    let path = path.to_str().ok_or("规则集路径不是 UTF-8。")?;
    let value = json!({
        "outbounds": [{ "type": "direct", "tag": "direct" }],
        "route": {
            "rule_set": [crate::rule_sets::definition("candidate", Path::new(path))],
            "rules": [{ "rule_set": "candidate", "action": "reject" }],
            "final": "direct"
        }
    });
    checked_config(
        binary,
        directory,
        &value,
        "sing-box 无法读取此规则集。请检查文件格式和引擎版本。",
    )?;
    Ok(())
}

fn checked_config(
    binary: &Path,
    directory: &Path,
    value: &Value,
    invalid: &str,
) -> Result<tempfile::NamedTempFile> {
    let mut file =
        tempfile::NamedTempFile::new_in(directory).map_err(|_| "无法创建引擎配置。".to_owned())?;
    serde_json::to_writer(file.as_file_mut(), value)
        .map_err(|_| "无法写入引擎配置。".to_owned())?;
    file.as_file_mut()
        .flush()
        .map_err(|_| "无法写完引擎配置。".to_owned())?;
    check_config_file(
        Command::new(binary).arg("check").arg("-c").arg(file.path()),
        invalid,
    )?;
    Ok(file)
}

fn check_config_file(command: &mut Command, invalid: &str) -> Result<()> {
    let mut check = ChildGuard::spawn(command.stdin(Stdio::null()))?;
    let deadline = Instant::now() + CHECK_WAIT;
    loop {
        match check.child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) | Err(_) => return Err(invalid.into()),
            Ok(None) if Instant::now() >= deadline => return Err("sing-box 校验超时。".into()),
            Ok(None) => std::thread::sleep(POLL),
        }
    }
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

struct ChildGuard {
    child: Child,
    #[cfg(target_os = "windows")]
    _job: std::os::windows::io::OwnedHandle,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(target_os = "windows")]
        let job = windows_job::prepare(command)?;
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "无法运行 sing-box。请检查引擎文件的权限。".to_owned())?;
        let guard = Self {
            child,
            #[cfg(target_os = "windows")]
            _job: job,
        };
        #[cfg(target_os = "windows")]
        let guard = {
            let mut guard = guard;
            windows_job::assign(&guard._job, &mut guard.child)?;
            guard
        };
        Ok(guard)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub(crate) struct Proxy {
    child: ChildGuard,
    port: u16,
}

impl Proxy {
    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.child.child.try_wait(), Ok(None))
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
