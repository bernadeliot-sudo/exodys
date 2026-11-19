use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use crate::Result;

const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PROFILES: usize = 64;
pub const DEFAULT_PORT: u16 = 2080;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub(crate) outbound: Map<String, Value>,
    #[serde(skip)]
    search: String,
}

impl Node {
    pub(crate) fn new(id: String, name: String, outbound: Map<String, Value>) -> Self {
        let mut node = Self {
            id,
            name,
            outbound,
            search: String::new(),
        };
        node.index();
        node
    }

    fn index(&mut self) {
        self.search = format!("{} {} {}", self.name, self.kind(), self.endpoint()).to_lowercase();
    }

    pub fn kind(&self) -> &str {
        self.outbound
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    pub fn endpoint(&self) -> String {
        let address = self
            .outbound
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or("");
        let port = self.outbound.get("server_port").map(|port| {
            port.as_str()
                .map_or_else(|| port.to_string(), str::to_owned)
        });
        let Some(port) = port else {
            return address.to_owned();
        };
        if address.contains(':') {
            format!("[{address}]:{port}")
        } else {
            format!("{address}:{port}")
        }
    }

    pub fn protocol(&self) -> &str {
        match self.kind() {
            "socks" => "SOCKS5",
            "http" => "HTTP",
            "vmess" => "VMess",
            "vless" => "VLESS",
            "trojan" => "Trojan",
            "shadowsocks" => "Shadowsocks",
            "hysteria2" => "Hysteria2",
            "tuic" => "TUIC",
            value => value,
        }
    }
}

#[derive(PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    pub subscription: Option<String>,
    pub nodes: Vec<Node>,
    #[serde(skip)]
    index: HashMap<String, usize>,
}

impl Profile {
    pub(crate) fn new(
        id: u64,
        name: String,
        subscription: Option<String>,
        nodes: Vec<Node>,
    ) -> Self {
        let mut profile = Self {
            id,
            name,
            subscription,
            nodes,
            index: HashMap::new(),
        };
        profile.index_nodes();
        profile
    }

    fn index_nodes(&mut self) {
        self.index = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), index))
            .collect();
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.index.get(id).and_then(|index| self.nodes.get(*index))
    }

    pub fn search(&self, query: &str) -> Matches {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Matches::All(self.nodes.len());
        }
        let terms: Vec<&str> = query.split_whitespace().collect();
        Matches::Filtered(
            self.nodes
                .iter()
                .enumerate()
                .filter_map(|(index, node)| {
                    terms
                        .iter()
                        .all(|term| node.search.contains(term))
                        .then_some(index)
                })
                .collect(),
        )
    }
}

#[derive(Clone, Default, PartialEq)]
pub enum Matches {
    #[default]
    Empty,
    All(usize),
    Filtered(Vec<usize>),
}

impl Matches {
    pub fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::All(count) => *count,
            Self::Filtered(indices) => indices.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn at(&self, position: usize) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::All(count) => (position < *count).then_some(position),
            Self::Filtered(indices) => indices.get(position).copied(),
        }
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub profiles: Vec<Arc<Profile>>,
    next_id: u64,
}

impl Catalog {
    pub fn profile(&self, id: u64) -> Option<&Arc<Profile>> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    pub(crate) fn new_id(&mut self) -> Result<u64> {
        self.next_id = self.next_id.checked_add(1).ok_or("配置编号溢出。")?;
        Ok(self.next_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRef {
    pub profile: u64,
    pub node: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Preferences {
    pub selected: Option<NodeRef>,
    pub port: u16,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            selected: None,
            port: DEFAULT_PORT,
        }
    }
}

pub(crate) struct Store {
    root: PathBuf,
    _lock: File,
}

impl Store {
    pub(crate) fn open(root: PathBuf) -> Result<Self> {
        let mut directory = fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory
            .create(&root)
            .map_err(|_| "无法创建应用数据文件夹。".to_owned())?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(root.join("workspace.lock"))
            .map_err(|_| "无法打开应用数据文件夹。".to_owned())?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|_| "此数据文件夹已由另一个 Exodys 窗口打开。".to_owned())?;
        Ok(Self { root, _lock: lock })
    }

    pub(crate) fn load(&self) -> Result<(Catalog, Preferences)> {
        let mut catalog: Catalog = self.read("profiles.json", MAX_CATALOG_BYTES)?;
        let preferences: Preferences = self.read("preferences.json", 32 * 1024)?;
        let mut profile_ids = HashSet::new();
        if catalog.profiles.len() > MAX_PROFILES || preferences.port < 1024 {
            return Err("本地配置数量或端口无效。".into());
        }
        for profile in &mut catalog.profiles {
            let profile = Arc::get_mut(profile).expect("newly deserialized profile is unique");
            if !profile_ids.insert(profile.id)
                || profile.nodes.is_empty()
                || profile.nodes.len() > crate::import::MAX_NODES
            {
                return Err("本地配置包含无效的配置编号或节点列表。".into());
            }
            profile.index_nodes();
            if profile.index.len() != profile.nodes.len() {
                return Err("本地配置含有重复的节点编号。".into());
            }
            for node in &mut profile.nodes {
                node.index();
            }
            catalog.next_id = catalog.next_id.max(profile.id);
        }
        Ok((catalog, preferences))
    }

    fn read<T: DeserializeOwned + Default>(&self, filename: &str, limit: u64) -> Result<T> {
        let file = match File::open(self.root.join(filename)) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
            Err(_) => return Err(format!("无法读取 {filename}。")),
        };
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| format!("无法读完 {filename}。"))?;
        if bytes.len() as u64 > limit {
            return Err(format!("{filename} 超过大小上限。"));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| format!("{filename} 无法解析。请先备份并修复该文件。"))
    }

    pub(crate) fn save_catalog(&self, catalog: &Catalog) -> Result<()> {
        self.write("profiles.json", catalog, MAX_CATALOG_BYTES)
    }

    pub(crate) fn save_preferences(&self, preferences: &Preferences) -> Result<()> {
        self.write("preferences.json", preferences, 32 * 1024)
    }

    fn write(&self, filename: &str, value: &impl Serialize, limit: u64) -> Result<()> {
        let failed = || format!("无法写入 {filename}。请检查磁盘空间和文件权限。");
        let mut temp = tempfile::NamedTempFile::new_in(&self.root).map_err(|_| failed())?;
        let mut buffer = std::io::BufWriter::new(temp.as_file_mut());
        serde_json::to_writer(&mut buffer, value).map_err(|_| failed())?;
        buffer.flush().map_err(|_| failed())?;
        drop(buffer);
        if temp.as_file().metadata().map_err(|_| failed())?.len() > limit {
            return Err(format!("{filename} 超过大小上限。请删除不使用的配置。"));
        }
        temp.as_file().sync_all().map_err(|_| failed())?;
        temp.persist(self.root.join(filename))
            .map_err(|_| failed())?;
        Ok(())
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.root
    }
}

pub fn default_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("EXODYS_DATA_DIR") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("EXODYS_DATA_DIR 必须是绝对路径。".into());
        }
        return Ok(path);
    }
    let home = std::env::var_os("HOME").ok_or("找不到用户目录。")?;
    Ok(PathBuf::from(home).join("Library/Application Support/Exodys"))
}
