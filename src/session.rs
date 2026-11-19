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
    SetPort(u16),
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
        self.check_connection();
        match job {
            Job::Import(source) => self.import(source),
            Job::Refresh(id) => self.refresh(id),
            Job::Rename(id, name) => self.rename(id, name),
            Job::Remove(id) => self.remove(id),
            Job::Select(selection) => self.select(selection),
            Job::Connect => self.connect(),
            Job::Disconnect => {
                self.disconnect();
                Ok(Outcome::default())
            }
            Job::SetPort(port) => self.set_port(port),
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
        let next = engine::prepare(&binary, directory, node, port)?;
        let prior_node = self.snapshot.node().ok_or("当前节点已不存在。")?;
        let previous = engine::prepare(&binary, directory, prior_node, port)?;
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
        let prepared = engine::prepare(&self.engine_binary()?, self.store.directory(), node, port)?;
        self.store.save_preferences(&self.snapshot.preferences)?;
        self.proxy = Some(prepared.start()?);
        self.snapshot.connected = true;
        self.snapshot.probe_time = None;
        Ok(Outcome::default())
    }

    pub fn disconnect(&mut self) {
        self.proxy = None;
        self.snapshot.connected = false;
        self.snapshot.probe_time = None;
    }

    pub fn check_connection(&mut self) -> bool {
        if self.proxy.as_mut().is_some_and(|proxy| !proxy.alive()) {
            self.disconnect();
            return true;
        }
        false
    }

    fn set_port(&mut self, port: u16) -> Result<Outcome> {
        if self.proxy.is_some() {
            return Err("断开连接后即可更换本地端口。".into());
        }
        if port < 1024 {
            return Err("请输入 1024 至 65535 之间的端口。".into());
        }
        let preferences = Preferences {
            port,
            ..self.snapshot.preferences.clone()
        };
        self.store.save_preferences(&preferences)?;
        self.snapshot.preferences = preferences;
        Ok(Outcome::notice("已保存本地端口。"))
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
