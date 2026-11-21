use std::{fs::File, io::Read, net::IpAddr, path::Path};

use serde::Deserialize;

use serde_json::{Value, json};

use crate::Result;

pub(crate) const MAX_BYTES: usize = 16 * 1024;

/// Load a local YAML rule list into an editor draft. Fail closed if any
/// rule would require lossy conversion, a policy group or a remote rule set.
pub(crate) fn read_clash_rules(path: &Path) -> Result<String> {
    const MAX_FILE: u64 = crate::import::MAX_BYTES as u64;
    if !path
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= MAX_FILE)
    {
        return Err("请选择不超过 8 MiB 的 Clash YAML 普通文件。".into());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_FILE + 1).read_to_end(&mut bytes))
        .map_err(|_| "无法读取规则文件。")?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("规则文件超过 8 MiB。".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "请选择 UTF-8 编码的 Clash YAML。")?;
    #[derive(Deserialize)]
    struct ClashRules {
        rules: Vec<String>,
    }
    let ClashRules { rules } = serde_saphyr::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| "无法读取 YAML 的 rules 列表。")?;
    if rules.is_empty() {
        return Err("YAML 中没有分流规则。".into());
    }
    // A domain is not resolved solely to match our IP rules. Require Clash's
    // no-resolve flag instead of silently changing the resolution policy.
    // One YAML item must remain one rule. Prevent scalars containing embedded
    // newlines or comments from hiding rules from validation.
    let normalized = rules
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            if rule.trim().is_empty()
                || rule.trim_start().starts_with('#')
                || rule.chars().any(char::is_control)
            {
                return Err(format!("分流规则第 {} 条不是单行规则。", index + 1));
            }
            let parts: Vec<_> = rule.split(',').map(str::trim).collect();
            if parts
                .last()
                .is_some_and(|action| action.starts_with("GROUP:"))
            {
                return Err(format!(
                    "分流规则第 {} 条包含策略组动作，请手动配置策略组引用。",
                    index + 1
                ));
            }
            match parts.as_slice() {
                ["IP-CIDR" | "IP-CIDR6", _, _, "no-resolve"] => Ok(parts[..3].join(",")),
                ["IP-CIDR" | "IP-CIDR6", ..] => Err(format!(
                    "分流规则第 {} 条缺少 no-resolve，无法保留 DNS 匹配行为。",
                    index + 1
                )),
                ["RULE-SET" | "GEOIP" | "GEOSITE", ..] => Err(format!(
                    "分流规则第 {} 条引用了规则集。请在规则集库中导入 .srs，再手动添加引用。",
                    index + 1
                )),
                _ => Ok(rule.clone()),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let draft = normalized.join("\n");
    parse(&draft).map_err(|e| format!("未导入。{e}文件内的策略组和远程规则集不会自动转换。"))?;
    Ok(draft)
}

/// Convert the supported Clash rule syntax to ordered sing-box route actions.
/// An empty rule list leaves the selected proxy as the default route.
pub(crate) fn parse(text: &str) -> Result<Vec<Value>> {
    if text.len() > MAX_BYTES {
        return Err("分流规则文本超过 16 KiB。".into());
    }
    let mut rules = Vec::new();
    let mut has_default = false;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let error = |reason: &str| format!("分流规则第 {} 行：{reason}", index + 1);
        if has_default {
            return Err(error("MATCH 必须是最后一条规则。"));
        }
        let parts: Vec<_> = line.split(',').map(str::trim).collect();
        let (mut rule, target) = match parts.as_slice() {
            ["MATCH", target] => {
                has_default = true;
                (json!({}), target)
            }
            [kind @ ("RULE-SET" | "GEOIP" | "GEOSITE"), value, target] => {
                let name = match *kind {
                    "RULE-SET" => (*value).to_owned(),
                    _ => format!(
                        "{}-{}",
                        kind.to_ascii_lowercase(),
                        value.to_ascii_lowercase()
                    ),
                };
                if value.is_empty() || !crate::rule_sets::valid_name(&name) {
                    return Err(error(
                        "规则集名称须使用 1–48 个小写字母、数字、连字符或下划线。",
                    ));
                }
                (json!({"rule_set": name}), target)
            }
            [kind, value, target] => {
                let value = value.to_ascii_lowercase();
                let field = match *kind {
                    "DOMAIN" | "DOMAIN-SUFFIX" => {
                        if !valid_domain(&value) {
                            return Err(error("请填写有效的域名。"));
                        }
                        if *kind == "DOMAIN" {
                            "domain"
                        } else {
                            "domain_suffix"
                        }
                    }
                    "DOMAIN-KEYWORD"
                        if !value.is_empty()
                            && value
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)) =>
                    {
                        "domain_keyword"
                    }
                    "IP-CIDR" | "IP-CIDR6" => {
                        if !valid_prefix(&value, *kind == "IP-CIDR6") {
                            return Err(error("IP 地址或前缀长度无效。"));
                        }
                        "ip_cidr"
                    }
                    _ => return Err(error("规则类型或匹配值无效。")),
                };
                (json!({(field): [value]}), target)
            }
            _ => return Err(error("请使用 类型,匹配值,动作 或 MATCH,动作。")),
        };
        match *target {
            "DIRECT" | "PROXY" => {
                rule["action"] = json!("route");
                rule["outbound"] = json!(if *target == "DIRECT" {
                    "direct"
                } else {
                    "selected"
                });
            }
            "REJECT" => rule["action"] = json!("reject"),
            target => {
                let name = target
                    .strip_prefix("GROUP:")
                    .filter(|name| crate::catalog::valid_policy_name(name))
                    .ok_or_else(|| error("动作须为 DIRECT、PROXY、REJECT 或 GROUP:策略组名称。"))?;
                rule["action"] = json!("route");
                rule["outbound"] = json!(group_tag(name));
            }
        }
        rules.push(rule);
    }
    Ok(rules)
}

