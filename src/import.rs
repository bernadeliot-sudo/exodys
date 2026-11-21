//! Read node formats without running provider code or importing listeners, routes or services.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use percent_encoding::percent_decode_str;
use serde_json::{Map, Value, json};
use url::Url;

use crate::{Result, catalog::Node};

pub const MAX_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NODES: usize = 20_000;

pub struct Imported {
    pub name: String,
    pub subscription: Option<String>,
    pub nodes: Vec<Node>,
    pub skipped: usize,
}

pub enum Source {
    Text(String),
    File(std::path::PathBuf),
    Subscription(String),
}

impl Source {
    pub fn load(self) -> Result<Imported> {
        match self {
            Self::Text(text) => {
                let text = text.trim();
                if !text.contains(char::is_whitespace)
                    && (text.starts_with("https://") || text.starts_with("http://"))
                {
                    return Self::Subscription(text.to_owned()).load();
                }
                parse(text.as_bytes(), "粘贴的配置".into(), None)
            }
            Self::File(path) => {
                let name = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("本地配置");
                if !path.metadata().is_ok_and(|info| info.is_file()) {
                    return Err("请选择可读取的配置文件。".into());
                }
                let file = File::open(&path).map_err(|_| "无法打开此配置文件。".to_owned())?;
                let mut bytes = Vec::new();
                file.take(MAX_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "无法读取此配置文件。".to_owned())?;
                parse(&bytes, title(name), None)
            }
            Self::Subscription(link) => {
                let url = Url::parse(&link).map_err(|_| "订阅地址无效。".to_owned())?;
                let host = url.host_str().ok_or("订阅地址缺少主机名。")?;
                if !matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err("请输入不含用户名或密码的 HTTP(S) 订阅地址。".into());
                }
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(15)))
                    .max_redirects(5)
                    .https_only(url.scheme() == "https")
                    .proxy(None)
                    .build()
                    .into();
                let mut response = agent
                    .get(url.as_str())
                    .header("User-Agent", "Exodys/0.1 (sing-box)")
                    .call()
                    .map_err(|error| match error {
                        ureq::Error::StatusCode(code) => {
                            format!("订阅服务器返回 HTTP {code}。请检查订阅是否有效。")
                        }
                        _ => "无法下载订阅。请检查网络和订阅地址。".to_owned(),
                    })?;
                let bytes = response
                    .body_mut()
                    .with_config()
                    .limit(MAX_BYTES as u64 + 1)
                    .read_to_vec()
                    .map_err(|_| "无法读完订阅，或订阅超过 8 MiB。".to_owned())?;
                parse(&bytes, title(host), Some(link))
            }
        }
    }
}

