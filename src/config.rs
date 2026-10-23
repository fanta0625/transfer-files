use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Persistent connection settings. Passwords and private key contents are intentionally absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteConfig {
    /// Ordered intermediate SSH nodes. None migrates the legacy jump fields.
    pub route: Option<String>,
    pub jump_host: String,
    pub jump_port: String,
    pub jump_user: String,
    pub windows_host: String,
    pub windows_port: String,
    pub windows_user: String,
    pub windows_root: String,
}

impl RemoteConfig {
    pub fn intermediates(&self) -> Result<Vec<SshNode>, String> {
        if let Some(route) = &self.route {
            route
                .lines()
                .filter(|line| !line.trim().is_empty())
                .enumerate()
                .map(|(index, line)| {
                    let node = SshNode::parse_route_line(line)
                        .map_err(|e| format!("中转节点 {}：{e}", index + 1))?;
                    Ok(node)
                })
                .collect()
        } else if self.jump_host.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(vec![SshNode {
                host: self.jump_host.clone(),
                port: self.jump_port.clone(),
                user: self.jump_user.clone(),
            }])
        }
    }

    pub fn nodes(&self) -> Result<Vec<SshNode>, String> {
        let mut nodes = self.intermediates()?;
        nodes.push(SshNode {
            host: self.windows_host.clone(),
            port: self.windows_port.clone(),
            user: self.windows_user.clone(),
        });
        for node in &nodes {
            node.validate()?;
        }
        if nodes.len() > 8 {
            return Err("最多支持 8 个 SSH 节点（含目标），请缩短连接链".into());
        }
        Ok(nodes)
    }

    pub fn route_text(&self) -> String {
        self.route.clone().unwrap_or_else(|| {
            self.intermediates()
                .unwrap_or_default()
                .iter()
                .map(|n| format!("{}@{} -p {}", n.user, n.host, n.port))
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    pub fn is_configured(&self) -> bool {
        self.validate().is_ok()
    }

    pub fn validate(&self) -> Result<(), String> {
        self.nodes()?;
        if self.windows_root.is_empty() || self.windows_root.contains(['\r', '\n', '\0']) {
            return Err("目标默认目录不能为空或包含换行、空字符".into());
        }
        Ok(())
    }

    pub fn load() -> io::Result<Self> {
        let path = config_path();
        match fs::read_to_string(path) {
            Ok(contents) => Ok(Self::from_contents(&contents)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self {
                jump_port: "22".into(),
                windows_port: "22".into(),
                windows_root: "/".into(),
                ..Self::default()
            }),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self) -> io::Result<PathBuf> {
        let path = config_path();
        let directory = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "配置路径没有父目录"))?;
        fs::create_dir_all(directory)?;
        let temporary = path.with_extension("conf.tmp");
        fs::write(&temporary, self.to_contents())?;
        fs::rename(&temporary, &path)?;
        Ok(path)
    }

    fn from_contents(contents: &str) -> Self {
        let mut config = Self {
            jump_port: "22".into(),
            windows_port: "22".into(),
            windows_root: "/".into(),
            ..Self::default()
        };
        for line in contents.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "route" => config.route = Some(value.replace('|', "\n")),
                "jump_host" => config.jump_host = value,
                "jump_port" => config.jump_port = value,
                "jump_user" => config.jump_user = value,
                "windows_host" => config.windows_host = value,
                "windows_port" => config.windows_port = value,
                "windows_user" => config.windows_user = value,
                "windows_root" => config.windows_root = value,
                _ => {}
            }
        }
        config
    }

    fn to_contents(&self) -> String {
        let mut contents: String = [
            ("jump_host", &self.jump_host),
            ("jump_port", &self.jump_port),
            ("jump_user", &self.jump_user),
            ("windows_host", &self.windows_host),
            ("windows_port", &self.windows_port),
            ("windows_user", &self.windows_user),
            ("windows_root", &self.windows_root),
        ]
        .into_iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect();
        if let Some(route) = &self.route {
            contents.push_str(&format!("route={}\n", route.replace('\n', "|")));
        }
        contents
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshNode {
    pub host: String,
    pub port: String,
    pub user: String,
}
impl SshNode {
    fn parse_route_line(line: &str) -> Result<Self, String> {
        let tokens: Vec<_> = line.split_whitespace().collect();
        // Preserve the previous host/port/user format, including usernames with spaces.
        if tokens.len() >= 3 && tokens[1].parse::<u16>().is_ok() {
            let (host, rest) = line.trim().split_once(char::is_whitespace).unwrap();
            let (port, user) = rest.trim_start().split_once(char::is_whitespace).unwrap();
            let node = Self {
                host: host.into(),
                port: port.into(),
                user: user.trim().into(),
            };
            node.validate()?;
            return Ok(node);
        }
        let mut args = tokens.iter().copied().peekable();
        if args.peek() == Some(&"ssh") {
            args.next();
        }
        let mut destination = None;
        let mut port = None;
        while let Some(arg) = args.next() {
            if arg == "-p" {
                if port.is_some() {
                    return Err("请勿重复指定 -p 端口".into());
                }
                port = Some(args.next().ok_or("-p 后需要端口")?);
            } else if let Some(value) = arg.strip_prefix("-p") {
                if port.is_some() {
                    return Err("请勿重复指定 -p 端口".into());
                }
                port = Some(value);
            } else if arg.starts_with('-') || destination.replace(arg).is_some() {
                return Err(
                    "请输入 user@example.com -p 22（可加 ssh 前缀，仅支持 -p 选项）".into(),
                );
            }
        }
        let (user, host) = destination
            .and_then(|d| d.rsplit_once('@'))
            .ok_or("请输入 user@example.com -p 22，必须包含用户名和地址")?;
        let node = Self {
            host: host.into(),
            port: port.unwrap_or("22").into(),
            user: user.into(),
        };
        node.validate()?;
        Ok(node)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn label(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.host.is_empty()
            || self.host.starts_with('-')
            || self
                .host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '|')
        {
            return Err("SSH 地址无效".into());
        }
        if self.user.trim().is_empty() || self.user.chars().any(|c| c.is_control() || c == '|') {
            return Err("SSH 用户名无效".into());
        }
        if self.port.parse::<u16>().ok().filter(|p| *p > 0).is_none() {
            return Err("端口必须为 1 到 65535".into());
        }
        Ok(())
    }
}

fn config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Ok(app_data) = std::env::var("APPDATA") {
        return PathBuf::from(app_data)
            .join("transfer-files")
            .join("config.conf");
    }

    if let Ok(config_home) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home)
            .join("transfer-files")
            .join("config.conf");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("transfer-files")
            .join("config.conf");
    }
    Path::new(".config/transfer-files/config.conf").to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::RemoteConfig;
    #[test]
    fn ssh_style_routes_and_invalid_arguments() {
        for line in [
            "alice@example.com -p 2222",
            "ssh -p 2222 alice@example.com",
            "ssh alice@example.com -p2222",
            "example.com 2222 alice",
        ] {
            let node = super::SshNode::parse_route_line(line).unwrap();
            assert_eq!(node.label(), "alice@example.com:2222");
        }
        assert_eq!(
            super::SshNode::parse_route_line("alice@example.com")
                .unwrap()
                .port,
            "22"
        );
        for line in [
            "ssh",
            "example.com",
            "alice@example.com -p",
            "alice@example.com -p 0",
            "alice@example.com -p nope",
            "alice@example.com -p 22 -p 23",
            "ssh -J jump alice@example.com",
            "alice@example.com echo hi",
            "@example.com",
        ] {
            assert!(super::SshNode::parse_route_line(line).is_err(), "{line}");
        }
        let config = RemoteConfig {
            route: Some("ssh alice@example.com -p 2222\nbob@second.example.com".into()),
            ..RemoteConfig::default()
        };
        assert_eq!(
            RemoteConfig::from_contents(&config.to_contents()).intermediates(),
            config.intermediates()
        );
    }

    #[test]
    fn migrate_direct_and_recursive_routes_roundtrip() {
        let legacy = RemoteConfig::from_contents(
            "jump_host=a\njump_port=22\njump_user=u\nwindows_host=target\nwindows_port=2222\nwindows_user=v\nwindows_root=C:/\n",
        );
        assert_eq!(legacy.nodes().unwrap().len(), 2);
        let mut direct = legacy.clone();
        direct.route = Some(String::new());
        assert_eq!(direct.nodes().unwrap().len(), 1);
        assert_eq!(
            RemoteConfig::from_contents(&direct.to_contents()).nodes(),
            direct.nodes()
        );
        direct.route =
            Some("one.example 22 user\n2001:db8::1 2200 user two\nthird.example 2222 user".into());
        assert_eq!(direct.nodes().unwrap().len(), 4);
        assert_eq!(
            RemoteConfig::from_contents(&direct.to_contents()).nodes(),
            direct.nodes()
        );
        direct.route = Some("one.example invalid user".into());
        assert!(direct.validate().is_err());
    }

    #[test]
    fn parses_and_serializes_connection_fields_without_passwords() {
        let parsed = RemoteConfig::from_contents(
            "jump_host=bastion.example\njump_port=2200\njump_user=alice\nwindows_host=100.64.0.2\nwindows_port=22\nwindows_user=bob\nwindows_root=C:/Users/bob\nignored=value\n",
        );
        assert_eq!(parsed.jump_host, "bastion.example");
        assert_eq!(parsed.jump_port, "2200");
        assert_eq!(parsed.windows_root, "C:/Users/bob");
        assert!(!parsed.to_contents().contains("password"));
    }

    #[test]
    fn validates_required_values_and_ports() {
        let mut config = RemoteConfig {
            route: None,
            jump_host: "bastion".into(),
            jump_port: "22".into(),
            jump_user: "alice".into(),
            windows_host: "100.64.0.2".into(),
            windows_port: "22".into(),
            windows_user: "bob".into(),
            windows_root: "C:/Users/bob".into(),
        };
        assert!(config.validate().is_ok());
        config.windows_port = "0".into();
        assert!(config.validate().is_err());
    }
}