const GROUP_TAG_PREFIX: &str = "policy:";

pub(crate) fn group_tag(name: &str) -> String {
    format!("{GROUP_TAG_PREFIX}{name}")
}

pub(crate) fn group_names(rules: &[Value]) -> std::collections::BTreeSet<&str> {
    rules
        .iter()
        .filter_map(|rule| {
            rule.get("outbound")?
                .as_str()?
                .strip_prefix(GROUP_TAG_PREFIX)
        })
        .collect()
}

fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn valid_prefix(value: &str, ipv6: bool) -> bool {
    let Some((address, bits)) = value.split_once('/') else {
        return false;
    };
    match (address.parse::<IpAddr>(), bits.parse::<u8>()) {
        (Ok(IpAddr::V4(_)), Ok(0..=32)) => !ipv6,
        (Ok(IpAddr::V6(_)), Ok(0..=128)) => ipv6,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clash_file_requires_complete_supported_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.yaml");
        std::fs::write(
            &path,
            "proxies: []\nrules:\n  - DOMAIN,fixture.test,DIRECT\n  - MATCH,REJECT\n",
        )
        .unwrap();
        let draft = read_clash_rules(&path).unwrap();
        assert_eq!(draft, "DOMAIN,fixture.test,DIRECT\nMATCH,REJECT");
        for yaml in [
            "rules: []",
            "rules: [3]",
            "rules:\n  - DOMAIN,x.test,DIRECT\n  - RULE-SET,external,PROXY",
            "rules:\n  - |-\n    DOMAIN,x.test,DIRECT\n    MATCH,PROXY",
            "rules:\n  - MATCH,ProviderGroup",
        ] {
            std::fs::write(&path, yaml).unwrap();
            assert!(read_clash_rules(&path).is_err(), "{yaml}");
        }
    }

    #[test]
    fn converts_rules_in_order_and_reports_the_invalid_line() {
        let rules = parse("# home\nDOMAIN,Box.Example,DIRECT\nDOMAIN-SUFFIX,example,REJECT\nIP-CIDR6,::1/128,DIRECT\nMATCH,PROXY").unwrap();
        assert_eq!(
            rules[0],
            json!({"domain":["box.example"],"action":"route","outbound":"direct"})
        );
        assert_eq!(
            rules[1],
            json!({"domain_suffix":["example"],"action":"reject"})
        );
        assert_eq!(rules[2]["ip_cidr"], json!(["::1/128"]));
        assert_eq!(rules[3], json!({"action":"route","outbound":"selected"}));
        for bad in [
            "IP-CIDR,::1/128,DIRECT",
            "IP-CIDR,127.0.0.1/33,DIRECT",
            "DOMAIN,-box.example,DIRECT",
            "DOMAIN,example,missing",
            "DOMAIN-KEYWORD,,DIRECT",
        ] {
            assert!(
                parse(&format!("# first\n{bad}"))
                    .unwrap_err()
                    .contains("第 2 行")
            );
        }
        assert!(parse("MATCH,REJECT\nMATCH,DIRECT").is_err());
    }
}