pub fn parse(bytes: &[u8], name: String, subscription: Option<String>) -> Result<Imported> {
    if bytes.len() > MAX_BYTES {
        return Err("配置超过 8 MiB。请将节点拆分成多个配置。".into());
    }
    let content = std::str::from_utf8(bytes)
        .map_err(|_| "此文件并非 UTF-8 文本。".to_owned())?
        .trim_start_matches('\u{feff}')
        .trim();
    if content.is_empty() {
        return Err("请粘贴订阅地址或节点内容。".into());
    }

    let mut report = Imported {
        name,
        subscription,
        nodes: Vec::new(),
        skipped: 0,
    };
    let mut entries = Vec::new();
    if content.starts_with(['{', '[']) {
        let json: Value =
            serde_json::from_str(content).map_err(|_| "JSON 配置语法有误。".to_owned())?;
        if json.get("servers").is_some() && json.get("outbounds").is_none() {
            if json.get("version").and_then(Value::as_u64) != Some(1) {
                return Err("不支持的 SIP008 版本。".into());
            }
            let servers = json
                .get("servers")
                .and_then(Value::as_array)
                .ok_or("SIP008 配置缺少 servers 列表。")?;
            entries.extend(servers.iter().take(MAX_NODES + 1).map(sip008_entry));
        } else if let Some(items) = json.get("outbounds").and_then(Value::as_array) {
            entries.extend(items.iter().take(MAX_NODES + 1).map(sing_box_entry));
        } else if let Some(items) = json.as_array() {
            entries.extend(items.iter().take(MAX_NODES + 1).map(|item| {
                if item.get("type").is_some() {
                    sing_box_entry(item)
                } else {
                    sip008_entry(item)
                }
            }));
        } else {
            entries.push(sing_box_entry(&json));
        }
    } else if has_links(content) {
        entries.extend(link_entries(content).take(MAX_NODES + 1));
    } else if let Some(decoded) = decode_base64(content).filter(|text| has_links(text)) {
        entries.extend(link_entries(&decoded).take(MAX_NODES + 1));
    } else {
        #[derive(serde::Deserialize)]
        struct ClashConfig {
            proxies: Vec<Value>,
        }
        let yaml: ClashConfig = serde_saphyr::from_str(content)
            .map_err(|_| "无法读取 Clash 的 proxies 列表。请确认文件格式和大小。".to_owned())?;
        entries.extend(yaml.proxies.iter().take(MAX_NODES + 1).map(clash_entry));
    }
    if entries.len() > MAX_NODES {
        return Err(format!("单个配置最多支持 {MAX_NODES} 个条目。"));
    }

    let mut seen = HashMap::<String, usize>::new();
    for entry in entries {
        let Some((name, outbound)) = entry else {
            report.skipped += 1;
            continue;
        };
        let name = title(&name);
        let count = seen.entry(name.clone()).or_default();
        *count += 1;
        report
            .nodes
            .push(Node::new(format!("{name}\u{1f}{count}"), name, outbound));
    }
    if report.nodes.is_empty() {
        return Err("未找到可用节点。请检查节点的协议、地址、端口和认证信息。".into());
    }
    Ok(report)
}

fn title(s: &str) -> String {
    let label: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    if label.is_empty() {
        "未命名节点".into()
    } else {
        label
    }
}

type Entry = Option<(String, Map<String, Value>)>;

fn sip008_entry(value: &Value) -> Entry {
    let host = required(value, "server")?;
    let mut node = base_outbound("shadowsocks", host, port(value.get("server_port")?)?)?;
    node.insert("method".into(), json!(required(value, "method")?));
    auth(&mut node, value, "password")?;
    // Plugin options are plugin-specific; ignoring them can change the connection.
    for key in ["plugin", "plugin_opts"] {
        if let Some(option) = value.get(key)
            && !option.as_str().is_some_and(str::is_empty)
        {
            return None;
        }
    }
    Some((required(value, "remarks").unwrap_or(host).to_owned(), node))
}

pub(crate) fn valid_saved_outbound(fields: &Map<String, Value>) -> bool {
    sing_box_entry(&Value::Object(fields.clone())).is_some()
}

fn sing_box_entry(value: &Value) -> Entry {
    let fields = value.as_object()?;
    let protocol = fields.get("type")?.as_str()?;
    if !matches!(
        protocol,
        "socks" | "http" | "shadowsocks" | "trojan" | "vless" | "vmess" | "hysteria2" | "tuic"
    ) || has_local_refs(value)
    {
        return None;
    }
    if matches!(protocol, "vless" | "vmess" | "tuic") {
        valid_uuid(fields.get("uuid")?.as_str()?)?;
    }
    let server = valid_address(fields.get("server")?.as_str()?)?.trim_matches(['[', ']']);
    let port = port(fields.get("server_port")?)?;
    let name = fields
        .get("tag")
        .and_then(Value::as_str)
        .unwrap_or(server)
        .to_owned();
    let mut node = fields.clone();
    node.remove("tag");
    node.insert("server".into(), json!(server));
    node.insert("server_port".into(), json!(port));
    Some((name, node))
}

