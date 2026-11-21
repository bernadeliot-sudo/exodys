//! Immutable encrypted revisions let shared folders deliver concurrent changes
//! without either writer overwriting the other's file.
mod merge;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::Result;
use crate::backup::{self, Restore};
use crate::catalog::{Catalog, Preferences, Store};

const PREFIX: &str = "exodys-sync-";
const SUFFIX: &str = ".exodyssync";
const MAGIC: &[u8] = b"SYNC\0\x01";
const MAX_REVISIONS: usize = 512;
const MISSING: &str = "正在等待共享文件夹下载完整的同步记录。";
const INVALID: &str = "同步记录无效或已损坏。";

type Payload = Zeroizing<Vec<u8>>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Id([u8; 16]);

impl Id {
    fn new() -> Result<Self> {
        let mut bytes = [0; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "无法生成同步版本编号。")?;
        Ok(Self(bytes))
    }

    fn merge(parents: &[Self]) -> Self {
        let mut digest = digest::Context::new(&digest::SHA256);
        digest.update(b"exodys-sync-merge-v1");
        for parent in parents {
            digest.update(&parent.0);
        }
        Self(digest.finish().as_ref()[..16].try_into().unwrap())
    }

    fn filename(self) -> String {
        format!("{PREFIX}{:032x}{SUFFIX}", u128::from_be_bytes(self.0))
    }

