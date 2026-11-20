//! Serializes disk changes and engine transitions. The UI reads cheap immutable snapshots.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::Result;
use crate::catalog::{Catalog, MAX_PROFILES, Node, NodeRef, Preferences, Profile, Store};
use crate::{engine, import::Source};

#[derive(Clone, Default, PartialEq)]
pub struct Snapshot {
    pub catalog: Arc<Catalog>,
    pub preferences: Preferences,
    pub connected: bool,
    pub probe_time: Option<Duration>,
}

impl Snapshot {
    pub fn node(&self) -> Option<&Node> {
        let selected = self.preferences.selected.as_ref()?;
        self.catalog.profile(selected.profile)?.node(&selected.node)
    }
}

pub enum Job {
    Import(Source),
    Refresh(u64),
    Rename(u64, String),
    Remove(u64),
    Select(NodeRef),
    Connect,
    Disconnect,
    SetSystemProxy(bool),
    RecoverSystemProxy,
    SetPort(u16),
    SetRouting(String),
    ExportSettings(PathBuf),
    RestoreSettings(PathBuf),
    Probe(String),
}

#[derive(Default)]
pub struct Outcome {
    pub focus: Option<u64>,
    pub message: Option<String>,
}

impl Outcome {
    fn notice(message: impl Into<String>) -> Self {
        Self {
            focus: None,
            message: Some(message.into()),
        }
    }
}

pub struct Session {
    store: Store,
    snapshot: Snapshot,
    system_proxy: Option<crate::system_proxy::Lease>,
    proxy: Option<engine::Proxy>,
    engine_override: Option<PathBuf>,
}

impl Session {
    pub fn open(directory: PathBuf) -> Result<Self> {
        Self::open_with_engine(directory, None)
    }

    pub fn open_with_engine(directory: PathBuf, engine_override: Option<PathBuf>) -> Result<Self> {
        let store = Store::open(directory)?;
        let (catalog, preferences) = store.load()?;
        let mut snapshot = Snapshot {
            catalog: Arc::new(catalog),
            preferences,
            ..Snapshot::default()
        };
        repair_selection(&mut snapshot);
        Ok(Self {
            store,
            snapshot,
            system_proxy: None,
            proxy: None,
            engine_override,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    fn engine_binary(&self) -> Result<PathBuf> {
        self.engine_override
            .clone()
            .map_or_else(engine::engine_path, Ok)
    }

    pub fn run(&mut self, job: Job) -> Result<Outcome> {
        if let Some(error) = self.check_connection() {
            return Err(error);
        }
        match job {
            Job::Import(source) => self.import(source),
            Job::Refresh(id) => self.refresh(id),
            Job::Rename(id, name) => self.rename(id, name),
            Job::Remove(id) => self.remove(id),
            Job::Select(selection) => self.select(selection),
            Job::Connect => self.connect(),
            Job::Disconnect => {
                self.disconnect()?;
                Ok(Outcome::default())
            }
            Job::SetSystemProxy(enabled) => {
                if self.proxy.is_some() {
                    return Err("断开连接后可修改系统代理选项。".into());
                }
                let mut prefs = self.snapshot.preferences.clone();
                prefs.system_proxy = enabled;
                self.store.save_preferences(&prefs)?;
                self.snapshot.preferences = prefs;
                Ok(Outcome::notice(if enabled {
                    "下次连接时将启用 macOS 系统代理。"
                } else {
                    "下次连接仅开启本地代理。"
                }))
            }
            Job::RecoverSystemProxy => {
                if self.proxy.is_some() {
                    return Err("请先断开连接。".into());
                }
                crate::system_proxy::recover()?;
                Ok(Outcome::notice("已完成系统代理恢复检查。"))
            }
            Job::SetPort(port) => self.set_port(port),
            Job::SetRouting(rules) => {
                self.save_settings(self.snapshot.preferences.port, rules, "已保存分流规则。")
            }
            Job::ExportSettings(path) => {
                let parent = path
                    .parent()
                    .ok_or("请选择设置文件的保存位置。")?
                    .canonicalize()
                    .map_err(|_| "无法打开目标文件夹。")?;
                let workspace = self
                    .store
                    .directory()
                    .canonicalize()
                    .map_err(|_| "无法打开应用数据文件夹。")?;
                if parent == workspace
                    && matches!(
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_ascii_lowercase)
                            .as_deref(),
                        Some("preferences.json" | "profiles.json" | "workspace.lock")
                    )
                {
                    return Err("请选择其他文件名，以保留当前配置库。".into());
                }
                let prefs = &self.snapshot.preferences;
                crate::settings_file::save(&path, prefs.port, &prefs.routing_rules)?;
                Ok(Outcome::notice("已导出本地端口和分流规则。"))
            }
            Job::RestoreSettings(path) => {
                let (port, rules) = crate::settings_file::load(&path)?;
                self.save_settings(port, rules, "已恢复本地端口和分流规则。")
            }
            Job::Probe(url) => self.probe(&url),
        }
    }

    fn import(&mut self, source: Source) -> Result<Outcome> {
        let mut catalog = (*self.snapshot.catalog).clone();
        if catalog.profiles.len() >= MAX_PROFILES {
            return Err(format!(
                "最多可保存 {MAX_PROFILES} 个配置。请先删除不使用的配置。"
            ));
        }
        let imported = source.load()?;
        let id = catalog.new_id()?;
        let count = imported.nodes.len();
        let profile = Profile::new(id, imported.name, imported.subscription, imported.nodes);
        catalog.profiles.push(Arc::new(profile));
        self.replace_catalog(catalog)?;
        Ok(Outcome {
            focus: Some(id),
            message: Some(import_message("导入", count, imported.skipped)),
        })
    }

    fn refresh(&mut self, id: u64) -> Result<Outcome> {
        if self.active_profile() == Some(id) {
            return Err("请先断开使用此配置的连接，再更新订阅。".into());
        }
        let mut catalog = (*self.snapshot.catalog).clone();
        let profile = profile_slot(&mut catalog, id)?;
        let link = profile.subscription.clone().ok_or("此配置没有订阅地址。")?;
        let imported = Source::Subscription(link).load()?;
        let count = imported.nodes.len();
        *profile = Arc::new(Profile::new(
            id,
            profile.name.clone(),
            imported.subscription,
            imported.nodes,
        ));
        self.replace_catalog(catalog)?;
        Ok(Outcome::notice(import_message(
            "更新",
            count,
            imported.skipped,
        )))
    }

    fn rename(&mut self, id: u64, name: String) -> Result<Outcome> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
            return Err("请输入 1 到 120 个字符的配置名称。".into());
        }
        let mut catalog = (*self.snapshot.catalog).clone();
        let profile = profile_slot(&mut catalog, id)?;
        *profile = Arc::new(Profile::new(
            id,
            name.to_owned(),
            profile.subscription.clone(),
            profile.nodes.clone(),
        ));
        self.replace_catalog(catalog)?;
        Ok(Outcome::notice("已重命名配置。"))
    }

