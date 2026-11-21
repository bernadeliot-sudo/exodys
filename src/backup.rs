//! Version 1 authenticated archives: magic, 16-byte salt, 12-byte nonce, then
//! AES-256-GCM ciphertext and tag. The encrypted payload contains length-prefixed
//! catalog and preferences JSON, then the count and name/data pair of each SRS.
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::path::Path;

use ring::rand::SecureRandom;
use ring::{aead, pbkdf2, rand};
use serde::Serialize;
use zeroize::Zeroizing;

use crate::catalog::{Catalog, MAX_CATALOG_BYTES, MAX_PREFERENCES_BYTES, Preferences, Store};
use crate::{Result, rule_sets};

const MAGIC: &[u8; 8] = b"EXODYS\0\x01";
const SALT_BYTES: usize = 16;
const HEADER_BYTES: usize = MAGIC.len() + SALT_BYTES + aead::NONCE_LEN;
const ITERATIONS: NonZeroU32 = NonZeroU32::new(600_000).unwrap();
const MAX_BYTES: usize = MAX_CATALOG_BYTES as usize
    + MAX_PREFERENCES_BYTES as usize
    + rule_sets::MAX_FILES * (rule_sets::MAX_BYTES + 64)
    + 128;
const INVALID: &str = "备份文件格式无效、内容损坏或版本不受支持。";
const FAILED: &str = "无法保存加密备份。请检查目标文件夹的权限和剩余空间。";

pub(crate) fn save(
    path: &Path,
    passphrase: &str,
    store: &Store,
    catalog: &Catalog,
    preferences: &Preferences,
) -> Result<()> {
    validate_passphrase(passphrase)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent.canonicalize().map_err(|_| FAILED)?;
    if store.relative_to_root(&parent)?.is_some()
        || path.extension().is_none_or(|ext| ext != "exodysbackup")
    {
        return Err("请在应用数据文件夹之外选择一个 .exodysbackup 文件。".into());
    }
    let destination = parent.join(path.file_name().ok_or(FAILED)?);
    let payload = capture(store, catalog, preferences)?;
    write_archive(&destination, passphrase, payload, true)
}

pub(crate) fn capture(
    store: &Store,
    catalog: &Catalog,
    preferences: &Preferences,
) -> Result<Zeroizing<Vec<u8>>> {
    let workspace = store.directory();
    let names = rule_sets::list(workspace)?;
    let mut payload = encode_start(catalog, preferences, names.len())?;
    for name in names {
        append_block(&mut payload, name.as_bytes());
        append_block(&mut payload, &rule_sets::backup_bytes(workspace, &name)?);
    }
    Ok(payload)
}

pub(crate) fn encode(workspace: &Restore<'_>) -> Result<Zeroizing<Vec<u8>>> {
    let mut payload = encode_start(
        &workspace.catalog,
        &workspace.preferences,
        workspace.rule_sets.len(),
    )?;
    for (name, bytes) in &workspace.rule_sets {
        append_block(&mut payload, name.as_bytes());
        append_block(&mut payload, bytes);
    }
    Ok(payload)
}

fn encode_start(
    catalog: &Catalog,
    preferences: &Preferences,
    count: usize,
) -> Result<Zeroizing<Vec<u8>>> {
    if count > rule_sets::MAX_FILES {
        return Err("最多保存 32 个规则集。".into());
    }
    let mut payload = Zeroizing::new(Vec::new());
    append_json(&mut payload, catalog, MAX_CATALOG_BYTES as usize)?;
    append_json(&mut payload, preferences, MAX_PREFERENCES_BYTES as usize)?;
    payload.push(count as u8);
    Ok(payload)
}

pub(crate) fn write_archive(
    destination: &Path,
    passphrase: &str,
    mut payload: Zeroizing<Vec<u8>>,
    replace: bool,
) -> Result<()> {
    validate_passphrase(passphrase)?;
    let parent = destination.parent().ok_or(FAILED)?;
    let mut header = [0; HEADER_BYTES];
    header[..MAGIC.len()].copy_from_slice(MAGIC);
    rand::SystemRandom::new()
        .fill(&mut header[MAGIC.len()..])
        .map_err(|_| "无法生成备份加密随机数。")?;
    let (key, nonce) = derive_key(&header, passphrase)?;
    key.seal_in_place_append_tag(nonce, aead::Aad::from(header), &mut *payload)
        .map_err(|_| "无法加密备份文件。")?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| FAILED)?;
    file.write_all(&header)
        .and_then(|_| file.write_all(&payload))
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| FAILED)?;
    if replace {
        file.persist(destination)
    } else {
        file.persist_noclobber(destination)
    }
    .map_err(|_| FAILED)?;
    Ok(())
}