pub(crate) fn has_local_refs(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(has_local_refs),
        Value::Object(map) => map.iter().any(|(key, item)| {
            let key = key.to_ascii_lowercase();
            key.ends_with("_path")
                || key.ends_with("_paths")
                || matches!(
                    key.as_str(),
                    "detour"
                        | "domain_resolver"
                        | "network_namespace"
                        | "network_namespace_path"
                        | "plugin_path"
                        | "process_path"
                )
                || has_local_refs(item)
        }),
        _ => false,
    }
}

fn valid_address(host: &str) -> Option<&str> {
    (!host.is_empty() && !host.contains(char::is_whitespace) && !host.contains(['/', '\\', '\0']))
        .then_some(host)
}

fn valid_uuid(value: &str) -> Option<&str> {
    let sections = [8, 4, 4, 4, 12];
    let mut parts = value.split('-');
    for expected in sections {
        let part = parts.next()?;
        if part.len() != expected || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
    }
    parts.next().is_none().then_some(value)
}

fn port(value: &Value) -> Option<u16> {
    let number = value.as_u64().or_else(|| value.as_str()?.parse().ok())?;
    u16::try_from(number).ok().filter(|port| *port != 0)
}

fn base_outbound(kind: &str, host: &str, port: u16) -> Option<Map<String, Value>> {
    valid_address(host)?;
    (port > 0).then(|| {
        Map::from_iter([
            ("type".into(), json!(kind)),
            ("server".into(), json!(host.trim_matches(['[', ']']))),
            ("server_port".into(), json!(port)),
        ])
    })
}

fn required<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str().filter(|text| !text.is_empty())
}

fn auth(map: &mut Map<String, Value>, value: &Value, key: &str) -> Option<()> {
    map.insert(key.to_owned(), json!(required(value, key)?));
    Some(())
}

fn credentials(node: &mut Map<String, Value>, source: &Value) -> Option<()> {
    if source.get("username").is_some() || source.get("password").is_some() {
        auth(node, source, "username")?;
        auth(node, source, "password")?;
    }
    Some(())
}

