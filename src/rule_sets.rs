//! Local copies of explicitly imported sing-box binary rule sets. The engine
//! never fetches rule data or resolves paths taken from a rule expression.
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::Result;

pub(crate) const MAX_FILES: usize = 32;
const MAX_URL_BYTES: usize = 2048;
pub(crate) const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DECOMPRESSED: u64 = 48 * 1024 * 1024;

pub enum Source {
    File(PathBuf),
    Url(String),
}

#[derive(Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Remote {
    pub(crate) url: String,
    pub(crate) last_success: Option<u64>,
}

impl Remote {
    pub(crate) fn new(url: &str) -> Result<Self> {
        Ok(Self {
            url: remote_url(url)?.into(),
            last_success: None,
        })
    }
}

pub(crate) fn valid_name(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 48
        && label.bytes().all(|char| {
            char.is_ascii_lowercase() || char.is_ascii_digit() || matches!(char, b'-' | b'_')
        })
}

fn directory(workspace: &Path) -> PathBuf {
    workspace.join("rule-sets")
}

pub(crate) fn list(workspace: &Path) -> Result<Vec<String>> {
    let dir = directory(workspace);
    match dir.symlink_metadata() {
        Ok(meta) if meta.is_dir() => (),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        _ => return Err("规则集库不是普通文件夹。".into()),
    }
    let entries = fs::read_dir(&dir).map_err(|_| "无法读取规则集库。")?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| "无法读取规则集库条目。")?;
        let file = entry.file_name();
        let Some(name) = file
            .to_str()
            .and_then(|s| s.strip_suffix(".srs"))
            .filter(|s| valid_name(s))
        else {
            continue;
        };
        if entry.file_type().is_ok_and(|typ| typ.is_file()) {
            names.push(name.to_owned());
        }
        if names.len() > MAX_FILES {
            return Err("规则集库包含超过 32 个文件。".into());
        }
    }
    names.sort_unstable();
    Ok(names)
}

fn path(workspace: &Path, name: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        return Err("名称使用 1–48 个小写字母、数字、连字符或下划线。".into());
    }
    Ok(directory(workspace).join(format!("{name}.srs")))
}

fn read_bounded(reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读完规则集。")?;
    if bytes.len() > MAX_BYTES {
        return Err("规则集超过 8 MiB。".into());
    }
    Ok(bytes)
}

fn validate_frame(bytes: &[u8]) -> Result<()> {
    if !bytes.starts_with(b"SRS") || bytes.len() < 5 {
        return Err("请选择 sing-box 二进制 .srs 规则集。".into());
    }
    let decoded = flate2::read::ZlibDecoder::new(&bytes[4..]);
    let size = io::copy(&mut decoded.take(MAX_DECOMPRESSED + 1), &mut io::sink())
        .map_err(|_| "规则集压缩数据损坏。")?;
    if size == 0 || size > MAX_DECOMPRESSED {
        return Err("规则集展开数据为空或超过 48 MiB。".into());
    }
    Ok(())
}

impl Source {
    fn load(self) -> Result<Vec<u8>> {
        let bytes = match self {
            Self::File(path) => {
                if !path
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.len() <= MAX_BYTES as u64)
                {
                    return Err("请选择不超过 8 MiB 的 .srs 文件。".into());
                }
                read_bounded(File::open(path).map_err(|_| "无法打开规则集文件。")?)?
            }
            Self::Url(text) => {
                let url = remote_url(&text)?;
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(20)))
                    .https_only(true)
                    .max_redirects(3)
                    .proxy(None)
                    .build()
                    .into();
                let mut response = agent
                    .get(url.as_str())
                    .call()
                    .map_err(|_| "无法下载规则集，请检查 HTTPS 来源和网络。")?;
                read_bounded(response.body_mut().as_reader())?
            }
        };
        Ok(bytes)
    }
}

fn remote_url(text: &str) -> Result<url::Url> {
    let text = text.trim();
    if text.len() > MAX_URL_BYTES {
        return Err("规则集 URL 超过 2048 字节。".into());
    }
    let url = url::Url::parse(text).map_err(|_| "规则集 URL 无效。")?;
    if url.as_str().len() > MAX_URL_BYTES
        || url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("请输入无用户名、密码或片段的 HTTPS 规则集 URL。".into());
    }
    Ok(url)
}

