//! Portable settings contain only the local port and plain routing rules.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;

const VERSION: u32 = 1;
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    version: u32,
    port: u16,
    rules: String,
}

fn validate(port: u16, rules: &str) -> Result<()> {
    if port < 1024 {
        return Err("请输入 1024 至 65535 之间的端口。".into());
    }
    crate::routing::parse(rules)?;
    Ok(())
}

pub fn load(path: &Path) -> Result<(u16, String)> {
    let file = File::open(path).map_err(|_| "无法打开设置文件。".to_owned())?;
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取设置文件。".to_owned())?;
    if bytes.len() > MAX_BYTES {
        return Err("设置文件超过大小上限。".into());
    }
    let settings: SettingsFile =
        serde_json::from_slice(&bytes).map_err(|_| "设置文件格式无效。".to_owned())?;
    if settings.version != VERSION {
        return Err("不支持此设置文件的版本。".into());
    }
    validate(settings.port, &settings.rules)?;
    Ok((settings.port, settings.rules))
}

pub fn save(path: &Path, port: u16, rules: &str) -> Result<()> {
    if rules.len() > MAX_BYTES {
        return Err("设置文件超过大小上限。".into());
    }
    validate(port, rules)?;
    let settings = SettingsFile {
        version: VERSION,
        port,
        rules: rules.to_owned(),
    };
    let bytes = serde_json::to_vec(&settings).map_err(|_| "无法生成设置文件。".to_owned())?;
    if bytes.len() > MAX_BYTES {
        return Err("设置文件超过大小上限。".into());
    }
    let failed = || "无法保存设置文件。请检查磁盘空间和文件权限。".to_owned();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|_| failed())?;
    temp.write_all(&bytes).map_err(|_| failed())?;
    temp.as_file().sync_all().map_err(|_| failed())?;
    temp.persist(path).map_err(|_| failed())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn round_trip_only_port_and_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let rules = "# synthetic\nDOMAIN,example.test,DIRECT\nMATCH,PROXY";
        save(&path, 1024, rules).unwrap();
        assert_eq!(load(&path).unwrap(), (1024, rules.to_owned()));
        let json: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"version": 1, "port": 1024, "rules": rules})
        );
    }

    #[test]
    fn rejects_invalid_files_and_preserves_existing_on_failed_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for data in [
            r#"{"version":2,"port":2080,"rules":""}"#,
            r#"{"version":1,"port":1023,"rules":""}"#,
            r#"{"version":1,"port":2080,"rules":"","extra":true}"#,
            r#"{"version":1,"port":2080}"#,
            r#"{"version":1,"port":2080,"rules":"INVALID"}"#,
            "not json",
        ] {
            fs::write(&path, data).unwrap();
            assert!(load(&path).is_err());
        }
        fs::write(&path, vec![b' '; MAX_BYTES + 1]).unwrap();
        assert!(load(&path).is_err());
        fs::write(&path, b"existing").unwrap();
        assert!(save(&path, 1023, "").is_err());
        assert!(save(&path, 2080, "INVALID").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing");
        assert!(save(&path, 2080, &"x".repeat(MAX_BYTES + 1)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing");
    }
}
