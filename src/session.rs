//! Serializes disk changes and engine transitions. The UI reads cheap immutable snapshots.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use crate::sync::Conflict as SyncConflict;

const UPDATE_BACKOFF: Duration = Duration::from_secs(60 * 60);
const RULE_SET_REFRESH_SECONDS: u64 = 24 * 60 * 60;

#[derive(Clone, Default, PartialEq)]
pub struct SyncView {
    pub directory: Option<PathBuf>,
    pub unlocked: bool,
    pub conflict: Option<SyncConflict>,
    pub problem: Option<String>,
}

enum RefreshTarget {
    Subscription(u64),
    RuleSet(String),
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

use crate::Result;
use crate::catalog::{
    Catalog, MAX_AUTO_MEMBERS, MAX_POLICY_GROUPS, MAX_PROFILES, Node, NodeRef, PolicyGroup,
    PolicyMode, Preferences, Profile, Store, valid_policy_name,
};
use crate::{backup, engine, import::Source, rule_sets, sync};
use zeroize::Zeroizing;

#[derive(Clone, Default, PartialEq)]
pub struct Snapshot {
    pub catalog: Arc<Catalog>,
    pub preferences: Preferences,
    pub rule_sets: Arc<Vec<String>>,
    pub connected: bool,
    pub tun_active: bool,
    pub probe_time: Option<Duration>,
    pub sync: SyncView,
    policy_profiles: Arc<HashSet<u64>>,
}

impl Snapshot {
    /// Manual selection remains available for browsing even in automatic mode.
    pub fn profile_in_use(&self, id: u64) -> bool {
        self.connected
            && (self.policy_profiles.contains(&id)
                || if self.preferences.auto_enabled {
                    self.preferences
                        .auto_members
                        .iter()
                        .any(|member| member.profile == id)
                } else {
                    self.preferences
                        .selected
                        .as_ref()
                        .is_some_and(|node| node.profile == id)
                })
    }

    pub fn node(&self) -> Option<&Node> {
        let selected = self.preferences.selected.as_ref()?;
        self.catalog.profile(selected.profile)?.node(&selected.node)
    }
}

pub enum Job {
    Import(Source),
    Refresh(u64),
    RefreshDue,
    SetScheduledRefresh(bool),
    SetRefreshHours(u64, u16),
    Rename(u64, String),
    Remove(u64),
    Select(NodeRef),
    Connect,
    ConnectTun,
    Disconnect,
    SetAutoMember(NodeRef, bool),
    SetAutoEnabled(bool),
    CreatePolicy(String),
    SetPolicyMember(String, NodeRef, bool),
    SetPolicyMode(String, PolicyMode),
    RemovePolicy(String),
    SetSystemProxy(bool),
    RecoverSystemProxy,
    SetPort(u16),
    SetRouting(String),
    ImportRuleSet(String, rule_sets::Source),
    ReplaceRuleSet(String, rule_sets::Source),
    RemoveRuleSet(String),
    TrackRuleSet(String, Option<String>),
    RefreshRuleSet(String),
    ExportSettings(PathBuf),
    RestoreSettings(PathBuf),
    ExportBackup(PathBuf, zeroize::Zeroizing<String>),
    RestoreBackup(PathBuf, zeroize::Zeroizing<String>),
    StartSync(PathBuf, Zeroizing<String>),
    PauseSync,
    SyncTick,
    ResolveSync {
        conflict: SyncConflict,
        keep_local: bool,
    },
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
    tun: Option<crate::tun::Lease>,
    engine_override: Option<PathBuf>,
    refresh_retry: HashMap<u64, Instant>,
    rule_set_retry: HashMap<String, Instant>,
    sync_state: sync::State,
    sync_folder: Option<sync::Folder>,
}

impl Session {
    pub fn open(directory: PathBuf) -> Result<Self> {
        Self::open_with_engine(directory, None)
    }