    fn remove(&mut self, id: u64) -> Result<Outcome> {
        if self.active_profile() == Some(id) {
            return Err("请先断开使用此配置的连接，再删除它。".into());
        }
        let mut catalog = (*self.snapshot.catalog).clone();
        catalog.profiles.retain(|profile| profile.id != id);
        self.replace_catalog(catalog)?;
        Ok(Outcome::notice("已删除配置。"))
    }

    fn replace_catalog(&mut self, catalog: Catalog) -> Result<()> {
        self.store.save_catalog(&catalog)?;
        self.snapshot.catalog = Arc::new(catalog);
        repair_selection(&mut self.snapshot);
        // A stale preference is repaired from the new catalogue after a restart as well.
        Ok(())
    }

    fn active_profile(&self) -> Option<u64> {
        self.proxy.as_ref()?;
        self.snapshot
            .preferences
            .selected
            .as_ref()
            .map(|selected| selected.profile)
    }

    fn select(&mut self, selected: NodeRef) -> Result<Outcome> {
        if self.snapshot.preferences.selected.as_ref() == Some(&selected) {
            return Ok(Outcome::default());
        }
        let node = self
            .snapshot
            .catalog
            .profile(selected.profile)
            .and_then(|profile| profile.node(&selected.node))
            .ok_or("此节点已不存在。")?;
        let preferences = Preferences {
            selected: Some(selected),
            ..self.snapshot.preferences.clone()
        };
        if self.proxy.is_none() {
            self.store.save_preferences(&preferences)?;
            self.snapshot.preferences = preferences;
            return Ok(Outcome::default());
        }

        let binary = self.engine_binary()?;
        let directory = self.store.directory();
        let port = preferences.port;
        let rules = &preferences.routing_rules;
        let next = engine::prepare(&binary, directory, node, port, rules)?;
        let prior_node = self.snapshot.node().ok_or("当前节点已不存在。")?;
        let previous = engine::prepare(&binary, directory, prior_node, port, rules)?;
        self.proxy = None;
        self.snapshot.probe_time = None;
        match next.start() {
            Ok(proxy) => match self.store.save_preferences(&preferences) {
                Ok(()) => {
                    self.proxy = Some(proxy);
                    self.snapshot.preferences = preferences;
                    Ok(Outcome::notice("已切换节点。"))
                }
                Err(reason) => {
                    drop(proxy);
                    self.rollback(previous, reason)
                }
            },
            Err(reason) => self.rollback(previous, reason),
        }
    }