pub(crate) fn open(path: &Path, passphrase: &str) -> Result<Zeroizing<Vec<u8>>> {
    validate_passphrase(passphrase)?;
    if !path
        .symlink_metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= MAX_BYTES as u64)
    {
        return Err(INVALID.into());
    }
    let mut file = File::open(path).map_err(|_| "无法打开加密备份文件。")?;
    let mut header = [0; HEADER_BYTES];
    file.read_exact(&mut header).map_err(|_| INVALID)?;
    if !header.starts_with(MAGIC) {
        return Err(INVALID.into());
    }
    let mut payload = Zeroizing::new(Vec::new());
    let max_ciphertext = MAX_BYTES - HEADER_BYTES;
    file.take(max_ciphertext as u64 + 1)
        .read_to_end(&mut payload)
        .map_err(|_| INVALID)?;
    if payload.len() > max_ciphertext {
        return Err(INVALID.into());
    }
    let (key, nonce) = derive_key(&header, passphrase)?;
    let length = key
        .open_in_place(nonce, aead::Aad::from(header), &mut payload)
        .map_err(|_| "密码不正确或备份文件已损坏。")?
        .len();
    payload.truncate(length);
    Ok(payload)
}

pub(crate) struct Restore<'a> {
    pub(crate) catalog: Catalog,
    pub(crate) preferences: Preferences,
    pub(crate) rule_sets: Vec<(&'a str, &'a [u8])>,
}

pub(crate) fn decode(mut payload: &[u8]) -> Result<Restore<'_>> {
    let catalog = serde_json::from_slice(take_block(&mut payload, MAX_CATALOG_BYTES as usize)?)
        .map_err(|_| INVALID)?;
    let preferences =
        serde_json::from_slice(take_block(&mut payload, MAX_PREFERENCES_BYTES as usize)?)
            .map_err(|_| INVALID)?;
    let (catalog, preferences) = Store::validate(catalog, preferences)?;
    if catalog
        .profiles
        .iter()
        .flat_map(|profile| &profile.nodes)
        .any(|node| !crate::import::valid_saved_outbound(&node.outbound))
    {
        return Err("备份含有无法导入的节点选项。".into());
    }
    let (&count, rest) = payload.split_first().ok_or(INVALID)?;
    if count as usize > rule_sets::MAX_FILES {
        return Err(INVALID.into());
    }
    payload = rest;
    let mut rule_sets = Vec::with_capacity(count as usize);
    let mut names = HashSet::new();
    for _ in 0..count {
        let name = std::str::from_utf8(take_block(&mut payload, 48)?).map_err(|_| INVALID)?;
        if !crate::rule_sets::valid_name(name) || !names.insert(name) {
            return Err(INVALID.into());
        }
        rule_sets.push((name, take_block(&mut payload, crate::rule_sets::MAX_BYTES)?));
    }
    if !payload.is_empty() {
        return Err(INVALID.into());
    }
    Ok(Restore {
        catalog,
        preferences,
        rule_sets,
    })
}

pub(crate) fn validate_passphrase(passphrase: &str) -> Result<()> {
    if passphrase.len() > 1024 || passphrase.chars().count() < 12 {
        return Err("备份密码至少需要 12 个字符，最多 1024 字节。".into());
    }
    Ok(())
}

fn derive_key(
    header: &[u8; HEADER_BYTES],
    passphrase: &str,
) -> Result<(aead::LessSafeKey, aead::Nonce)> {
    let mut key = Zeroizing::new([0; 32]);
    let salt_end = MAGIC.len() + SALT_BYTES;
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        ITERATIONS,
        &header[MAGIC.len()..salt_end],
        passphrase.as_bytes(),
        &mut *key,
    );
    let cipher = aead::UnboundKey::new(&aead::AES_256_GCM, &*key).map_err(|_| INVALID)?;
    let nonce = aead::Nonce::try_assume_unique_for_key(&header[salt_end..]).map_err(|_| INVALID)?;
    Ok((aead::LessSafeKey::new(cipher), nonce))
}

fn append_json(data: &mut Vec<u8>, value: &impl Serialize, limit: usize) -> Result<()> {
    let length_start = data.len();
    data.extend([0; 4]);
    serde_json::to_writer(&mut *data, value).map_err(|_| "无法编码备份内容。")?;
    let length = data.len() - length_start - 4;
    if length > limit {
        return Err("配置库超过备份的大小上限。".into());
    }
    data[length_start..length_start + 4].copy_from_slice(&(length as u32).to_be_bytes());
    Ok(())
}

fn append_block(data: &mut Vec<u8>, block: &[u8]) {
    data.extend((block.len() as u32).to_be_bytes());
    data.extend(block);
}

fn take_block<'a>(data: &mut &'a [u8], limit: usize) -> Result<&'a [u8]> {
    let (length, rest) = data.split_first_chunk::<4>().ok_or(INVALID)?;
    let length = u32::from_be_bytes(*length) as usize;
    if length > limit {
        return Err(INVALID.into());
    }
    let (block, remaining) = rest.split_at_checked(length).ok_or(INVALID)?;
    *data = remaining;
    Ok(block)
}