    pub fn open_with_engine(directory: PathBuf, engine_override: Option<PathBuf>) -> Result<Self> {
        let store = Store::open(directory)?;
        let (catalog, preferences) = store.load()?;
        let (sync_state, problem) = match store.load_sync_state() {
            Ok(state) => (state, None),
            Err(error) => (sync::State::default(), Some(error)),
        };
        let mut snapshot = Snapshot {
            catalog: Arc::new(catalog),
            preferences,
            rule_sets: Arc::new(rule_sets::list(store.directory())?),
            sync: SyncView {
                directory: sync_state.directory.clone(),
                problem,
                ..SyncView::default()
            },
            ..Snapshot::default()
        };
        #[cfg(target_os = "windows")]
        let cleared_system_proxy = std::mem::take(&mut snapshot.preferences.system_proxy);
        #[cfg(not(target_os = "windows"))]
        let cleared_system_proxy = false;
        let prior_sources = snapshot.preferences.rule_set_updates.len();
        snapshot
            .preferences
            .rule_set_updates
            .retain(|name, _| snapshot.rule_sets.contains(name));
        if repair_selection(&mut snapshot)
            || cleared_system_proxy
            || snapshot.preferences.rule_set_updates.len() != prior_sources
        {
            store.save_preferences(&snapshot.preferences)?;
        }
        Ok(Self {
            store,
            snapshot,
            system_proxy: None,
            proxy: None,
            tun: None,
            engine_override,
            refresh_retry: HashMap::new(),
            rule_set_retry: HashMap::new(),
            sync_state,
            sync_folder: None,
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
            Job::RefreshDue => match self.next_refresh() {
                Some(RefreshTarget::Subscription(id)) => self.refresh(id),
                Some(RefreshTarget::RuleSet(name)) => self.refresh_rule_set(&name),
                None => Ok(Outcome::default()),
            },
            Job::SetScheduledRefresh(enabled) => {
                let prefs = Preferences {
                    scheduled_refresh: enabled,
                    ..self.snapshot.preferences.clone()
                };
                self.store.save_preferences(&prefs)?;
                self.snapshot.preferences = prefs;
                Ok(Outcome::notice(if enabled {
                    "已启用定时订阅更新。"
                } else {
                    "已关闭定时订阅更新。"
                }))
            }
            Job::SetRefreshHours(id, hours) => {
                if hours > crate::catalog::MAX_REFRESH_HOURS {
                    return Err("更新周期须为 0 至 720 小时。".into());
                }
                let mut catalog = (*self.snapshot.catalog).clone();
                let profile = profile_slot(&mut catalog, id)?;
                if profile.subscription.is_none() {
                    return Err("此配置没有订阅地址。".into());
                }
                Arc::make_mut(profile).refresh_hours = Some(hours);
                self.replace_catalog(catalog)?;
                Ok(Outcome::notice("已保存订阅更新周期。"))
            }
            Job::Rename(id, name) => self.rename(id, name),
            Job::Remove(id) => self.remove(id),
            Job::Select(selection) => self.select(selection),
            Job::Connect => self.connect(),
            Job::ConnectTun => self.connect_tun(),
            Job::Disconnect => {
                self.disconnect()?;
                Ok(Outcome::default())
            }
            Job::SetSystemProxy(enabled) => {
                if enabled && !cfg!(target_os = "macos") {
                    return Err("系统代理仅支持 macOS。".into());
                }
                if self.snapshot.connected {
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
            Job::SetAutoMember(member, add) => self.edit_group(Some((member, add)), None),
            Job::SetAutoEnabled(enabled) => self.edit_group(None, Some(enabled)),
            Job::CreatePolicy(name) => {
                if !valid_policy_name(&name) {
                    return Err("策略组名称使用 1–32 个小写字母、数字或连字符。".into());
                }
                self.edit_policies(|groups| {
                    if groups.iter().any(|group| group.name == name) {
                        return Err("同名策略组已存在。".into());
                    }
                    if groups.len() >= MAX_POLICY_GROUPS {
                        return Err(format!("最多可保存 {MAX_POLICY_GROUPS} 个策略组。"));
                    }
                    groups.push(PolicyGroup { name, members: Vec::new(), mode: PolicyMode::Manual(None) });
                    Ok(())
                })
            }
            Job::SetPolicyMember(name, member, add) => {
                if add && self.snapshot.catalog.profile(member.profile).and_then(|p| p.node(&member.node)).is_none() {
                    return Err("此节点已不存在。".into());
                }
                self.edit_policies(|groups| {
                    let group = policy_mut(groups, &name)?;
                    if add {
                        if !group.members.contains(&member) {
                            if group.members.len() >= MAX_AUTO_MEMBERS {
                                return Err(format!("策略组最多可加入 {MAX_AUTO_MEMBERS} 个节点。"));
                            }
                            group.members.push(member);
                        }
                    } else {
                        group.retain_members(|candidate| candidate != &member);
                    }
                    Ok(())
                })
            }
            Job::SetPolicyMode(name, mode) => self.edit_policies(|groups| {
                let group = policy_mut(groups, &name)?;
                if group.members.is_empty() || matches!(&mode, PolicyMode::Manual(None))
                    || matches!(&mode, PolicyMode::Manual(Some(member)) if !group.members.contains(member))
                {
                    return Err("请先加入成员，再选择手动节点或启用自动选择。".into());
                }
                group.mode = mode;
                Ok(())
            }),
            Job::RemovePolicy(name) => {
                let rules = crate::routing::parse(&self.snapshot.preferences.routing_rules)?;
                if crate::routing::group_names(&rules).contains(name.as_str()) {
                    return Err(format!("分流规则仍引用策略组「{name}」。请先移除引用并保存规则。"));
                }
                self.edit_policies(|groups| {
                    policy_mut(groups, &name)?;
                    groups.retain(|group| group.name != name);
                    Ok(())
                })
            }
            Job::RecoverSystemProxy => {
                if !cfg!(target_os = "macos") {
                    return Err("系统代理仅支持 macOS。".into());
                }
                if self.snapshot.connected {
                    return Err("请先断开连接。".into());
                }
                crate::system_proxy::recover()?;
                Ok(Outcome::notice("已完成系统代理恢复检查。"))
            }
            Job::SetPort(port) => self.set_port(port),
            Job::SetRouting(rules) => {
                self.save_settings(self.snapshot.preferences.port, rules, "已保存分流规则。")
            }
            Job::ImportRuleSet(name, source) => self.install_rule_set(name, source, false),
            Job::ReplaceRuleSet(name, source) => self.install_rule_set(name, source, true),
            Job::RemoveRuleSet(name) => self.remove_rule_set(&name),
            Job::TrackRuleSet(name, url) => self.track_rule_set(&name, url.as_deref()),
            Job::RefreshRuleSet(name) => self.refresh_rule_set(&name),
            Job::ExportSettings(path) => {
                let destination = self.store.settings_export_path(&path)?;
                let prefs = &self.snapshot.preferences;
                crate::settings_file::save(&destination, prefs.port, &prefs.routing_rules)?;
                Ok(Outcome::notice("已导出本地端口和分流规则。"))
            }
            Job::RestoreSettings(path) => {
                let (port, rules) = crate::settings_file::load(&path)?;
                self.save_settings(port, rules, "已恢复本地端口和分流规则。")
            }
            Job::ExportBackup(path, passphrase) => {
                crate::backup::save(&path, &passphrase, &self.store, &self.snapshot.catalog, &self.snapshot.preferences)?;
                Ok(Outcome::notice("已保存完整加密备份。"))
            }
            Job::RestoreBackup(path, passphrase) => self.restore_backup(&path, &passphrase),
            Job::StartSync(path, passphrase) => self.start_sync(&path, passphrase),
            Job::PauseSync => {
                self.sync_folder = None;
                self.snapshot.sync.conflict = None;
                self.snapshot.sync.unlocked = false;
                self.snapshot.sync.problem = None;
                Ok(Outcome::notice("已暂停自动同步。"))
            }
            Job::SyncTick => self.sync_tick(None),
            Job::ResolveSync {
                conflict,
                keep_local,
            } => {
                if self.snapshot.sync.conflict.as_ref() != Some(&conflict) {
                    return Err("同步冲突已变化。请重新选择要保留的版本。".into());
                }
                self.sync_tick(Some((&conflict, keep_local)))
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
        let mut profile = Profile::new(id, imported.name, imported.subscription, imported.nodes);
        profile.last_subscription_update = profile.subscription.as_ref().map(|_| unix_now());
        catalog.profiles.push(Arc::new(profile));
        self.replace_catalog(catalog)?;
        Ok(Outcome {
            focus: Some(id),
            message: Some(import_message("导入", count, imported.skipped)),
        })
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn has_due_refresh(&self) -> bool {
        self.next_refresh().is_some()
    }

    fn next_refresh(&self) -> Option<RefreshTarget> {
        let now = unix_now();
        let monotonic_now = Instant::now();
        let subscription = self.next_due_subscription(now, monotonic_now);
        let rule_set = if !self.snapshot.connected {
            self.snapshot
                .preferences
                .rule_set_updates
                .iter()
                .filter(|(name, _)| {
                    self.snapshot.rule_sets.contains(name)
                        && self
                            .rule_set_retry
                            .get(name.as_str())
                            .is_none_or(|&retry| retry <= monotonic_now)
                })
                .filter_map(|(name, remote)| {
                    let due = due_at(remote.last_success, now, RULE_SET_REFRESH_SECONDS);
                    (due <= now).then_some((due, name))
                })
                .min()
        } else {
            None
        };
        if let Some((due, name)) = rule_set
            && subscription.is_none_or(|(subscription_due, _)| due <= subscription_due)
        {
            return Some(RefreshTarget::RuleSet(name.clone()));
        }
        subscription.map(|(_, id)| RefreshTarget::Subscription(id))
    }

    fn next_due_subscription(&self, now: u64, monotonic_now: Instant) -> Option<(u64, u64)> {
        if !self.snapshot.preferences.scheduled_refresh {
            return None;
        }
        self.snapshot
            .catalog
            .profiles
            .iter()
            .filter(|p| {
                p.subscription.is_some()
                    && !self.snapshot.profile_in_use(p.id)
                    && self
                        .refresh_retry
                        .get(&p.id)
                        .is_none_or(|&retry| retry <= monotonic_now)
            })
            .filter_map(|profile| {
                let hours = profile
                    .refresh_hours
                    .unwrap_or(crate::catalog::DEFAULT_REFRESH_HOURS);
                if hours == 0 {
                    return None;
                }
                let due = due_at(
                    profile.last_subscription_update,
                    now,
                    u64::from(hours) * 3600,
                );
                (due <= now).then_some((due, profile.id))
            })
            .min()
    }

    fn refresh(&mut self, id: u64) -> Result<Outcome> {
        let result = self.refresh_profile(id);
        if result.is_ok() {
            self.refresh_retry.remove(&id);
        } else if self
            .snapshot
            .catalog
            .profile(id)
            .is_some_and(|profile| profile.subscription.is_some())
            && !self.snapshot.profile_in_use(id)
        {
            self.refresh_retry
                .insert(id, Instant::now() + UPDATE_BACKOFF);
        }
        result
    }

    fn refresh_profile(&mut self, id: u64) -> Result<Outcome> {
        if self.snapshot.profile_in_use(id) {
            return Err("请先断开使用此配置的连接，再更新订阅。".into());
        }
        let mut catalog = (*self.snapshot.catalog).clone();
        let profile = profile_slot(&mut catalog, id)?;
        let link = profile.subscription.clone().ok_or("此配置没有订阅地址。")?;
        let imported = Source::Subscription(link).load()?;
        let count = imported.nodes.len();
        let mut next = Profile::new(
            id,
            profile.name.clone(),
            imported.subscription,
            imported.nodes,
        );
        next.last_subscription_update = Some(unix_now());
        next.refresh_hours = profile.refresh_hours;
        // IDs derived from display names can be reused by a replacement. Revoke
        // membership before publishing any changed outbound at the old ID.
        let mut prefs = self.snapshot.preferences.clone();
        let unchanged =
            |m: &NodeRef| m.profile != id || profile.node(&m.node) == next.node(&m.node);
        let prior_count = prefs.auto_members.len();
        prefs.auto_members.retain(unchanged);
        let removed = prior_count - prefs.auto_members.len()
            + prefs
                .policy_groups
                .iter_mut()
                .map(|group| group.retain_members(unchanged))
                .sum::<usize>();
        if removed > 0 {
            self.store.save_preferences(&prefs)?;
            self.snapshot.preferences = prefs;
        }
        *profile = Arc::new(next);
        self.replace_catalog(catalog)?;
        let mut message = import_message("更新", count, imported.skipped);
        if removed > 0 {
            message.push_str(&format!(
                "自动组或策略组中有 {removed} 项成员已变更或删除，已移出。"
            ));
        }
        Ok(Outcome::notice(message))
    }

    fn rename(&mut self, id: u64, name: String) -> Result<Outcome> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
            return Err("请输入 1 到 120 个字符的配置名称。".into());
        }
        let mut catalog = (*self.snapshot.catalog).clone();
        let profile = profile_slot(&mut catalog, id)?;
        Arc::make_mut(profile).name = name.to_owned();
        self.replace_catalog(catalog)?;
        Ok(Outcome::notice("已重命名配置。"))
    }

    fn remove(&mut self, id: u64) -> Result<Outcome> {
        if self.snapshot.profile_in_use(id) {
            return Err("请先断开使用此配置的连接，再删除它。".into());
        }
        let mut catalog = (*self.snapshot.catalog).clone();
        catalog.profiles.retain(|profile| profile.id != id);
        self.replace_catalog(catalog)?;
        self.refresh_retry.remove(&id);
        Ok(Outcome::notice("已删除配置。"))
    }

    fn replace_catalog(&mut self, catalog: Catalog) -> Result<()> {
        self.store.save_catalog(&catalog)?;
        self.snapshot.catalog = Arc::new(catalog);
        if repair_selection(&mut self.snapshot) {
            self.store.save_preferences(&self.snapshot.preferences)?;
        }
        // If interrupted between the two atomic writes, open repairs from the new catalog.
        Ok(())
    }

    fn edit_group(
        &mut self,
        member: Option<(NodeRef, bool)>,
        enabled: Option<bool>,
    ) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("断开连接后可修改自动组。".into());
        }
        let mut prefs = self.snapshot.preferences.clone();
        if let Some((member, add)) = member {
            if add {
                if self
                    .snapshot
                    .catalog
                    .profile(member.profile)
                    .and_then(|p| p.node(&member.node))
                    .is_none()
                {
                    return Err("此节点已不存在。".into());
                }
                if !prefs.auto_members.contains(&member) {
                    if prefs.auto_members.len() >= crate::catalog::MAX_AUTO_MEMBERS {
                        return Err("自动组最多可加入 32 个节点。".into());
                    }
                    prefs.auto_members.push(member);
                }
            } else {
                prefs.auto_members.retain(|m| m != &member);
            }
        }
        if let Some(enabled) = enabled {
            prefs.auto_enabled = enabled;
        }
        if prefs.auto_enabled && prefs.auto_members.is_empty() {
            return Err("自动模式至少需要一个节点。请先添加节点或关闭自动模式。".into());
        }
        self.store.save_preferences(&prefs)?;
        self.snapshot.preferences = prefs;
        Ok(Outcome::default())
    }

    fn select(&mut self, selected: NodeRef) -> Result<Outcome> {
        if self.snapshot.tun_active {
            return Err("请先断开 TUN，再切换节点。".into());
        }
        if self.proxy.is_some() && self.snapshot.preferences.auto_enabled {
            return Err("自动组连接中不能手动切换节点。".into());
        }
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

        let (next, _) = self.prepare_connection(Some(node))?;
        let (previous, _) = self.prepare_connection(None)?;
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
        if self.snapshot.connected {
            return Ok(Outcome::default());
        }
        let port = self.snapshot.preferences.port;
        let (prepared, policy_profiles) = self.prepare_connection(None)?;
        self.store.save_preferences(&self.snapshot.preferences)?;
        let proxy = prepared.start()?;
        if self.snapshot.preferences.system_proxy {
            self.system_proxy = Some(crate::system_proxy::Lease::acquire(port)?);
        }
        self.proxy = Some(proxy);
        self.snapshot.connected = true;
        self.snapshot.policy_profiles = Arc::new(policy_profiles);
        self.snapshot.probe_time = None;
        Ok(Outcome::default())
    }

    fn connect_tun(&mut self) -> Result<Outcome> {
        if !cfg!(target_os = "macos") {
            return Err("TUN 当前仅支持 macOS。".into());
        }
        if self.snapshot.connected {
            return Err("请先断开当前连接，再启动 TUN。".into());
        }
        let dns = self
            .store
            .tun_dns()?
            .ok_or("请先使用 --prepare-tun-config 设置 TUN DNS，并请管理员批准当前配置。")?;
        let (policy, active) = self.connection_policy(None)?;
        let id = engine::tun_candidate(self.store.directory(), dns, policy, None)?;
        self.tun = Some(crate::tun::Lease::acquire(&id)?);
        self.snapshot.connected = true;
        self.snapshot.tun_active = true;
        self.snapshot.policy_profiles = Arc::new(active);
        self.snapshot.probe_time = None;
        Ok(Outcome::notice("已使用当前配置连接 TUN。"))
    }

    fn prepare_connection(
        &self,
        manual: Option<&Node>,
    ) -> Result<(engine::Prepared, HashSet<u64>)> {
        let (policy, active) = self.connection_policy(manual)?;
        let prepared = engine::prepare_connection(
            &self.engine_binary()?,
            self.store.directory(),
            self.snapshot.preferences.port,
            policy,
        )?;
        Ok((prepared, active))
    }

    pub fn check_tun_config(&self, dns: std::net::IpAddr) -> Result<()> {
        self.stage_tun_config(dns)?;
        Ok(())
    }

    pub fn prepare_tun_config(&self, dns: std::net::IpAddr) -> Result<(PathBuf, String)> {
        let (candidate, id) = self.stage_tun_config(dns)?;
        self.store.save_tun_dns(dns)?;
        Ok((candidate.keep(), id))
    }

    fn stage_tun_config(&self, dns: std::net::IpAddr) -> Result<(tempfile::TempDir, String)> {
        let (policy, _) = self.connection_policy(None)?;
        let candidate = self.store.stage_tun_candidate()?;
        let id =
            engine::tun_candidate(self.store.directory(), dns, policy, Some(candidate.path()))?;
        engine::check_tun_candidate(&self.engine_binary()?, candidate.path())?;
        Ok((candidate, id))
    }

    fn connection_policy(&self, manual: Option<&Node>) -> Result<(engine::Policy, HashSet<u64>)> {
        use engine::Selection;

        let prefs = &self.snapshot.preferences;
        let rules = crate::routing::parse(&prefs.routing_rules)?;
        let resolve = |member: &NodeRef| {
            self.snapshot
                .catalog
                .profile(member.profile)
                .and_then(|profile| profile.node(&member.node))
                .ok_or("组内节点已不存在。".to_owned())
        };
        let selected = if prefs.auto_enabled {
            if prefs.auto_members.is_empty() {
                return Err("自动组中没有可用节点，请先添加节点。".into());
            }
            Selection::Automatic(
                prefs
                    .auto_members
                    .iter()
                    .map(resolve)
                    .collect::<Result<_>>()?,
            )
        } else {
            Selection::Manual(
                manual
                    .or_else(|| self.snapshot.node())
                    .ok_or("请先导入配置并选择节点。")?,
            )
        };
        let mut active = HashSet::new();
        let groups = referenced_policies(prefs, &rules)?
            .into_iter()
            .map(|group| {
                let mode = match &group.mode {
                    PolicyMode::Automatic if !group.members.is_empty() => {
                        active.extend(group.members.iter().map(|member| member.profile));
                        Selection::Automatic(
                            group.members.iter().map(resolve).collect::<Result<_>>()?,
                        )
                    }
                    PolicyMode::Manual(Some(member)) => {
                        active.insert(member.profile);
                        Selection::Manual(resolve(member)?)
                    }
                    _ => {
                        return Err(format!(
                            "策略组「{}」没有可用出口。请添加成员并选择出口。",
                            group.name
                        ));
                    }
                };
                Ok((group.name.as_str(), mode))
            })
            .collect::<Result<_>>()?;
        Ok((engine::connection_policy(rules, selected, groups)?, active))
    }

    fn edit_policies(
        &mut self,
        edit: impl FnOnce(&mut Vec<PolicyGroup>) -> Result<()>,
    ) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("断开连接后可修改策略组。".into());
        }
        let mut prefs = self.snapshot.preferences.clone();
        edit(&mut prefs.policy_groups)?;
        self.store.save_preferences(&prefs)?;
        self.snapshot.preferences = prefs;
        Ok(Outcome::default())
    }

