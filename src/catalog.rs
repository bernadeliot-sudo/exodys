use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use crate::Result;

pub(crate) const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;
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

pub const DEFAULT_REFRESH_HOURS: u16 = 24;
pub const MAX_REFRESH_HOURS: u16 = 30 * 24;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    pub subscription: Option<String>,
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub last_subscription_update: Option<u64>,
    #[serde(default)]
    pub refresh_hours: Option<u16>,
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
            last_subscription_update: None,
            refresh_hours: None,
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

    pub(crate) fn with_profiles(&self, other: &Self, profiles: Vec<Arc<Profile>>) -> Self {
        Self {
            profiles,
            next_id: self.next_id.max(other.next_id),
        }
    }

    pub(crate) fn new_id(&mut self) -> Result<u64> {
        use ring::rand::{SecureRandom, SystemRandom};

        // Preserve legacy IDs and use the upper half of the JSON-safe range for
        // independent imports, so devices need no shared counter while offline.
        let random = SystemRandom::new();
        let mut bytes = [0; 8];
        for _ in 0..8 {
            random.fill(&mut bytes).map_err(|_| "无法生成配置编号。")?;
            let id = (u64::from_ne_bytes(bytes) & ((1 << 52) - 1)) | (1 << 52);
            if self.profile(id).is_none() {
                self.next_id = self.next_id.max(id);
                return Ok(id);
            }
        }
        Err("无法生成唯一的配置编号。".into())
    }
}

pub(crate) const MAX_PREFERENCES_BYTES: u64 = 512 * 1024;
pub const MAX_AUTO_MEMBERS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeRef {
    pub profile: u64,
    pub node: String,
}

pub const MAX_POLICY_GROUPS: usize = 8;