fn clash_entry(source: &Value) -> Entry {
    let kind = match required(source, "type")? {
        "ss" => "shadowsocks",
        "socks5" => "socks",
        "hy2" => "hysteria2",
        "http" | "trojan" | "vless" | "vmess" | "hysteria2" | "tuic" => required(source, "type")?,
        _ => return None,
    };
    if source.get("dialer-proxy").is_some() || source.get("proxy").is_some() {
        return None;
    }
    let host = required(source, "server")?;
    let mut node = base_outbound(kind, host, port(source.get("port")?)?)?;
    match kind {
        "shadowsocks" => {
            node.insert("method".into(), json!(required(source, "cipher")?));
            auth(&mut node, source, "password")?;
            if let Some(plugin) = required(source, "plugin") {
                let opts = source.get("plugin-opts").and_then(Value::as_object)?;
                let (plugin, options) = match plugin {
                    "obfs" => (
                        "obfs-local",
                        format!(
                            "obfs={};obfs-host={}",
                            required(&source["plugin-opts"], "mode")?,
                            opts.get("host").and_then(Value::as_str).unwrap_or("")
                        ),
                    ),
                    "v2ray-plugin" => {
                        let config = &source["plugin-opts"];
                        let mode = required(config, "mode").unwrap_or("websocket");
                        if mode != "websocket" {
                            return None;
                        }
                        let host = required(config, "host").unwrap_or("");
                        let path = required(config, "path").unwrap_or("/");
                        let tls = if config["tls"].as_bool().unwrap_or(false) {
                            ";tls"
                        } else {
                            ""
                        };
                        (
                            "v2ray-plugin",
                            format!("mode={mode};host={host};path={path}{tls}"),
                        )
                    }
                    _ => return None,
                };
                node.insert("plugin".into(), json!(plugin));
                node.insert("plugin_opts".into(), json!(options));
            }
        }
        "trojan" | "hysteria2" => {
            auth(&mut node, source, "password")?;
        }
        "vless" | "vmess" | "tuic" => {
            let uuid = required(source, "uuid")?;
            valid_uuid(uuid)?;
            auth(&mut node, source, "uuid")?;
        }
        _ => credentials(&mut node, source)?,
    }
    if kind == "vless" {
        if let Some(flow) = required(source, "flow") {
            node.insert("flow".into(), json!(flow));
        }
        if let Some(encoding) = required(source, "packet-encoding") {
            node.insert("packet_encoding".into(), json!(encoding));
        }
    }
    if kind == "vmess" {
        node.insert(
            "security".into(),
            json!(required(source, "cipher").unwrap_or("auto")),
        );
        node.insert(
            "alter_id".into(),
            json!(source["alterId"].as_u64().unwrap_or(0)),
        );
    }
    if kind == "tuic" {
        auth(&mut node, source, "password")?;
        for (from, to) in [
            ("congestion-controller", "congestion_control"),
            ("udp-relay-mode", "udp_relay_mode"),
        ] {
            if let Some(option) = required(source, from) {
                node.insert(to.into(), json!(option));
            }
        }
    }
    if kind == "hysteria2"
        && let Some(obfs) = required(source, "obfs")
    {
        node.insert(
            "obfs".into(),
            json!({"type": obfs, "password": required(source, "obfs-password")?}),
        );
    }
    if source.get("ports").is_some() || source.get("fingerprint").is_some() {
        return None;
    }

    let reality = source.get("reality-opts");
    if matches!(kind, "trojan" | "hysteria2" | "tuic")
        || source["tls"].as_bool() == Some(true)
        || reality.is_some()
    {
        let mut tls = tls_options(
            required(source, "servername").or_else(|| required(source, "sni")),
            source["skip-cert-verify"].as_bool() == Some(true),
            required(source, "client-fingerprint"),
        );
        if source["alpn"].is_array() {
            tls["alpn"] = source["alpn"].clone();
        }
        if let Some(reality) = reality {
            tls["reality"] = json!({"enabled": true, "public_key": required(reality, "public-key")?, "short_id": reality["short-id"].as_str().unwrap_or("")});
        }
        node.insert("tls".into(), tls);
    }
    let network = required(source, "network").unwrap_or("tcp");
    let ws = &source["ws-opts"];
    let grpc = &source["grpc-opts"];
    let upgrade = &source["http-upgrade-opts"];
    let transport = match network {
        "tcp" | "udp" => None,
        "ws" => {
            let mut transport = json!({"type": "ws", "path": required(ws, "path").unwrap_or("/")});
            for (from, to) in [
                ("headers", "headers"),
                ("max-early-data", "max_early_data"),
                ("early-data-header-name", "early_data_header_name"),
            ] {
                if let Some(value) = ws.get(from) {
                    transport[to] = value.clone();
                }
            }
            Some(transport)
        }
        "grpc" => Some(
            json!({"type": "grpc", "service_name": required(grpc, "grpc-service-name").unwrap_or("")}),
        ),
        "httpupgrade" => Some(
            json!({"type": "httpupgrade", "host": required(upgrade, "host").unwrap_or(host), "path": required(upgrade, "path").unwrap_or("/")}),
        ),
        _ => return None,
    };
    if let Some(transport) = transport {
        node.insert("transport".into(), transport);
    }
    if has_local_refs(&Value::Object(node.clone())) {
        return None;
    }
    Some((required(source, "name").unwrap_or(host).to_owned(), node))
}

fn tls_options(sni: Option<&str>, insecure: bool, fingerprint: Option<&str>) -> Value {
    let mut tls = json!({"enabled": true, "insecure": insecure});
    if let Some(sni) = sni.filter(|sni| !sni.is_empty()) {
        tls["server_name"] = json!(sni);
    }
    if let Some(fingerprint) = fingerprint.filter(|value| !value.is_empty() && *value != "none") {
        tls["utls"] = json!({"enabled": true, "fingerprint": fingerprint});
    }
    tls
}

