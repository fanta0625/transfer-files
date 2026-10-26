//! Read-only Windows directory browsing through two ordinary SSH sessions.
use crate::{config::RemoteConfig, files::Entry};
use std::{io, path::PathBuf, sync::atomic::AtomicBool};
use zeroize::Zeroizing;

#[cfg(any(target_os = "linux", test))]
mod operations;
#[cfg(all(test, windows))]
mod protocol;
#[cfg(any(target_os = "linux", test))]
mod shell;
#[cfg(target_os = "linux")]
mod terminal;
#[cfg(target_os = "linux")]
pub mod transfer;

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub enum Prompt {
    Secret {
        hop: String,
    },
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    KeyPassphrase {
        hop: String,
        key: String,
    },
    HostKey {
        hop: String,
        details: String,
    },
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
    entries: std::collections::HashMap<String, (Zeroizing<String>, Option<std::time::Instant>)>,
    policy: crate::config::CachePolicy,
}
impl CredentialCache {
    pub fn set_policy(&mut self, policy: crate::config::CachePolicy) {
        if self.policy != policy {
            self.clear();
            self.policy = policy;
        }
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn expire(&mut self) {
        self.entries
            .retain(|_, (_, expiry)| expiry.is_none_or(|t| t > std::time::Instant::now()));
    }
    pub fn get(&mut self, hop: &str) -> Option<Zeroizing<String>> {
        self.expire();
        self.entries.get(hop).map(|(value, _)| value.clone())
    }
    pub fn put(&mut self, hop: &str, value: Zeroizing<String>) {
        use crate::config::CachePolicy;
        let expiry = match self.policy {
            CachePolicy::Once => return,
            CachePolicy::FiveMinutes => {
                Some(std::time::Instant::now() + std::time::Duration::from_secs(300))
            }
            CachePolicy::Session => None,
        };
        self.entries.insert(hop.into(), (value, expiry));
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
            "请输入远程目录，例如 /home/user 或 C:/Users/user",
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

pub fn resolve_path_for(os: crate::config::RemoteOs, current: &str, requested: &str) -> PathBuf {
    if os == crate::config::RemoteOs::Windows {
        resolve_path(current, requested)
    } else if requested.starts_with('/') {
        requested.into()
    } else {
        PathBuf::from(current).join(requested)
    }
}
pub fn parent_path_for(os: crate::config::RemoteOs, path: &str) -> Option<PathBuf> {
    if os == crate::config::RemoteOs::Windows {
        parent_path(path)
    } else {
        std::path::Path::new(path).parent().map(PathBuf::from)
    }
}
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn file_name_for(os: crate::config::RemoteOs, path: &str) -> io::Result<String> {
    if os == crate::config::RemoteOs::Windows {
        file_name(path)
    } else {
        std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from)
            .ok_or_else(|| io::Error::other("请选择文件或子目录"))
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
    fn cache_policies_and_linux_paths() {
        use crate::config::{CachePolicy, RemoteOs};
        let mut cache = CredentialCache::default();
        cache.set_policy(CachePolicy::Once);
        cache.put("node", Zeroizing::new("secret".into()));
        assert!(cache.get("node").is_none());
        cache.set_policy(CachePolicy::Session);
        cache.put("node", Zeroizing::new("secret".into()));
        assert!(cache.entries["node"].1.is_none());
        assert!(cache.get("node").is_some());
        cache.set_policy(CachePolicy::FiveMinutes);
        assert!(cache.get("node").is_none());
        cache.put("node", Zeroizing::new("secret".into()));
        cache.entries.get_mut("node").unwrap().1 = Some(std::time::Instant::now());
        cache.expire();
        assert!(cache.entries.is_empty());
        assert_eq!(
            resolve_path_for(RemoteOs::Linux, "/home/u", "/etc"),
            PathBuf::from("/etc")
        );
        assert_eq!(
            resolve_path_for(RemoteOs::Linux, "/home/u", "docs"),
            PathBuf::from("/home/u/docs")
        );
        assert!(parent_path_for(RemoteOs::Linux, "/").is_none());
        assert_eq!(file_name_for(RemoteOs::Linux, "/tmp/a:b").unwrap(), "a:b");
    }
    #[test]
    fn cache_separates_nodes_and_expiration() {
        let mut cache = CredentialCache::default();
        cache.put("u@first:22", Zeroizing::new("first".into()));
        cache.put("u@second:22", Zeroizing::new("second".into()));
        assert_eq!(cache.get("u@first:22").unwrap().as_str(), "first");
        assert!(cache.get("u@first:2222").is_none());
        cache.entries.get_mut("u@first:22").unwrap().1 = Some(std::time::Instant::now());
        assert!(cache.get("u@first:22").is_none());
        assert_eq!(cache.get("u@second:22").unwrap().as_str(), "second");
        cache.clear();
        assert!(cache.entries.is_empty());
    }
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