pub(crate) fn valid_policy_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyMode {
    Automatic,
    Manual(Option<NodeRef>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyGroup {
    pub name: String,
    pub members: Vec<NodeRef>,
    pub mode: PolicyMode,
}

impl PolicyGroup {
    pub(crate) fn retain_members(&mut self, keep: impl FnMut(&NodeRef) -> bool) -> usize {
        let before = self.members.len();
        self.members.retain(keep);
        if let PolicyMode::Manual(Some(member)) = &self.mode
            && !self.members.contains(member)
        {
            self.mode = PolicyMode::Manual(None);
        }
        before - self.members.len()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Preferences {
    pub selected: Option<NodeRef>,
    pub port: u16,
    #[serde(default)]
    pub routing_rules: String,
    #[serde(default)]
    pub system_proxy: bool,
    #[serde(default)]
    pub auto_members: Vec<NodeRef>,
    #[serde(default)]
    pub auto_enabled: bool,
    #[serde(default)]
    pub scheduled_refresh: bool,
    #[serde(default)]
    pub policy_groups: Vec<PolicyGroup>,
    #[serde(default)]
    pub rule_set_updates: std::collections::BTreeMap<String, crate::rule_sets::Remote>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            selected: None,
            port: DEFAULT_PORT,
            routing_rules: String::new(),
            system_proxy: false,
            auto_members: vec![],
            auto_enabled: false,
            scheduled_refresh: false,
            policy_groups: Vec::new(),
            rule_set_updates: Default::default(),
        }
    }
}

const ACTIVE_WORKSPACE: &str = "active-workspace";
const TUN_DNS: &str = "tun-dns.json";
const TUN_CANDIDATE_PREFIX: &str = ".tun-candidate-";
const SYNC_STATE: &str = "sync-state.json";
const GENERATION_PREFIX: &str = ".restored-";

pub(crate) struct Store {
    root: PathBuf,
    active: PathBuf,
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
        let marker = root.join(ACTIVE_WORKSPACE);
        let active = match marker.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => root.clone(),
            Ok(meta) if meta.is_file() && meta.len() <= 64 => {
                let name = fs::read_to_string(marker).map_err(|_| "无法读取现用配置库。")?;
                root.join(generation_name(&name)?)
            }
            _ => return Err("现用配置库记录无效。".into()),
        };
        let metadata = if active == root {
            active.metadata()
        } else {
            active.symlink_metadata()
        };
        if !metadata.is_ok_and(|meta| meta.is_dir()) {
            return Err("找不到现用配置库。".into());
        }
        Ok(Self {
            root,
            active,
            _lock: lock,
        })
    }

    pub(crate) fn load(&self) -> Result<(Catalog, Preferences)> {
        let catalog = Self::read(&self.active, "profiles.json", MAX_CATALOG_BYTES)?;
        let preferences = Self::read(&self.active, "preferences.json", MAX_PREFERENCES_BYTES)?;
        Self::validate(catalog, preferences)
    }

    pub(crate) fn validate(
        mut catalog: Catalog,
        preferences: Preferences,
    ) -> Result<(Catalog, Preferences)> {
        crate::routing::parse(&preferences.routing_rules)?;
        if preferences.auto_members.len() > MAX_AUTO_MEMBERS
            || preferences
                .auto_members
                .iter()
                .collect::<HashSet<_>>()
                .len()
                != preferences.auto_members.len()
        {
            return Err("本地自动组成员重复或数量超过上限。".into());
        }
        let mut names = HashSet::new();
        if preferences.policy_groups.len() > MAX_POLICY_GROUPS
            || preferences.policy_groups.iter().any(|group| {
                !valid_policy_name(&group.name)
                    || !names.insert(&group.name)
                    || group.members.len() > MAX_AUTO_MEMBERS
                    || group.members.iter().collect::<HashSet<_>>().len() != group.members.len()
            })
        {
            return Err("本地策略组名称或成员无效。".into());
        }
        if preferences.rule_set_updates.len() > crate::rule_sets::MAX_FILES
            || preferences.rule_set_updates.iter().any(|(name, remote)| {
                !crate::rule_sets::valid_name(name)
                    || crate::rule_sets::Remote::new(&remote.url).is_err()
            })
        {
            return Err("本地规则集更新来源无效。".into());
        }
        let mut profile_ids = HashSet::new();
        if catalog.profiles.len() > MAX_PROFILES || preferences.port < 1024 {
            return Err("本地配置数量或端口无效。".into());
        }
        for profile in &mut catalog.profiles {
            let profile = Arc::get_mut(profile).expect("newly deserialized profile is unique");
            if !profile_ids.insert(profile.id)
                || profile.nodes.is_empty()
                || profile.nodes.len() > crate::import::MAX_NODES
                || profile.refresh_hours.is_some_and(|hours| {
                    hours > MAX_REFRESH_HOURS || profile.subscription.is_none()
                })
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

    fn read<T: DeserializeOwned + Default>(
        directory: &Path,
        filename: &str,
        limit: u64,
    ) -> Result<T> {
        let file = match File::open(directory.join(filename)) {
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
        Self::write(&self.active, "profiles.json", catalog, MAX_CATALOG_BYTES)
    }

    pub(crate) fn save_preferences(&self, preferences: &Preferences) -> Result<()> {
        Self::write(
            &self.active,
            "preferences.json",
            preferences,
            MAX_PREFERENCES_BYTES,
        )
    }

    pub(crate) fn load_sync_state(&self) -> Result<crate::sync::State> {
        Self::read(&self.active, SYNC_STATE, 16 * 1024)
    }

    pub(crate) fn save_sync_state(&self, state: &crate::sync::State) -> Result<()> {
        Self::write(&self.active, SYNC_STATE, state, 16 * 1024)
    }

    pub(crate) fn tun_dns(&self) -> Result<Option<std::net::IpAddr>> {
        Self::read(&self.root, TUN_DNS, 128)
    }

    pub(crate) fn save_tun_dns(&self, dns: std::net::IpAddr) -> Result<()> {
        Self::write(&self.root, TUN_DNS, &dns.to_canonical(), 128)
    }

    pub(crate) fn stage_tun_candidate(&self) -> Result<tempfile::TempDir> {
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        builder
            .prefix(TUN_CANDIDATE_PREFIX)
            .tempdir_in(&self.root)
            .map_err(|_| "无法创建私有 TUN 配置文件夹。".into())
    }

    fn write(directory: &Path, filename: &str, value: &impl Serialize, limit: u64) -> Result<()> {
        let failed = || format!("无法写入 {filename}。请检查磁盘空间和文件权限。");
        let mut temp = tempfile::NamedTempFile::new_in(directory).map_err(|_| failed())?;
        let mut buffer = std::io::BufWriter::new(temp.as_file_mut());
        serde_json::to_writer(&mut buffer, value).map_err(|_| failed())?;
        buffer.flush().map_err(|_| failed())?;
        drop(buffer);
        if temp.as_file().metadata().map_err(|_| failed())?.len() > limit {
            return Err(format!("{filename} 超过大小上限。请删除不使用的配置。"));
        }
        temp.as_file().sync_all().map_err(|_| failed())?;
        temp.persist(directory.join(filename))
            .map_err(|_| failed())?;
        Ok(())
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.active
    }

    pub(crate) fn relative_to_root<'a>(&self, parent: &'a Path) -> Result<Option<&'a Path>> {
        let failed = || "无法定位应用数据文件夹。".to_owned();
        let root = self.root.canonicalize().map_err(|_| failed())?;
        if let Ok(relative) = parent.strip_prefix(&root) {
            return Ok(Some(relative));
        }
        // A case-insensitive volume can resolve different spellings to the same directory.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let root = root.metadata().map_err(|_| failed())?;
            for ancestor in parent.ancestors() {
                if ancestor
                    .metadata()
                    .is_ok_and(|meta| meta.dev() == root.dev() && meta.ino() == root.ino())
                {
                    return Ok(parent.strip_prefix(ancestor).ok());
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            let root = same_file::Handle::from_path(root).map_err(|_| failed())?;
            for ancestor in parent.ancestors() {
                if same_file::Handle::from_path(ancestor).is_ok_and(|handle| handle == root) {
                    return Ok(parent.strip_prefix(ancestor).ok());
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn settings_export_path(&self, path: &Path) -> Result<PathBuf> {
        let filename = path.file_name().ok_or("请选择设置文件的保存位置。")?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map_err(|_| "无法打开目标文件夹。")?;
        if let Some(relative) = self.relative_to_root(&parent)?
            && let Some(first) = relative.iter().next().unwrap_or(filename).to_str()
        {
            let first = first.to_ascii_lowercase();
            if matches!(
                first.as_str(),
                "profiles.json"
                    | "preferences.json"
                    | "workspace.lock"
                    | "rule-sets"
                    | ACTIVE_WORKSPACE
                    | SYNC_STATE
                    | TUN_DNS
            ) || first.starts_with(GENERATION_PREFIX)
                || first.starts_with(TUN_CANDIDATE_PREFIX)
            {
                return Err("请选择应用管理文件之外的保存位置。".into());
            }
        }
        Ok(parent.join(filename))
    }

    pub(crate) fn stage(&self) -> Result<tempfile::TempDir> {
        tempfile::Builder::new()
            .prefix(GENERATION_PREFIX)
            .tempdir_in(&self.root)
            .map_err(|_| "无法创建待恢复的配置库。".into())
    }

    pub(crate) fn activate(&mut self, stage: tempfile::TempDir) -> Result<()> {
        let name = generation_name(
            stage
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        )?;
        if stage.path() != self.root.join(name) {
            return Err("待恢复的配置库位置无效。".into());
        }
        let failed = || "无法切换到已恢复的配置库。".to_owned();
        let mut marker = tempfile::NamedTempFile::new_in(&self.root).map_err(|_| failed())?;
        marker
            .write_all(name.as_bytes())
            .and_then(|_| marker.as_file().sync_all())
            .map_err(|_| failed())?;
        #[cfg(unix)]
        {
            let rule_sets = stage.path().join("rule-sets");
            if rule_sets.is_dir() {
                File::open(rule_sets)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|_| failed())?;
            }
            File::open(stage.path())
                .and_then(|dir| dir.sync_all())
                .map_err(|_| failed())?;
            File::open(&self.root)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| failed())?;
        }
        marker
            .persist(self.root.join(ACTIVE_WORKSPACE))
            .map_err(|_| failed())?;
        self.active = stage.keep();
        // The marker is already visible. Keep the published generation even if
        // a subsequent directory sync fails, so both possible markers are valid.
        #[cfg(unix)]
        let _ = File::open(&self.root).and_then(|dir| dir.sync_all());
        Ok(())
    }
}

fn generation_name(name: &str) -> Result<&str> {
    if !name.strip_prefix(GENERATION_PREFIX).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.len() <= 32
            && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
    }) {
        return Err("现用配置库记录无效。".into());
    }
    Ok(name)
}

pub fn default_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("EXODYS_DATA_DIR") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("EXODYS_DATA_DIR 必须是绝对路径。".into());
        }
        return Ok(path);
    }
    #[cfg(target_os = "windows")]
    {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or("找不到 Windows 用户应用数据目录（LOCALAPPDATA）。")?;
        Ok(local.join("Exodys"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var_os("HOME").ok_or("找不到用户目录。")?;
        Ok(PathBuf::from(home).join("Library/Application Support/Exodys"))
    }
}
