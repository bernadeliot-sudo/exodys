//! Pure, base-aware merging of sync workspace contents.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use crate::backup::Restore;
use crate::catalog::{Catalog, NodeRef, PolicyGroup, PolicyMode, Preferences, Profile};

fn choose<T: PartialEq + Clone>(base: &T, local: &T, remote: &T, what: &str) -> Result<T, String> {
    if local == remote || remote == base {
        Ok(local.clone())
    } else if local == base {
        Ok(remote.clone())
    } else {
        Err(format!("同步冲突：{what}。"))
    }
}

// Keys are sorted so that merging the same two workspaces is deterministic.
fn keyed<K: Ord + Clone, V: PartialEq + Clone>(
    base: BTreeMap<K, V>,
    local: BTreeMap<K, V>,
    remote: BTreeMap<K, V>,
    what: &str,
) -> Result<BTreeMap<K, V>, String> {
    let keys: BTreeSet<_> = base
        .keys()
        .chain(local.keys())
        .chain(remote.keys())
        .cloned()
        .collect();
    let mut result = BTreeMap::new();
    for key in keys {
        if let Some(value) = choose(&base.get(&key), &local.get(&key), &remote.get(&key), what)? {
            result.insert(key, value.clone());
        }
    }
    Ok(result)
}

fn profiles(catalog: &Catalog) -> Result<BTreeMap<u64, Arc<Profile>>, String> {
    let mut result = BTreeMap::new();
    for profile in &catalog.profiles {
        if result.insert(profile.id, profile.clone()).is_some() {
            return Err("同步配置含有重复的配置编号。".into());
        }
    }
    Ok(result)
}

fn groups(preferences: &Preferences) -> Result<BTreeMap<String, PolicyGroup>, String> {
    let mut result = BTreeMap::new();
    for group in &preferences.policy_groups {
        if result.insert(group.name.clone(), group.clone()).is_some() {
            return Err("同步配置含有重复的策略组。".into());
        }
    }
    Ok(result)
}

fn rule_sets<'a>(restore: &Restore<'a>) -> Result<BTreeMap<&'a str, &'a [u8]>, String> {
    let mut result = BTreeMap::new();
    for &(name, bytes) in &restore.rule_sets {
        if !crate::rule_sets::valid_name(name) || result.insert(name, bytes).is_some() {
            return Err("同步配置含有无效或重复的规则集名称。".into());
        }
    }
    Ok(result)
}

pub(super) fn merge<'a>(
    base: &Restore<'a>,
    local: &Restore<'a>,
    remote: &Restore<'a>,
) -> Result<Restore<'a>, String> {
    let profiles = keyed(
        profiles(&base.catalog)?,
        profiles(&local.catalog)?,
        profiles(&remote.catalog)?,
        "配置",
    )?;
    let catalog = local
        .catalog
        .with_profiles(&remote.catalog, profiles.into_values().collect());
    let b = &base.preferences;
    let l = &local.preferences;
    let r = &remote.preferences;
    let preferences = Preferences {
        selected: None,
        system_proxy: false,
        port: choose(&b.port, &l.port, &r.port, "端口")?,
        routing_rules: choose(
            &b.routing_rules,
            &l.routing_rules,
            &r.routing_rules,
            "分流规则",
        )?,
        auto_members: choose(
            &b.auto_members,
            &l.auto_members,
            &r.auto_members,
            "自动组成员",
        )?,
        auto_enabled: choose(
            &b.auto_enabled,
            &l.auto_enabled,
            &r.auto_enabled,
            "自动组开关",
        )?,
        scheduled_refresh: choose(
            &b.scheduled_refresh,
            &l.scheduled_refresh,
            &r.scheduled_refresh,
            "定时更新",
        )?,
        policy_groups: keyed(groups(b)?, groups(l)?, groups(r)?, "策略组")?
            .into_values()
            .collect(),
        rule_set_updates: keyed(
            b.rule_set_updates.clone(),
            l.rule_set_updates.clone(),
            r.rule_set_updates.clone(),
            "规则集来源",
        )?,
    };
    let rule_sets: Vec<_> = keyed(
        rule_sets(base)?,
        rule_sets(local)?,
        rule_sets(remote)?,
        "规则集",
    )?
    .into_iter()
    .collect();
    validate(&catalog, &preferences, &rule_sets)?;
    Ok(Restore {
        catalog,
        preferences,
        rule_sets,
    })
}

