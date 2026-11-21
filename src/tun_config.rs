//! Content identity shared by the desktop and the privileged TUN service.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use ring::digest::{Context, SHA256};
use serde_json::Value;

pub const CONFIG_FILE: &str = "config.json";
pub const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
pub const REQUEST_PREFIX: &[u8] = b"EXODYS TUN 2 ";
const ID_LENGTH: usize = 64;
pub const REQUEST_LENGTH: usize = REQUEST_PREFIX.len() + ID_LENGTH + 1;
pub const UNAPPROVED: &[u8; 6] = b"NOAPP\n";

pub fn requested_id(request: &[u8]) -> Option<&str> {
    let id = request.strip_prefix(REQUEST_PREFIX)?.strip_suffix(b"\n")?;
    if id.len() != ID_LENGTH
        || !id
            .iter()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return None;
    }
    std::str::from_utf8(id).ok()
}

/// Hash the exact configuration and its local rule sets in filename order.
/// The caller owns the trust and permissions policy for opening each file.
pub fn config_id(
    bytes: &[u8],
    mut read_rule_set: impl FnMut(&str, usize) -> io::Result<Vec<u8>>,
) -> io::Result<String> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid TUN configuration");
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(invalid());
    }
    let config: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    // Require the desktop's encoding so duplicate JSON keys cannot be interpreted
    // differently by this parser and the engine.
    if serde_json::to_vec(&config).map_err(|_| invalid())? != bytes {
        return Err(invalid());
    }
    // Approvals cover the generated configuration and its rule sets. Keep other
    // engine subsystems and external paths outside of this format.
    if !only_keys(&config, &["log", "inbounds", "outbounds", "dns", "route"])
        || !only_keys(
            &config["route"],
            &[
                "rules",
                "rule_set",
                "final",
                "default_domain_resolver",
                "auto_detect_interface",
            ],
        )
        || !only_keys(&config["dns"], &["servers", "final"])
        || !config["dns"]["servers"].as_array().is_some_and(|servers| {
            servers.len() == 1
                && only_keys(&servers[0], &["type", "tag", "server"])
                && servers[0]["type"] == "udp"
                && servers[0]["server"]
                    .as_str()
                    .is_some_and(|address| address.parse::<std::net::IpAddr>().is_ok())
        })
        || crate::import::has_local_refs(&config)
        || !config["inbounds"].as_array().is_some_and(|inbounds| {
            inbounds.len() == 1
                && only_keys(&inbounds[0], &["type", "address", "auto_route", "dns_mode"])
                && inbounds[0]["type"] == "tun"
                && inbounds[0]["auto_route"] == true
        })
        || config["route"]["auto_detect_interface"] != true
        || !only_keys(&config["log"], &["level"])
        || config["log"]["level"] != "info"
    {
        return Err(invalid());
    }
    let rules = config["route"]["rule_set"].as_array().ok_or_else(invalid)?;
    if rules.len() > crate::rule_sets::MAX_FILES {
        return Err(invalid());
    }
    let mut filenames = BTreeSet::new();
    for rule in rules {
        let name = rule["tag"].as_str().ok_or_else(invalid)?;
        let file = rule["path"].as_str().ok_or_else(invalid)?;
        if !crate::rule_sets::valid_name(name)
            || file.strip_suffix(".srs") != Some(name)
            || rule != &crate::rule_sets::definition(name, Path::new(file))
            || !filenames.insert(file)
        {
            return Err(invalid());
        }
    }

    let mut digest = Context::new(&SHA256);
    digest.update(b"EXODYS TUN CONFIG 2\0");
    hash_part(&mut digest, bytes);
    for name in filenames {
        let bytes = read_rule_set(name, crate::rule_sets::MAX_BYTES)?;
        if bytes.len() > crate::rule_sets::MAX_BYTES {
            return Err(invalid());
        }
        hash_part(&mut digest, name.as_bytes());
        hash_part(&mut digest, &bytes);
    }
    Ok(digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn only_keys(value: &Value, allowed: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())))
}

fn hash_part(digest: &mut Context, bytes: &[u8]) {
    digest.update(&(bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rule_set_contents_are_part_of_the_id_and_paths_are_local() {
        let mut config = json!({
            "log": {"level": "info"},
            "inbounds": [{"type": "tun", "auto_route": true}],
            "dns": {"servers": [{"type": "udp", "tag": "tun-dns", "server": "192.0.2.53"}]},
            "route": {"auto_detect_interface": true, "rule_set": [
                {"type": "local", "tag": "geoip-cn", "format": "binary", "path": "geoip-cn.srs"}
            ]}
        });
        let identify = |config: &Value, data: &[u8]| {
            config_id(&serde_json::to_vec(config).unwrap(), |file, _| {
                assert_eq!(file, "geoip-cn.srs");
                Ok(data.to_vec())
            })
        };
        let first = identify(&config, b"first").unwrap();
        assert_eq!(first, identify(&config, b"first").unwrap());
        assert_ne!(first, identify(&config, b"second").unwrap());
        let mut external = config.clone();
        external["outbounds"] = json!([{
            "type": "trojan",
            "server": "proxy.example",
            "server_port": 443,
            "tls": {"enabled": true, "certificate_path": "/tmp/fixture.pem"}
        }]);
        assert!(
            config_id(&serde_json::to_vec(&external).unwrap(), |_, _| {
                panic!("invalid configuration must fail before opening rule sets")
            })
            .is_err()
        );
        config["route"]["rule_set"][0]["path"] = json!("../geoip-cn.srs");
        assert!(identify(&config, b"first").is_err());
    }

    #[test]
    fn requests_accept_only_a_version_and_one_lowercase_hash() {
        let id = "a0".repeat(32);
        let request = format!("EXODYS TUN 2 {id}\n");
        assert_eq!(requested_id(request.as_bytes()), Some(id.as_str()));
        for invalid in [
            "EXODYS TUN 1\n".to_owned(),
            format!("EXODYS TUN 2 {}\n", id.to_uppercase()),
            format!("EXODYS TUN 2 {}\n", &id[..63]),
            format!("EXODYS TUN 2 {id}/config.json\n"),
            format!("EXODYS TUN 2 {id}\r\n"),
        ] {
            assert_eq!(requested_id(invalid.as_bytes()), None);
        }
    }
}
