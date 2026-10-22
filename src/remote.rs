//! Read-only Windows directory browsing through two ordinary SSH sessions.
use crate::{config::RemoteConfig, files::Entry};
use std::{io, path::PathBuf, sync::atomic::AtomicBool};
use zeroize::Zeroizing;

#[cfg(any(target_os = "linux", test))]
mod protocol;
#[cfg(target_os = "linux")]
mod terminal;
#[cfg(target_os = "linux")]
pub mod transfer;

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub enum Prompt {
    Secret { hop: &'static str },
    HostKey { hop: &'static str, details: String },
}

pub type Answer = Option<Zeroizing<String>>;
pub type Listing = (PathBuf, Vec<Entry>);

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub fn file_name(path: &str) -> io::Result<String> {
    // Path::file_name on Linux does not recognize Windows backslashes.
    let normalized = path.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." || name.contains(':') {
        return Err(io::Error::other(
            "请选择远程文件或子目录，不能传输盘符根目录",
        ));
    }
    Ok(name.to_string())
}

#[derive(Default)]
pub struct CredentialCache {
    pub jump: Option<Zeroizing<String>>,
    pub windows: Option<Zeroizing<String>>,
    pub expires_at: Option<std::time::Instant>,
}

impl CredentialCache {
    pub fn clear(&mut self) {
        self.jump = None;
        self.windows = None;
        self.expires_at = None;
    }
    pub fn valid(&self) -> bool {
        self.expires_at
            .is_some_and(|t| t > std::time::Instant::now())
    }
    pub fn get(&mut self, hop: &str) -> Option<Zeroizing<String>> {
        if !self.valid() {
            self.clear();
            return None;
        }
        match hop {
            "跳板机" => self.jump.clone(),
            "家里 Windows" => self.windows.clone(),
            _ => None,
        }
    }
    pub fn put(&mut self, hop: &str, value: Zeroizing<String>) {
        match hop {
            "跳板机" => self.jump = Some(value),
            "家里 Windows" => self.windows = Some(value),
            _ => return,
        }
        self.expires_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(300));
    }
}

pub fn read_directory(
    config: &RemoteConfig,
    path: &str,
    cancel: &AtomicBool,
    ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<Listing> {
    config.validate().map_err(io::Error::other)?;
    if path.is_empty() || path.contains('\0') {
        return Err(io::Error::other(
            "请输入 Windows 目录，例如 C:/Users/你的用户名",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        terminal::read_directory(config, path, cancel, ask)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cancel, ask);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "远程浏览目前只支持 Linux 客户端",
        ))
    }
}

/// Windows paths must not be interpreted using the client's Linux path rules.
pub fn resolve_path(current: &str, requested: &str) -> PathBuf {
    let requested = requested.replace('\\', "/");
    let current = current.replace('\\', "/");
    if requested.as_bytes().get(1) == Some(&b':') || requested.starts_with("//") {
        return PathBuf::from(requested);
    }
    if requested.starts_with('/') {
        let drive = if current.as_bytes().get(1) == Some(&b':') {
            &current[..2]
        } else {
            "C:"
        };
        return PathBuf::from(format!("{drive}{requested}"));
    }
    PathBuf::from(format!("{}/{requested}", current.trim_end_matches('/')))
}

pub fn parent_path(path: &str) -> Option<PathBuf> {
    let normalized = path.replace('\\', "/");
    let value = normalized.trim_end_matches('/');
    if value.len() == 2 && value.as_bytes()[1] == b':' {
        return None;
    }
    if value.starts_with("//") && value[2..].split('/').count() <= 2 {
        return None;
    }
    let separator = value.rfind('/')?;
    if separator == 2 && value.as_bytes()[1] == b':' {
        Some(PathBuf::from(format!("{}/", &value[..2])))
    } else if separator > 0 {
        Some(PathBuf::from(&value[..separator]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_file_name_uses_windows_separators_on_every_platform() {
        assert_eq!(file_name(r"C:\Users\home\资料.txt").unwrap(), "资料.txt");
        assert_eq!(file_name("C:/Users/home/资料").unwrap(), "资料");
        assert!(file_name("C:/").is_err());
        assert!(file_name("C:").is_err());
    }
    #[test]
    fn windows_navigation_preserves_drive_and_unc_roots() {
        assert_eq!(parent_path(r"C:\Users"), Some(PathBuf::from("C:/")));
        assert_eq!(parent_path("C:/"), None);
        assert_eq!(parent_path("//server/share"), None);
        assert_eq!(
            resolve_path("C:/Users", r"D:\资料"),
            PathBuf::from("D:/资料")
        );
        assert_eq!(resolve_path("D:/Users", "/"), PathBuf::from("D:/"));
        assert_eq!(
            resolve_path("C:/Users", "资料"),
            PathBuf::from("C:/Users/资料")
        );
    }
}