fn validate(
    catalog: &Catalog,
    preferences: &Preferences,
    rule_sets: &[(&str, &[u8])],
) -> Result<(), String> {
    if catalog.profiles.len() > crate::catalog::MAX_PROFILES
        || preferences.port < 1024
        || preferences.auto_members.len() > crate::catalog::MAX_AUTO_MEMBERS
        || preferences.policy_groups.len() > crate::catalog::MAX_POLICY_GROUPS
        || rule_sets.len() > crate::rule_sets::MAX_FILES
        || preferences.rule_set_updates.len() > crate::rule_sets::MAX_FILES
    {
        return Err("合并后的配置超过数量上限或端口无效。".into());
    }
    for profile in &catalog.profiles {
        let mut ids = HashSet::new();
        if profile.nodes.is_empty()
            || profile.nodes.len() > crate::import::MAX_NODES
            || profile.refresh_hours.is_some_and(|hours| {
                hours > crate::catalog::MAX_REFRESH_HOURS || profile.subscription.is_none()
            })
            || profile.nodes.iter().any(|node| !ids.insert(&node.id))
        {
            return Err("合并后的配置包含无效或重复的节点编号。".into());
        }
    }
    let exists = |reference: &NodeRef| {
        catalog.profiles.iter().any(|profile| {
            profile.id == reference.profile
                && profile.nodes.iter().any(|node| node.id == reference.node)
        })
    };
    let valid_members = |members: &[NodeRef]| {
        members.len() <= crate::catalog::MAX_AUTO_MEMBERS
            && members.iter().collect::<HashSet<_>>().len() == members.len()
            && members.iter().all(exists)
    };
    if !valid_members(&preferences.auto_members) {
        return Err("合并后的自动组成员无效。".into());
    }
    for group in &preferences.policy_groups {
        if !crate::catalog::valid_policy_name(&group.name)
            || !valid_members(&group.members)
            || matches!(&group.mode, PolicyMode::Manual(Some(member)) if !group.members.contains(member))
        {
            return Err("合并后的策略组或其成员无效。".into());
        }
    }
    let names: HashSet<_> = rule_sets.iter().map(|(name, _)| *name).collect();
    if rule_sets
        .iter()
        .any(|(_, bytes)| bytes.len() > crate::rule_sets::MAX_BYTES)
        || preferences.rule_set_updates.iter().any(|(name, source)| {
            !names.contains(name.as_str()) || crate::rule_sets::Remote::new(&source.url).is_err()
        })
    {
        return Err("合并后的规则集或更新来源无效。".into());
    }
    let rules = crate::routing::parse(&preferences.routing_rules)?;
    if crate::rule_sets::reference_names(&rules)
        .iter()
        .any(|name| !names.contains(name))
        || crate::routing::group_names(&rules).iter().any(|name| {
            !preferences
                .policy_groups
                .iter()
                .any(|group| group.name == *name)
        })
    {
        return Err("合并后的分流规则引用了不存在的规则集或策略组。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Node, Profile};

    fn workspace() -> Restore<'static> {
        Restore {
            catalog: Catalog::default(),
            preferences: Preferences::default(),
            rule_sets: vec![],
        }
    }

    fn profile(id: u64, name: &str) -> Arc<Profile> {
        Arc::new(Profile::new(
            id,
            name.into(),
            None,
            vec![Node::new("node".into(), "node".into(), Default::default())],
        ))
    }

    #[test]
    fn merges_independent_changes_and_normalizes_device_settings() {
        let base = workspace();
        let mut local = workspace();
        let mut remote = workspace();
        local.preferences.port = 3000;
        local.preferences.system_proxy = true;
        local.catalog.profiles.push(profile(1, "one"));
        local.rule_sets.push(("a", b"a"));
        remote.preferences.scheduled_refresh = true;
        remote.catalog.profiles.push(profile(2, "two"));
        remote.rule_sets.push(("b", b"b"));
        let merged = merge(&base, &local, &remote).unwrap();
        assert_eq!(merged.preferences.port, 3000);
        assert!(merged.preferences.scheduled_refresh);
        assert!(!merged.preferences.system_proxy);
        assert!(merged.preferences.selected.is_none());
        assert_eq!(merged.catalog.profiles.len(), 2);
        assert_eq!(merged.rule_sets.len(), 2);
    }

    #[test]
    fn rejects_conflicting_creation_and_edit_delete() {
        let base = workspace();
        let mut local = workspace();
        let mut remote = workspace();
        local.catalog.profiles.push(profile(1, "one"));
        remote.catalog.profiles.push(profile(1, "different"));
        assert!(merge(&base, &local, &remote).is_err());
        let mut base = workspace();
        base.catalog.profiles.push(profile(1, "original"));
        remote.catalog.profiles.clear();
        assert!(merge(&base, &local, &remote).is_err());
    }

    #[test]
    fn rejects_dangling_references_after_merge() {
        let mut base = workspace();
        base.catalog.profiles.push(profile(1, "one"));
        let mut local = workspace();
        local.catalog.profiles.push(profile(1, "one"));
        local.preferences.auto_members.push(NodeRef {
            profile: 1,
            node: "node".into(),
        });
        let remote = workspace();
        assert!(merge(&base, &local, &remote).is_err());
    }
}