    fn rollback(&mut self, previous: engine::Prepared, reason: String) -> Result<Outcome> {
        self.proxy = previous.start().ok();
        self.snapshot.connected = self.proxy.is_some();
        let status = if self.snapshot.connected {
            "已恢复上一条连接。"
        } else {
            if let Err(restore) = self.disconnect() {
                return Err(format!("切换失败。{reason}{restore}"));
            }
            "上一条连接也已断开。"
        };
        Err(format!("切换失败。{reason}{status}"))
    }

    fn connect(&mut self) -> Result<Outcome> {
        if self.proxy.is_some() {
            return Ok(Outcome::default());
        }
        let node = self.snapshot.node().ok_or("请先导入配置并选择节点。")?;
        let port = self.snapshot.preferences.port;
        let prepared = engine::prepare(
            &self.engine_binary()?,
            self.store.directory(),
            node,
            port,
            &self.snapshot.preferences.routing_rules,
        )?;
        self.store.save_preferences(&self.snapshot.preferences)?;
        let proxy = prepared.start()?;
        if self.snapshot.preferences.system_proxy {
            self.system_proxy = Some(crate::system_proxy::Lease::acquire(port)?);
        }
        self.proxy = Some(proxy);
        self.snapshot.connected = true;
        self.snapshot.probe_time = None;
        Ok(Outcome::default())
    }

    /// The window is going away. Signal the helper without waiting for a
    /// possible macOS authorization dialog on AppKit's main thread.
    #[cfg(target_os = "macos")]
    pub(crate) fn close_window(&mut self) {
        self.system_proxy.take();
        self.proxy.take();
        self.snapshot.connected = false;
        self.snapshot.probe_time = None;
    }

    pub fn disconnect(&mut self) -> Result<()> {
        let restored = self
            .system_proxy
            .take()
            .map_or(Ok(()), |lease| lease.release());
        self.proxy = None;
        self.snapshot.connected = false;
        self.snapshot.probe_time = None;
        restored
    }

    pub(crate) fn connection_issue(&mut self) -> Option<&'static str> {
        if self
            .system_proxy
            .as_mut()
            .is_some_and(|lease| !lease.alive())
        {
            Some("系统代理守护进程已退出。请检查或恢复系统代理设置。")
        } else if self.proxy.as_mut().is_some_and(|proxy| !proxy.alive()) {
            Some("sing-box 已退出。本地代理已断开，请重新连接。")
        } else {
            None
        }
    }

    fn check_connection(&mut self) -> Option<String> {
        let reason = self.connection_issue()?;
        Some(self.disconnect().err().unwrap_or_else(|| reason.into()))
    }

    fn set_port(&mut self, port: u16) -> Result<Outcome> {
        self.save_settings(
            port,
            self.snapshot.preferences.routing_rules.clone(),
            "已保存本地端口。",
        )
    }

    fn save_settings(
        &mut self,
        port: u16,
        routing_rules: String,
        message: &str,
    ) -> Result<Outcome> {
        if self.proxy.is_some() {
            return Err("断开连接后可修改本地端口和分流规则。".into());
        }
        if port < 1024 {
            return Err("请输入 1024 至 65535 之间的端口。".into());
        }
        crate::routing::parse(&routing_rules)?;
        let preferences = Preferences {
            port,
            routing_rules,
            ..self.snapshot.preferences.clone()
        };
        self.store.save_preferences(&preferences)?;
        self.snapshot.preferences = preferences;
        Ok(Outcome::notice(message))
    }

    fn probe(&mut self, url: &str) -> Result<Outcome> {
        self.snapshot.probe_time = None;
        let proxy = self.proxy.as_ref().ok_or("连接后即可测试代理。")?;
        let elapsed = proxy.probe(url)?;
        self.snapshot.probe_time = Some(elapsed);
        Ok(Outcome::notice(format!(
            "代理请求成功，耗时 {} 毫秒。",
            elapsed.as_millis()
        )))
    }

    pub fn data_directory(&self) -> &Path {
        self.store.directory()
    }
}

fn repair_selection(snapshot: &mut Snapshot) {
    if snapshot.node().is_none() {
        snapshot.preferences.selected = snapshot.catalog.profiles.first().and_then(|profile| {
            profile.nodes.first().map(|node| NodeRef {
                profile: profile.id,
                node: node.id.clone(),
            })
        });
    }
}

fn profile_slot(catalog: &mut Catalog, id: u64) -> Result<&mut Arc<Profile>> {
    catalog
        .profiles
        .iter_mut()
        .find(|profile| profile.id == id)
        .ok_or_else(|| "此配置已不存在。".into())
}

fn import_message(action: &str, count: usize, skipped: usize) -> String {
    if skipped == 0 {
        format!("已{action} {count} 个节点。")
    } else {
        format!("已{action} {count} 个节点，跳过 {skipped} 个不适用或无效的条目。")
    }
}
