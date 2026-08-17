use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::config::home_dir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    pub alias: String,
    pub ssh: String,
    pub socket: String,
    pub remote_zmux: String,
    pub dir: Option<String>,
}

impl SshHost {
    pub fn from_alias(alias: &str) -> Self {
        Self {
            alias: alias.to_string(),
            ssh: alias.to_string(),
            socket: "default".to_string(),
            remote_zmux: "zmux".to_string(),
            dir: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SshConfig {
    pub hosts: BTreeMap<String, SshHost>,
}

pub fn config_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".config").join("zmux").join("ssh.toml"))
}

pub fn resolve_host(alias: &str) -> SshHost {
    if let Some(path) = config_path() {
        if let Ok(cfg) = load_path(&path) {
            if let Some(host) = cfg.hosts.get(alias) {
                return host.clone();
            }
        }
    }
    SshHost::from_alias(alias)
}

pub fn load_path(path: &Path) -> std::io::Result<SshConfig> {
    let text = fs::read_to_string(path)?;
    Ok(parse_ssh_toml(&text))
}

fn parse_ssh_toml(text: &str) -> SshConfig {
    let mut cfg = SshConfig::default();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[hosts.") {
            if let Some(name) = rest.strip_suffix(']') {
                let alias = name.trim().to_string();
                cfg.hosts
                    .entry(alias.clone())
                    .or_insert_with(|| SshHost::from_alias(&alias));
                current = Some(alias);
            }
            continue;
        }
        let Some(alias) = current.as_deref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        if let Some(host) = cfg.hosts.get_mut(alias) {
            match key {
                "ssh" => host.ssh = value,
                "socket" => host.socket = value,
                "remote_zmux" => host.remote_zmux = value,
                "dir" => host.dir = Some(value),
                _ => {}
            }
        }
    }
    cfg
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if let Some(inner) =
        value.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
    {
        return inner.replace("\\\"", "\"");
    }
    if let Some(inner) =
        value.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))
    {
        return inner.to_string();
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_host_block() {
        let cfg = parse_ssh_toml(
            r#"
[hosts.linux]
ssh = "linux"
socket = "default"
remote_zmux = "zmux"
dir = "~"
"#,
        );
        let host = cfg.hosts.get("linux").unwrap();
        assert_eq!(host.ssh, "linux");
        assert_eq!(host.socket, "default");
        assert_eq!(host.dir.as_deref(), Some("~"));
    }
}
