use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Persistent connection settings. Passwords and private key contents are intentionally absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteConfig {
    pub jump_host: String,
    pub jump_port: String,
    pub jump_user: String,
    pub windows_host: String,
    pub windows_port: String,
    pub windows_user: String,
    pub windows_root: String,
}

impl RemoteConfig {
    pub fn validate(&self) -> Result<(), String> {
        let required = [
            ("跳板机地址", &self.jump_host),
            ("跳板机用户名", &self.jump_user),
            ("Windows 地址", &self.windows_host),
            ("Windows 用户名", &self.windows_user),
        ];
        for (label, value) in required {
            if value.trim().is_empty() {
                return Err(format!("{label}不能为空"));
            }
        }
        for (label, value) in [
            ("跳板机端口", &self.jump_port),
            ("Windows 端口", &self.windows_port),
        ] {
            let port = value
                .parse::<u16>()
                .map_err(|_| format!("{label}必须是 1 到 65535 的数字"))?;
            if port == 0 {
                return Err(format!("{label}必须是 1 到 65535 的数字"));
            }
        }
        if self.windows_root.contains(['\r', '\n']) {
            return Err("Windows 默认目录不能包含换行".into());
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
        [
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
        .collect()
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