pub(crate) fn install(
    workspace: &Path,
    binary: &Path,
    name: &str,
    source: Source,
    replace: bool,
) -> Result<()> {
    let destination = path(workspace, name)?;
    let existing = list(workspace)?;
    if existing.iter().any(|entry| entry == name) != replace {
        return Err(if replace {
            "要替换的规则集不存在。"
        } else {
            "同名规则集已存在。请明确选择替换。"
        }
        .into());
    }
    if !replace && existing.len() >= MAX_FILES {
        return Err("最多保存 32 个规则集。".into());
    }
    let bytes = source.load()?;
    install_data(workspace, binary, destination, &bytes, replace)
}

pub(crate) fn backup_bytes(workspace: &Path, name: &str) -> Result<Vec<u8>> {
    let file = path(workspace, name)?;
    if !file
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_BYTES as u64)
    {
        return Err(format!("无法备份规则集「{name}」。"));
    }
    let bytes = read_bounded(File::open(file).map_err(|_| format!("无法打开规则集「{name}」。"))?)?;
    validate_frame(&bytes)?;
    Ok(bytes)
}

pub(crate) fn restore_bytes(
    workspace: &Path,
    binary: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<()> {
    install_data(workspace, binary, path(workspace, name)?, bytes, false)
}

fn install_data(
    workspace: &Path,
    binary: &Path,
    destination: PathBuf,
    bytes: &[u8],
    replace: bool,
) -> Result<()> {
    if bytes.len() > MAX_BYTES {
        return Err("规则集超过 8 MiB。".into());
    }
    validate_frame(bytes)?;
    let folder = directory(workspace);
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    if !folder.is_dir() {
        builder
            .create(&folder)
            .map_err(|_| "无法创建规则集文件夹。")?;
    }
    let error = || "无法保存规则集。".to_owned();
    let mut file = tempfile::NamedTempFile::new_in(&folder).map_err(|_| error())?;
    file.write_all(bytes)
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| error())?;
    crate::engine::validate_rule_set(binary, workspace, file.path())?;
    if replace {
        file.persist(destination)
    } else {
        file.persist_noclobber(destination)
    }
    .map_err(|_| error())?;
    Ok(())
}

pub(crate) fn remove(workspace: &Path, name: &str) -> Result<()> {
    if !list(workspace)?.iter().any(|label| label == name) {
        return Err("规则集不存在。".into());
    }
    fs::remove_file(path(workspace, name)?).map_err(|_| "无法删除规则集。".into())
}

pub(crate) fn reference_names(rules: &[Value]) -> BTreeSet<&str> {
    rules
        .iter()
        .filter_map(|r| r.get("rule_set").and_then(Value::as_str))
        .collect()
}

pub(crate) fn definitions(workspace: &Path, rules: &[Value]) -> Result<Vec<Value>> {
    let names = reference_names(rules);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let allowed = list(workspace)?;
    names
        .into_iter()
        .map(|name| {
            if !allowed.iter().any(|s| s == name) {
                return Err(format!("找不到规则集「{name}」。请安装规则集或修改引用。"));
            }
            let path = path(workspace, name)?
                .canonicalize()
                .map_err(|_| "无法定位规则集文件。")?;
            if path.to_str().is_none() {
                return Err("规则集路径不是 UTF-8。".into());
            }
            Ok(definition(name, &path))
        })
        .collect()
}

pub(crate) fn definition(name: &str, path: &Path) -> Value {
    json!({"type": "local", "tag": name, "format": "binary", "path": path})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_urls_and_frames_are_bounded() {
        for name in ["", "../local", ".", "Thing", "空间"] {
            assert!(!valid_name(name));
        }
        assert!(valid_name("geoip-cn"));
        for url in [
            "http://fixture.test/a.srs",
            "https://user@fixture.test/a.srs",
            "file:///tmp/a.srs",
            "https://fixture.test/x#fragment",
        ] {
            assert!(remote_url(url).is_err());
        }
        assert!(remote_url("https://fixture.test/foo.srs").is_ok());
        assert!(validate_frame(b"SRS\x01bad").is_err());
        assert!(read_bounded(io::repeat(0).take(MAX_BYTES as u64 + 1)).is_err());
    }
}
