use std::net::IpAddr;

use serde_json::{Value, json};

use crate::Result;

pub(crate) const MAX_BYTES: usize = 16 * 1024;

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
            _ => return Err(error("动作须为 DIRECT、PROXY 或 REJECT。")),
        }
        rules.push(rule);
    }
    Ok(rules)
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