    fn parse(name: &str) -> Option<Self> {
        let hex = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
        let id = Self(u128::from_str_radix(hex, 16).ok()?.to_be_bytes());
        (id.filename() == name).then_some(id)
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct State {
    pub(crate) directory: Option<PathBuf>,
    pub(crate) base: Option<Id>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Revision {
    parents: Vec<Id>,
    hash: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub detail: String,
    base: Id,
    incoming: Id,
}

pub(crate) enum Plan {
    Idle,
    Published(Id),
    Adopt(Id, Payload),
    Merge {
        parents: [Id; 2],
        data: Payload,
        resolution: bool,
    },
    Conflict(Conflict),
}

pub(crate) struct Folder {
    directory: PathBuf,
    passphrase: Zeroizing<String>,
    revisions: BTreeMap<Id, Revision>,
}

impl Folder {
    pub(crate) fn open(
        directory: &Path,
        passphrase: Zeroizing<String>,
        store: &Store,
    ) -> Result<Self> {
        backup::validate_passphrase(&passphrase)?;
        let directory = directory
            .canonicalize()
            .map_err(|_| "无法打开同步文件夹。")?;
        if !directory.is_dir() || store.relative_to_root(&directory)?.is_some() {
            return Err("请选择应用数据文件夹之外的共享文件夹。".into());
        }
        let mut folder = Self {
            directory,
            passphrase,
            revisions: BTreeMap::new(),
        };
        folder.scan()?;
        Ok(folder)
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    fn scan(&mut self) -> Result<()> {
        let mut present = BTreeSet::new();
        let entries = fs::read_dir(&self.directory).map_err(|_| "无法读取共享文件夹。")?;
        for entry in entries {
            let entry = entry.map_err(|_| "无法读取共享文件夹中的文件。")?;
            let Some(id) = entry.file_name().to_str().and_then(Id::parse) else {
                continue;
            };
            present.insert(id);
            if present.len() > MAX_REVISIONS {
                return Err("共享文件夹的同步版本已达上限。请备份后选择新的共享文件夹。".into());
            }
        }
        if self.revisions.keys().any(|id| !present.contains(id)) {
            return Err(MISSING.into());
        }
        for id in present {
            if !self.revisions.contains_key(&id) {
                let (revision, _, _) = self.read(id)?;
                self.revisions.insert(id, revision);
            }
        }
        // A provider can deliver children before their parents. Preserve the
        // known graph and wait instead of treating the partial folder as new.
        let mut ordered = BTreeSet::new();
        let mut remaining = self.revisions.iter().collect::<Vec<_>>();
        if remaining.iter().any(|(_, rev)| {
            rev.parents
                .iter()
                .any(|id| !self.revisions.contains_key(id))
        }) {
            return Err(MISSING.into());
        }
        while !remaining.is_empty() {
            let size = remaining.len();
            remaining.retain(|(id, revision)| {
                if revision
                    .parents
                    .iter()
                    .all(|parent| ordered.contains(parent))
                {
                    ordered.insert(**id);
                    false
                } else {
                    true
                }
            });
            if remaining.len() == size {
                return Err(INVALID.into());
            }
        }
        Ok(())
    }

    pub(crate) fn plan(
        &mut self,
        base: Option<Id>,
        local: Payload,
        resolution: Option<(&Conflict, bool)>,
    ) -> Result<Plan> {
        self.scan()?;
        let local_hash = hash(&local);
        let Some(base) = base else {
            if let Some(incoming) = self.heads().first().copied()
                && local_hash == hash(&empty_workspace()?)
            {
                return Ok(Plan::Adopt(incoming, self.data(incoming)?));
            }
            return self.publish(&[], local, false).map(Plan::Published);
        };
        let current = self.revisions.get(&base).ok_or(MISSING)?;
        if let Some((conflict, keep_local)) = resolution {
            if conflict.base != base || !self.heads().contains(&conflict.incoming) {
                return Err("共享版本已更新。请重新同步后选择要保留的版本。".into());
            }
            let data = if keep_local {
                local
            } else {
                self.data(conflict.incoming)?
            };
            return Ok(Plan::Merge {
                parents: sorted_pair(base, conflict.incoming),
                data,
                resolution: true,
            });
        }
        if current.hash != local_hash {
            return self.publish(&[base], local, false).map(Plan::Published);
        }
        let ancestors = self.ancestors(base);
        let Some(incoming) = self.heads().into_iter().find(|id| !ancestors.contains(id)) else {
            return Ok(Plan::Idle);
        };
        let incoming_ancestors = self.ancestors(incoming);
        let remote = self.data(incoming)?;
        if incoming_ancestors.contains(&base) {
            return Ok(Plan::Adopt(incoming, remote));
        }
        let parents = sorted_pair(base, incoming);
        if current.hash == hash(&remote) {
            return Ok(Plan::Merge {
                parents,
                data: local,
                resolution: false,
            });
        }
        let shared = ancestors
            .intersection(&incoming_ancestors)
            .copied()
            .collect::<BTreeSet<_>>();
        let superseded: BTreeSet<_> = shared
            .iter()
            .flat_map(|id| self.revisions[id].parents.iter().copied())
            .collect();
        let mut nearest = shared.difference(&superseded).copied();
        let ancestor = nearest.next();
        let conflict = |detail| {
            Plan::Conflict(Conflict {
                detail,
                base,
                incoming,
            })
        };
        if nearest.next().is_some() {
            return Ok(conflict(
                "两个版本有多个共同历史，需选择保留的版本。".into(),
            ));
        }
        let base_data = ancestor.map_or_else(empty_workspace, |id| self.data(id))?;
        let base_data = backup::decode(&base_data)?;
        let local = backup::decode(&local)?;
        let remote = backup::decode(&remote)?;
        let (first, second) = if parents[0] == base {
            (&local, &remote)
        } else {
            (&remote, &local)
        };
        Ok(match merge::merge(&base_data, first, second) {
            Ok(merged) => Plan::Merge {
                parents,
                data: backup::encode(&merged)?,
                resolution: false,
            },
            Err(detail) => conflict(detail),
        })
    }

    pub(crate) fn publish(
        &mut self,
        parents: &[Id],
        data: Payload,
        automatic_merge: bool,
    ) -> Result<Id> {
        let id = if automatic_merge {
            Id::merge(parents)
        } else {
            Id::new()?
        };
        let revision = Revision {
            parents: parents.to_vec(),
            hash: hash(&data),
        };
        if self.revisions.len() >= MAX_REVISIONS && !self.revisions.contains_key(&id) {
            return Err("共享文件夹的同步版本已达上限。请备份后选择新的共享文件夹。".into());
        }
        let mut sealed = Zeroizing::new(Vec::with_capacity(
            MAGIC.len() + 1 + 3 * 16 + data.len() + 16,
        ));
        sealed.extend_from_slice(MAGIC);
        sealed.extend_from_slice(&id.0);
        sealed.push(parents.len() as u8);
        for parent in parents {
            sealed.extend_from_slice(&parent.0);
        }
        sealed.extend_from_slice(&data);
        let path = self.directory.join(id.filename());
        if let Err(error) = backup::write_archive(&path, &self.passphrase, sealed, false) {
            // Both peers may independently publish the same automatic merge.
            if !automatic_merge
                || !self
                    .read(id)
                    .is_ok_and(|(existing, _, _)| existing == revision)
            {
                return Err(error);
            }
        }
        self.revisions.insert(id, revision);
        Ok(id)
    }

    fn read(&self, id: Id) -> Result<(Revision, Payload, usize)> {
        let payload = backup::open(&self.directory.join(id.filename()), &self.passphrase)?;
        let mut input = payload.strip_prefix(MAGIC).ok_or(INVALID)?;
        let (recorded, rest) = input.split_first_chunk::<16>().ok_or(INVALID)?;
        let (&length, rest) = rest.split_first().ok_or(INVALID)?;
        if id.0 != *recorded || length > 2 {
            return Err(INVALID.into());
        }
        input = rest;
        let mut parents = Vec::with_capacity(length.into());
        for _ in 0..length {
            let (parent, rest) = input.split_first_chunk::<16>().ok_or(INVALID)?;
            let parent = Id(*parent);
            if parent == id || parents.contains(&parent) {
                return Err(INVALID.into());
            }
            parents.push(parent);
            input = rest;
        }
        let revision = Revision {
            parents,
            hash: hash(input),
        };
        let offset = payload.len() - input.len();
        Ok((revision, payload, offset))
    }

    fn data(&self, id: Id) -> Result<Payload> {
        let (revision, mut payload, offset) = self.read(id)?;
        if self.revisions.get(&id) != Some(&revision) {
            return Err(INVALID.into());
        }
        payload.drain(..offset);
        Ok(payload)
    }

    fn heads(&self) -> BTreeSet<Id> {
        let parents = self
            .revisions
            .values()
            .flat_map(|revision| revision.parents.iter().copied())
            .collect::<BTreeSet<_>>();
        self.revisions
            .keys()
            .filter(|id| !parents.contains(id))
            .copied()
            .collect()
    }

    fn ancestors(&self, id: Id) -> BTreeSet<Id> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![id];
        while let Some(id) = pending.pop() {
            if seen.insert(id) {
                pending.extend(self.revisions[&id].parents.iter().copied());
            }
        }
        seen
    }
}

fn sorted_pair(left: Id, right: Id) -> [Id; 2] {
    if left < right {
        [left, right]
    } else {
        [right, left]
    }
}

fn hash(input: &[u8]) -> [u8; 32] {
    digest::digest(&digest::SHA256, input)
        .as_ref()
        .try_into()
        .unwrap()
}

fn empty_workspace() -> Result<Payload> {
    backup::encode(&Restore {
        catalog: Catalog::default(),
        preferences: Preferences::default(),
        rule_sets: Vec::new(),
    })
}