    /// The window is going away. Signal the helper without waiting for a
    /// possible macOS authorization dialog on AppKit's main thread.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) fn close_window(&mut self) {
        self.system_proxy.take();
        self.proxy.take();
        self.tun.take();
        self.snapshot.connected = false;
        self.snapshot.tun_active = false;
        self.snapshot.policy_profiles = Arc::default();
        self.snapshot.probe_time = None;
    }

    pub fn disconnect(&mut self) -> Result<()> {
        let restored = self
            .system_proxy
            .take()
            .map_or(Ok(()), |lease| lease.release());
        let stopped = self.tun.take().map_or(Ok(()), |lease| lease.release());
        self.proxy = None;
        self.snapshot.connected = false;
        self.snapshot.tun_active = false;
        self.snapshot.policy_profiles = Arc::default();
        self.snapshot.probe_time = None;
        restored.and(stopped)
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
        } else if self.tun.as_mut().is_some_and(|lease| !lease.alive()) {
            Some("TUN 服务已退出。请检查服务后重新连接。")
        } else {
            None
        }
    }

    fn check_connection(&mut self) -> Option<String> {
        let reason = self.connection_issue()?;
        self.tun.take();
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
        if self.snapshot.connected {
            return Err("断开连接后可修改本地端口和分流规则。".into());
        }
        if port < 1024 {
            return Err("请输入 1024 至 65535 之间的端口。".into());
        }
        let rules = crate::routing::parse(&routing_rules)?;
        rule_sets::definitions(self.store.directory(), &rules)?;
        referenced_policies(&self.snapshot.preferences, &rules)?;
        let preferences = Preferences {
            port,
            routing_rules,
            ..self.snapshot.preferences.clone()
        };
        self.store.save_preferences(&preferences)?;
        self.snapshot.preferences = preferences;
        Ok(Outcome::notice(message))
    }

    fn install_rule_set(
        &mut self,
        name: String,
        source: rule_sets::Source,
        replace: bool,
    ) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("断开连接后可修改规则集。".into());
        }
        if self
            .snapshot
            .preferences
            .rule_set_updates
            .contains_key(&name)
        {
            return Err(
                "此规则集已有定时来源。请点「立即更新」，或停止定时更新后更换文件。".into(),
            );
        }
        let binary = self.engine_binary()?;
        let directory = self.store.directory();
        rule_sets::install(directory, &binary, &name, source, replace)?;
        self.snapshot.rule_sets = Arc::new(rule_sets::list(directory)?);
        let action = if replace { "替换" } else { "导入" };
        Ok(Outcome::notice(format!("已{action}规则集「{name}」。")))
    }

    fn remove_rule_set(&mut self, name: &str) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("断开连接后可修改规则集。".into());
        }
        let rules = crate::routing::parse(&self.snapshot.preferences.routing_rules)?;
        if rule_sets::reference_names(&rules).contains(name) {
            return Err(format!(
                "分流规则仍引用「{name}」。请先移除引用并保存规则。"
            ));
        }
        if self
            .snapshot
            .preferences
            .rule_set_updates
            .contains_key(name)
        {
            self.track_rule_set(name, None)?;
        }
        let directory = self.store.directory();
        rule_sets::remove(directory, name)?;
        self.snapshot.rule_sets = Arc::new(rule_sets::list(directory)?);
        Ok(Outcome::notice(format!("已移除规则集「{name}」。")))
    }

    fn track_rule_set(&mut self, name: &str, url: Option<&str>) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("断开连接后可修改规则集更新来源。".into());
        }
        let mut prefs = self.snapshot.preferences.clone();
        let message = if let Some(url) = url {
            if !rule_sets::list(self.store.directory())?
                .iter()
                .any(|entry| entry == name)
            {
                return Err("请先安装同名规则集，再保存更新来源。".into());
            }
            let sources = &mut prefs.rule_set_updates;
            if sources.len() >= rule_sets::MAX_FILES && !sources.contains_key(name) {
                return Err("规则集定时来源已达上限。".into());
            }
            let remote = rule_sets::Remote::new(url)?;
            if sources.get(name).is_none_or(|old| old.url != remote.url) {
                sources.insert(name.to_owned(), remote);
            }
            format!("已启用规则集「{name}」的定时更新。")
        } else {
            prefs.rule_set_updates.remove(name);
            format!("已停止规则集「{name}」的定时更新。")
        };
        self.store.save_preferences(&prefs)?;
        self.snapshot.preferences = prefs;
        self.rule_set_retry.remove(name);
        Ok(Outcome::notice(message))
    }

    fn refresh_rule_set(&mut self, name: &str) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("断开连接后可更新规则集。".into());
        }
        let mut prefs = self.snapshot.preferences.clone();
        let remote = prefs
            .rule_set_updates
            .get_mut(name)
            .ok_or("此规则集没有定时来源。")?;
        let result = self.engine_binary().and_then(|binary| {
            rule_sets::install(
                self.store.directory(),
                &binary,
                name,
                rule_sets::Source::Url(remote.url.clone()),
                true,
            )
        });
        if let Err(error) = result {
            self.rule_set_retry
                .insert(name.to_owned(), Instant::now() + UPDATE_BACKOFF);
            return Err(format!("规则集「{name}」更新失败。{error}"));
        }
        remote.last_success = Some(unix_now());
        if self.store.save_preferences(&prefs).is_err() {
            self.rule_set_retry
                .insert(name.to_owned(), Instant::now() + UPDATE_BACKOFF);
            return Err(format!("已更新规则集「{name}」，但无法保存更新时间。"));
        }
        self.snapshot.preferences = prefs;
        self.rule_set_retry.remove(name);
        Ok(Outcome::notice(format!("已更新规则集「{name}」。")))
    }

    fn restore_backup(&mut self, path: &Path, passphrase: &str) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("请先断开连接，再恢复完整备份。".into());
        }
        let plaintext = backup::open(path, passphrase)?;
        let restored = backup::decode(&plaintext)?;
        let (stage, staged, snapshot) = self.stage_workspace(restored, false)?;
        self.replace_workspace(stage, staged, snapshot, self.sync_state.clone())?;
        Ok(Outcome {
            focus: self
                .snapshot
                .preferences
                .selected
                .as_ref()
                .map(|node| node.profile),
            message: Some("已恢复完整加密备份。".into()),
        })
    }

    fn stage_workspace(
        &self,
        restored: backup::Restore<'_>,
        sync: bool,
    ) -> Result<(tempfile::TempDir, Store, Snapshot)> {
        let stage = self.store.stage()?;
        let staged = Store::open(stage.path().to_owned())?;
        if !restored.rule_sets.is_empty() {
            let binary = self.engine_binary()?;
            for (name, bytes) in &restored.rule_sets {
                rule_sets::restore_bytes(staged.directory(), &binary, name, bytes)?;
            }
        }
        let mut preferences = restored.preferences;
        preferences.system_proxy = self.snapshot.preferences.system_proxy;
        if sync {
            preferences
                .selected
                .clone_from(&self.snapshot.preferences.selected);
        }
        let mut snapshot = Snapshot {
            catalog: Arc::new(restored.catalog),
            preferences,
            rule_sets: Arc::new(rule_sets::list(staged.directory())?),
            ..Snapshot::default()
        };
        let rules = crate::routing::parse(&snapshot.preferences.routing_rules)?;
        rule_sets::definitions(staged.directory(), &rules)?;
        referenced_policies(&snapshot.preferences, &rules)?;
        if snapshot
            .preferences
            .rule_set_updates
            .keys()
            .any(|name| !snapshot.rule_sets.contains(name))
        {
            return Err("备份缺少定时更新所需的规则集。".into());
        }
        repair_selection(&mut snapshot);
        staged.save_catalog(&snapshot.catalog)?;
        staged.save_preferences(&snapshot.preferences)?;
        Ok((stage, staged, snapshot))
    }

    fn replace_workspace(
        &mut self,
        stage: tempfile::TempDir,
        staged: Store,
        mut snapshot: Snapshot,
        state: sync::State,
    ) -> Result<()> {
        if state.directory.is_some() {
            staged.save_sync_state(&state)?;
        }
        drop(staged);
        self.store.activate(stage)?;
        snapshot.sync = self.snapshot.sync.clone();
        self.snapshot = snapshot;
        self.sync_state = state;
        self.refresh_retry.clear();
        self.rule_set_retry.clear();
        Ok(())
    }

    fn start_sync(&mut self, path: &Path, passphrase: Zeroizing<String>) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("请先断开连接，再开启自动同步。".into());
        }
        let folder = sync::Folder::open(path, passphrase, &self.store)?;
        let mut state = self.sync_state.clone();
        if state.directory.as_deref() != Some(folder.directory()) {
            state = sync::State {
                directory: Some(folder.directory().to_owned()),
                base: None,
            };
        }
        self.store.save_sync_state(&state)?;
        self.snapshot.sync = SyncView {
            directory: state.directory.clone(),
            unlocked: true,
            ..SyncView::default()
        };
        self.sync_state = state;
        self.sync_folder = Some(folder);
        self.sync_tick(None)
    }

    fn sync_tick(&mut self, resolution: Option<(&sync::Conflict, bool)>) -> Result<Outcome> {
        if self.snapshot.connected {
            return Err("断开连接后可以同步配置。".into());
        }
        let Some(mut folder) = self.sync_folder.take() else {
            return Err("请先解锁自动同步。".into());
        };
        let result = self.run_sync(&mut folder, resolution);
        self.sync_folder = Some(folder);
        self.snapshot.sync.problem = result.as_ref().err().cloned();
        result
    }

    fn run_sync(
        &mut self,
        folder: &mut sync::Folder,
        resolution: Option<(&sync::Conflict, bool)>,
    ) -> Result<Outcome> {
        let mut preferences = self.snapshot.preferences.clone();
        preferences.system_proxy = false;
        preferences.selected = None;
        let local = backup::capture(&self.store, &self.snapshot.catalog, &preferences)?;
        let mut state = self.sync_state.clone();
        let status = match folder.plan(state.base, local, resolution)? {
            sync::Plan::Idle => Outcome::default(),
            sync::Plan::Published(id) => {
                state.base = Some(id);
                self.store.save_sync_state(&state)?;
                self.sync_state = state;
                Outcome::notice("已保存新的加密同步版本。")
            }
            sync::Plan::Adopt(id, data) => {
                let (stage, staged, snapshot) =
                    self.stage_workspace(backup::decode(&data)?, true)?;
                state.base = Some(id);
                self.replace_workspace(stage, staged, snapshot, state)?;
                Outcome::notice("已应用共享文件夹的配置。")
            }
            sync::Plan::Merge {
                parents,
                data,
                resolution,
            } => {
                let (stage, staged, snapshot) =
                    self.stage_workspace(backup::decode(&data)?, true)?;
                state.base = Some(folder.publish(&parents, data, !resolution)?);
                self.replace_workspace(stage, staged, snapshot, state)?;
                Outcome::notice(if resolution {
                    "已解决同步冲突。"
                } else {
                    "已合并两台设备的修改。"
                })
            }
            sync::Plan::Conflict(conflict) => {
                self.snapshot.sync.conflict = Some(conflict);
                return Ok(Outcome::notice(
                    "同步已暂停，请在「备份与同步」中处理冲突。",
                ));
            }
        };
        self.snapshot.sync.conflict = None;
        Ok(status)
    }

    fn probe(&mut self, url: &str) -> Result<Outcome> {
        if self.snapshot.tun_active {
            return Err("本地 HTTP/SOCKS5 代理测试不适用于 TUN 连接。".into());
        }
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

fn due_at(last_success: Option<u64>, now: u64, interval: u64) -> u64 {
    last_success
        .filter(|&last| last <= now)
        .map_or(0, |last| last.saturating_add(interval))
}

fn repair_selection(snapshot: &mut Snapshot) -> bool {
    let exists = |member: &NodeRef| {
        snapshot
            .catalog
            .profile(member.profile)
            .is_some_and(|p| p.node(&member.node).is_some())
    };
    let before = snapshot.preferences.auto_members.len();
    snapshot.preferences.auto_members.retain(exists);
    let mut changed = snapshot.preferences.auto_members.len() != before;
    for group in &mut snapshot.preferences.policy_groups {
        let prior_mode = group.mode.clone();
        changed |= group.retain_members(exists) > 0 || group.mode != prior_mode;
    }
    if snapshot.node().is_none() {
        changed |= snapshot.preferences.selected.is_some() || !snapshot.catalog.profiles.is_empty();
        snapshot.preferences.selected = snapshot.catalog.profiles.first().and_then(|profile| {
            profile.nodes.first().map(|node| NodeRef {
                profile: profile.id,
                node: node.id.clone(),
            })
        });
    }
    changed
}

fn policy_mut<'a>(groups: &'a mut [PolicyGroup], name: &str) -> Result<&'a mut PolicyGroup> {
    groups
        .iter_mut()
        .find(|group| group.name == name)
        .ok_or_else(|| format!("策略组「{name}」不存在。"))
}

fn referenced_policies<'a>(
    prefs: &'a Preferences,
    rules: &[serde_json::Value],
) -> Result<Vec<&'a PolicyGroup>> {
    crate::routing::group_names(rules)
        .into_iter()
        .map(|name| {
            prefs
                .policy_groups
                .iter()
                .find(|group| group.name == name)
                .ok_or_else(|| format!("找不到策略组「{name}」。请创建策略组或修改引用。"))
        })
        .collect()
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