fn has_links(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim();
        line.split_once("://").is_some_and(|(scheme, _)| {
            !scheme.is_empty()
                && scheme
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    })
}

fn link_entries(content: &str) -> impl Iterator<Item = Entry> + '_ {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(link_entry)
}

fn decode_base64(s: &str) -> Option<String> {
    let compact: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    [
        general_purpose::STANDARD,
        general_purpose::STANDARD_NO_PAD,
        general_purpose::URL_SAFE,
        general_purpose::URL_SAFE_NO_PAD,
    ]
    .iter()
    .find_map(|engine| {
        engine
            .decode(&compact)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    })
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn link_entry(text: &str) -> Entry {
    let (scheme, body) = text.split_once("://")?;
    if scheme == "vmess" {
        return vmess_entry(body);
    }
    if scheme == "ss" {
        return ss_entry(body);
    }
    let kind = match scheme {
        "hy2" => "hysteria2",
        "socks5" => "socks",
        "trojan" | "vless" | "hysteria2" | "tuic" | "socks" => scheme,
        _ => return None,
    };
    let url = Url::parse(text).ok()?;
    let host = url.host_str()?;
    let mut node = base_outbound(kind, host, url.port()?)?;
    let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let param = |key: &str| {
        params
            .get(key)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    };
    let identity = decode(url.username());
    match kind {
        "vless" | "tuic" => {
            valid_uuid(&identity)?;
            node.insert("uuid".into(), json!(identity));
        }
        "trojan" | "hysteria2" => {
            if identity.is_empty() || url.password().is_some() {
                return None;
            }
            node.insert("password".into(), json!(identity));
        }
        "socks" if !identity.is_empty() => {
            node.insert("username".into(), json!(identity));
            node.insert("password".into(), json!(decode(url.password()?)));
        }
        _ => {}
    }
    if kind == "vless" {
        if !matches!(param("encryption"), None | Some("none")) {
            return None;
        }
        if let Some(flow) = param("flow") {
            node.insert("flow".into(), json!(flow));
        }
    }
    if kind == "hysteria2" {
        if param("mport").is_some() || param("pinSHA256").is_some() {
            return None;
        }
        if let Some(obfs) = param("obfs") {
            node.insert(
                "obfs".into(),
                json!({"type": obfs, "password": param("obfs-password")?}),
            );
        }
    }
    if kind == "tuic" {
        node.insert("password".into(), json!(decode(url.password()?)));
        for key in ["congestion_control", "udp_relay_mode"] {
            if let Some(value) = param(key) {
                node.insert(key.into(), json!(value));
            }
        }
    }
    let security =
        param("security").unwrap_or(if matches!(kind, "trojan" | "hysteria2" | "tuic") {
            "tls"
        } else {
            "none"
        });
    match security {
        "tls" | "reality" => {
            let mut tls = tls_options(
                param("sni"),
                matches!(
                    param("allowInsecure").or_else(|| param("insecure")),
                    Some("1" | "true")
                ),
                param("fp"),
            );
            if let Some(alpn) = param("alpn") {
                tls["alpn"] = json!(alpn.split(',').collect::<Vec<_>>());
            }
            if security == "reality" {
                tls["reality"] = json!({"enabled": true, "public_key": param("pbk")?, "short_id": param("sid").unwrap_or("")});
            }
            node.insert("tls".into(), tls);
        }
        "none" => {}
        _ => return None,
    }
    if let Some(transport) = link_transport(
        param("type").unwrap_or("tcp"),
        param("host"),
        param("path"),
        param("serviceName"),
    )? {
        node.insert("transport".into(), transport);
    }
    Some((
        url.fragment()
            .map(decode)
            .unwrap_or_else(|| host.to_owned()),
        node,
    ))
}

fn link_transport(
    kind: &str,
    host: Option<&str>,
    path: Option<&str>,
    service_name: Option<&str>,
) -> Option<Option<Value>> {
    Some(match kind {
        "tcp" | "none" => None,
        "ws" => {
            let mut value = json!({"type": "ws", "path": path.unwrap_or("/")});
            if let Some(host) = host {
                value["headers"] = json!({"Host": host});
            }
            Some(value)
        }
        "grpc" => {
            Some(json!({"type": "grpc", "service_name": service_name.or(path).unwrap_or("")}))
        }
        "http" | "h2" => Some(
            json!({"type": "http", "host": host.into_iter().collect::<Vec<_>>(), "path": path.unwrap_or("/")}),
        ),
        "httpupgrade" => Some(
            json!({"type": "httpupgrade", "host": host.unwrap_or(""), "path": path.unwrap_or("/")}),
        ),
        _ => return None,
    })
}

fn vmess_entry(body: &str) -> Entry {
    let config: Value = serde_json::from_str(&decode_base64(body)?).ok()?;
    let host = required(&config, "add")?;
    let mut node = base_outbound("vmess", host, port(config.get("port")?)?)?;
    let uuid = required(&config, "id")?;
    valid_uuid(uuid)?;
    node.insert("uuid".into(), json!(uuid));
    node.insert(
        "security".into(),
        json!(required(&config, "scy").unwrap_or("auto")),
    );
    let alter_id = config["aid"]
        .as_u64()
        .or_else(|| config["aid"].as_str()?.parse().ok())
        .unwrap_or(0);
    node.insert("alter_id".into(), json!(alter_id));
    if required(&config, "tls") == Some("tls") {
        node.insert(
            "tls".into(),
            tls_options(
                required(&config, "sni").or_else(|| required(&config, "host")),
                false,
                required(&config, "fp"),
            ),
        );
    }
    if let Some(transport) = link_transport(
        required(&config, "net").unwrap_or("tcp"),
        required(&config, "host"),
        required(&config, "path"),
        None,
    )? {
        node.insert("transport".into(), transport);
    }
    Some((required(&config, "ps").unwrap_or(host).to_owned(), node))
}

fn ss_entry(body: &str) -> Entry {
    let (body, label) = body
        .split_once('#')
        .map_or((body, None), |(body, label)| (body, Some(decode(label))));
    let (body, query) = body.split_once('?').unwrap_or((body, ""));
    let (user_info, address) = if let Some((user_info, address)) = body.rsplit_once('@') {
        let user_info = decode(user_info);
        let user_info = if user_info.contains(':') {
            user_info
        } else {
            decode_base64(&user_info)?
        };
        (user_info, address.trim_end_matches('/').to_owned())
    } else {
        let parts = decode_base64(body)?;
        let (user_info, address) = parts.rsplit_once('@')?;
        (user_info.to_owned(), address.to_owned())
    };
    let (method, password) = user_info.split_once(':')?;
    if method.is_empty() || password.is_empty() {
        return None;
    }
    let url = Url::parse(&format!("ss://{address}")).ok()?;
    let host = url.host_str()?;
    let mut node = base_outbound("shadowsocks", host, url.port()?)?;
    node.insert("method".into(), json!(method));
    node.insert("password".into(), json!(password));
    if let Some((_, plugin)) =
        url::form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == "plugin")
    {
        let (name, opts) = plugin.split_once(';').unwrap_or((&plugin, ""));
        let name = match name {
            "simple-obfs" | "obfs-local" => "obfs-local",
            "v2ray-plugin" => name,
            _ => return None,
        };
        node.insert("plugin".into(), json!(name));
        node.insert("plugin_opts".into(), json!(opts));
    }
    Some((label.unwrap_or_else(|| host.to_owned()), node))
}
