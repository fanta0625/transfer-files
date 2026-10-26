//! Verified, resumable SFTP on each SSH edge, without TCP or agent forwarding.
use super::{Answer, Prompt, operations, terminal};
use crate::config::{RemoteConfig, RemoteOs, SshNode};
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
struct Location {
    node: Option<usize>,
    path: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Item {
    path: String,
    directory: bool,
    size: u64,
    hash: String,
}

// Identical tabs must not write the same resumable staging files concurrently.
static ACTIVE_TASKS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();
struct TaskClaim(String);
impl TaskClaim {
    fn acquire(id: &str) -> io::Result<Self> {
        let mut active = ACTIVE_TASKS.get_or_init(Default::default).lock().unwrap();
        if !active.insert(id.into()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "另一个标签正在传输相同的源和目标，请等待其完成",
            ));
        }
        Ok(Self(id.into()))
    }
}
impl Drop for TaskClaim {
    fn drop(&mut self) {
        ACTIVE_TASKS.get().unwrap().lock().unwrap().remove(&self.0);
    }
}
struct Session<'a, F> {
    nodes: Vec<SshNode>,
    cancel: &'a AtomicBool,
    ask: F,
    token: String,
}
impl<F: FnMut(Prompt) -> io::Result<Answer>> Session<'_, F> {
    fn rpc(&mut self, node: Option<usize>, request: Value) -> io::Result<Value> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "传输已取消"));
        }
        let mut cmd = if let Some(index) = node {
            let script = operations::command(self.nodes[index].options.os, &request, &self.token);
            terminal::chain_command(
                &self.nodes[..=index],
                &script,
                &self.token,
                request["op"] == "batch",
            )
        } else {
            let mut cmd = CommandBuilder::new("python3");
            cmd.args([
                "-c",
                &operations::script(RemoteOs::Linux, &request, &self.token),
            ]);
            cmd.env("LC_ALL", "C");
            cmd.env("SSH_ASKPASS_REQUIRE", "never");
            cmd
        };
        let mut sequence: Vec<String> = node
            .map(|i| self.nodes[1..=i].iter().map(SshNode::label).collect())
            .unwrap_or_default();
        if let Some(label) = request["auth_node"].as_str() {
            sequence.push(label.into());
        }
        cmd.env(
            "TF_AUTH_SEQUENCE",
            serde_json::to_string(&sequence).unwrap(),
        );
        // Large-file hashing may be silent; cancellation is still polled every 50ms.
        let output = terminal::run_command(
            cmd,
            &self.token,
            self.cancel,
            &mut self.ask,
            Duration::from_secs(1800),
        )?;
        operations::parse(&output, &self.token)
    }
    fn manifest(&mut self, location: &Location) -> io::Result<Vec<Item>> {
        let value = self.rpc(
            location.node,
            json!({"op":"manifest", "path":location.path}),
        )?;
        let mut items: Vec<Item> = serde_json::from_value(value).map_err(io::Error::other)?;
        items.sort_by(|a, b| a.path.cmp(&b.path));
        if items.first().is_none_or(|i| !i.path.is_empty()) {
            return Err(io::Error::other("无效的源文件清单"));
        }
        for item in &items {
            if !item.path.is_empty() {
                for component in item.path.split('/') {
                    valid_name(component)?;
                }
            }
        }
        Ok(items)
    }
    fn hash(&mut self, location: &Location, limit: Option<u64>) -> io::Result<Value> {
        self.rpc(
            location.node,
            json!({"op":"hash", "path":location.path, "limit":limit}),
        )
    }
    fn batch(&mut self, runner: Option<usize>, remote: usize, batch: &str) -> io::Result<()> {
        let node = &self.nodes[remote];
        let mut args: Vec<String> = terminal::OPTIONS.iter().map(|v| v.to_string()).collect();
        args.extend(terminal::identity_args(node));
        args.extend([
            "-P".into(),
            node.port.clone(),
            "-o".into(),
            format!("User={}", node.user),
        ]);
        let host = if node.host.contains(':') && !node.host.starts_with('[') {
            format!("[{}]", node.host)
        } else {
            node.host.clone()
        };
        self.rpc(runner, json!({"op":"batch", "args":args, "host":host, "batch":batch, "marker":terminal::marker(node, &self.token), "auth_node":node.label()}))?;
        Ok(())
    }
    fn edge(&mut self, source: &Location, target: &Location, items: &[Item]) -> io::Result<()> {
        let download = match (source.node, target.node) {
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(a), Some(b)) => a > b,
            _ => return Err(io::Error::other("无效的 SSH 传输路径")),
        };
        let (runner, remote) = if download {
            (target.node, source.node.unwrap())
        } else {
            (source.node, target.node.unwrap())
        };
        let mut batch = String::new();
        for item in items {
            let src = Location {
                node: source.node,
                path: join(&source.path, &item.path),
            };
            let dst = Location {
                node: target.node,
                path: join(&target.path, &item.path),
            };
            if item.directory {
                self.rpc(dst.node, json!({"op":"mkdir", "path":dst.path}))?;
                continue;
            }
            let existing = self.hash(&dst, None)?;
            let mut resume = false;
            if !existing.is_null() {
                let size = existing["size"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("无效的续传大小"))?;
                if size > item.size {
                    return Err(io::Error::other(
                        "暂存文件大于源文件；请清理该任务暂存后重试",
                    ));
                }
                let prefix = self.hash(&src, Some(size))?;
                if prefix != existing {
                    return Err(io::Error::other(
                        "续传前缀 SHA-256 不匹配，已停止；请清理该任务暂存后重试",
                    ));
                }
                if size == item.size && existing["hash"] == item.hash {
                    continue;
                }
                resume = true;
            }
            let operation = match (download, resume) {
                (true, true) => "reget",
                (true, false) => "get",
                (false, true) => "reput",
                (false, false) => "put",
            };
            batch.push_str(&format!(
                "{operation} {} {}\n",
                sftp_quote(&src.path)?,
                sftp_quote(&dst.path)?
            ));
            if batch.len() >= 2048 {
                self.batch(runner, remote, &batch)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            self.batch(runner, remote, &batch)?;
        }
        if self.manifest(target)? != items {
            return Err(io::Error::other(
                "传输后 SHA-256 或目录清单不一致，未发布目标",
            ));
        }
        Ok(())
    }
}
fn valid_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(io::Error::other("文件名包含不支持的分隔符或控制字符"));
    }
    Ok(())
}
fn join(base: &str, relative: &str) -> String {
    if relative.is_empty() {
        base.into()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), relative)
    }
}
fn parent(path: &str) -> io::Result<&str> {
    let (parent, _) = path
        .rsplit_once('/')
        .ok_or_else(|| io::Error::other("请使用绝对目标路径"))?;
    Ok(if parent.is_empty() {
        "/"
    } else if parent.len() == 2 && parent.ends_with(':') {
        &path[..3]
    } else {
        parent
    })
}
fn sftp_quote(path: &str) -> io::Result<String> {
    if path.chars().any(char::is_control) {
        return Err(io::Error::other("SFTP 路径不能包含控制字符"));
    }
    let mut escaped = String::from("\"");
    for c in path.chars() {
        if matches!(c, '\\' | '"') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped.push('"');
    Ok(escaped)
}
fn transfer(
    config: &RemoteConfig,
    local: &Path,
    remote: &str,
    download: bool,
    cancel: &AtomicBool,
    ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<()> {
    config.validate().map_err(io::Error::other)?;
    let nodes = config.nodes().map_err(io::Error::other)?;
    let last = nodes.len() - 1;
    let remote = if config.target_options.os == RemoteOs::Windows {
        remote.replace('\\', "/")
    } else {
        remote.into()
    };
    let local = local
        .to_str()
        .ok_or_else(|| io::Error::other("仅支持 UTF-8 路径"))?
        .to_string();
    let local = Location {
        node: None,
        path: local,
    };
    let remote = Location {
        node: Some(last),
        path: remote,
    };
    let (source, destination) = if download {
        (remote, local)
    } else {
        (local, remote)
    };
    let token = format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let mut session = Session {
        nodes,
        cancel,
        ask,
        token,
    };
    let items = session.manifest(&source)?;
    if session.rpc(
        destination.node,
        json!({"op":"exists", "path":destination.path}),
    )? == true
    {
        if session.manifest(&destination)? == items {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "目标已存在且内容不同；不会覆盖或合并，请更换目标目录",
        ));
    }
    // Content + route + endpoints identify an interrupted task even after restart.
    let id = format!("{:x}", Sha256::digest(serde_json::to_vec(&json!({
        "version":1, "source":source.path, "destination":destination.path, "download":download,
        "nodes":session.nodes.iter().map(|n| (n.label(), n.options.os)).collect::<Vec<_>>(), "items":items
    })).unwrap()));
    let _claim = TaskClaim::acquire(&id)?;
    let order: Vec<Option<usize>> = if download {
        (0..last).rev().map(Some).chain([None]).collect()
    } else {
        (0..=last).map(Some).collect()
    };
    let mut staging = Vec::new();
    let result: io::Result<()> = (|| {
        let mut previous = source;
        for node in order {
            let base = if node == destination.node {
                Some(parent(&destination.path)?)
            } else {
                None
            };
            let value = session.rpc(node, json!({"op":"prepare", "base":base, "id":id}))?;
            let directory = value
                .as_str()
                .ok_or_else(|| io::Error::other("无效的暂存目录"))?
                .to_string();
            staging.push(Location {
                node,
                path: directory.clone(),
            });
            let next = Location {
                node,
                path: join(&directory, "payload"),
            };
            session.edge(&previous, &next, &items)?;
            previous = next;
        }
        session.rpc(
            destination.node,
            json!({"op":"publish", "source":previous.path, "target":destination.path}),
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        let paths = staging
            .iter()
            .map(|l| {
                format!(
                    "{}: {}",
                    l.node
                        .map(|i| session.nodes[i].label())
                        .unwrap_or("本地".into()),
                    l.path
                )
            })
            .collect::<Vec<_>>()
            .join("；");
        return Err(io::Error::new(
            error.kind(),
            format!("{error}；再次选择相同源和目标即可校验续传。保留暂存：{paths}"),
        ));
    }
    for location in staging.iter().rev() {
        session
            .rpc(location.node, json!({"op":"remove", "path":location.path}))
            .map_err(|e| {
                io::Error::other(format!(
                    "文件已送达，但暂存清理未完成：{}；{e}",
                    location.path
                ))
            })?;
    }
    Ok(())
}
pub fn upload(
    config: &RemoteConfig,
    source: &Path,
    target_dir: &str,
    cancel: &AtomicBool,
    ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<()> {
    let source = std::path::absolute(source)?;
    let name = source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("源路径没有有效文件名"))?;
    valid_name(name)?;
    let target_dir = if config.target_options.os == RemoteOs::Windows {
        target_dir.replace('\\', "/")
    } else {
        target_dir.into()
    };
    transfer(
        config,
        &source,
        &join(&target_dir, name),
        false,
        cancel,
        ask,
    )
}
pub fn download(
    config: &RemoteConfig,
    source: &str,
    name: &str,
    target_dir: &Path,
    cancel: &AtomicBool,
    ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<()> {
    valid_name(name)?;
    transfer(
        config,
        &std::path::absolute(target_dir)?.join(name),
        source,
        true,
        cancel,
        ask,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identical_tasks_are_excluded_until_the_first_exits() {
        let first = TaskClaim::acquire("test-concurrent-transfer").unwrap();
        assert!(TaskClaim::acquire("test-concurrent-transfer").is_err());
        let other = TaskClaim::acquire("test-other-transfer").unwrap();
        drop(first);
        assert!(TaskClaim::acquire("test-concurrent-transfer").is_ok());
        drop(other);
    }
    #[test]
    fn rejects_traversal_and_batch_injection() {
        for name in ["", ".", "..", "../x", "x\\y", "x\nput secret"] {
            assert!(valid_name(name).is_err());
        }
        assert!(sftp_quote("a\n!echo bad").is_err());
        assert_eq!(sftp_quote("/tmp/a b\"c").unwrap(), "\"/tmp/a b\\\"c\"");
    }

    /// Run using scripts/test-transfer.sh, with real SFTP and a local SSH transport fixture.
    #[test]
    #[ignore = "requires isolated SSH fixture and an OpenSSH sftp-server executable"]
    fn real_sftp_resume_and_multihop_roundtrip() {
        assert!(std::env::var_os("TF_TEST_SFTP_SERVER").is_some());
        let temp = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for count in [1, 2, 4] {
            let config = RemoteConfig {
                route: Some(
                    (0..count - 1)
                        .map(|i| format!("hop{i}.test 22 user"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                windows_host: "target.test".into(),
                windows_port: "22".into(),
                windows_user: "user".into(),
                windows_root: temp.path().display().to_string(),
                target_options: crate::config::NodeOptions::linux(),
                ..Default::default()
            };
            let source = temp.path().join(format!("source-{count}"));
            let remote = temp.path().join(format!("remote-{count}"));
            let downloaded = temp.path().join(format!("downloaded-{count}"));
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(source.join("空目录")).unwrap();
            std::fs::write(source.join("a [b]*? ' \"文件.txt"), b"abcdef").unwrap();
            std::fs::write(source.join("empty.txt"), b"").unwrap();
            let ask = |_| panic!("Unexpected authentication in transport fixture");
            transfer(
                &config,
                &source,
                remote.to_str().unwrap(),
                false,
                &cancel,
                ask,
            )
            .unwrap();
            transfer(
                &config,
                &downloaded,
                remote.to_str().unwrap(),
                true,
                &cancel,
                ask,
            )
            .unwrap();
            assert_eq!(
                std::fs::read(downloaded.join("a [b]*? ' \"文件.txt")).unwrap(),
                b"abcdef"
            );
            assert!(downloaded.join("空目录").is_dir());
            std::fs::write(downloaded.join("empty.txt"), "keep").unwrap();
            assert!(
                transfer(
                    &config,
                    &downloaded,
                    remote.to_str().unwrap(),
                    true,
                    &cancel,
                    ask
                )
                .is_err()
            );
            assert_eq!(
                std::fs::read(downloaded.join("empty.txt")).unwrap(),
                b"keep"
            );

            let nodes = config.nodes().unwrap();
            let mut session = Session {
                nodes,
                cancel: &cancel,
                ask,
                token: "resume-test".into(),
            };
            let src = Location {
                node: None,
                path: source.join("a [b]*? ' \"文件.txt").display().to_string(),
            };
            let dst = Location {
                node: Some(0),
                path: temp
                    .path()
                    .join(format!("partial-{count}"))
                    .display()
                    .to_string(),
            };
            let items = session.manifest(&src).unwrap();
            std::fs::write(&dst.path, b"abc").unwrap();
            session.edge(&src, &dst, &items).unwrap();
            assert_eq!(std::fs::read(&dst.path).unwrap(), b"abcdef");
            // Reverse direction uses reget, and validates its prefix too.
            let receive = Location {
                node: None,
                path: temp
                    .path()
                    .join(format!("receive-{count}"))
                    .display()
                    .to_string(),
            };
            std::fs::write(&receive.path, b"ab").unwrap();
            session.edge(&dst, &receive, &items).unwrap();
            assert_eq!(std::fs::read(&receive.path).unwrap(), b"abcdef");
            std::fs::write(&dst.path, b"BAD").unwrap();
            assert!(
                session
                    .edge(&src, &dst, &items)
                    .unwrap_err()
                    .to_string()
                    .contains("SHA-256")
            );
            assert_eq!(std::fs::read(&dst.path).unwrap(), b"BAD");
            let link = temp.path().join(format!("link-{count}"));
            std::os::unix::fs::symlink(&src.path, &link).unwrap();
            assert!(
                session
                    .manifest(&Location {
                        node: None,
                        path: link.display().to_string()
                    })
                    .is_err()
            );
        }
        // Two independent tab workers can run the real SFTP engine concurrently.
        let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for index in 0..2 {
            let source = temp.path().join(format!("parallel-source-{index}"));
            let target = temp.path().join(format!("parallel-target-{index}"));
            std::fs::write(&source, vec![index as u8; 128 * 1024]).unwrap();
            let config = RemoteConfig {
                route: Some("parallel-hop.test 22 user".into()),
                windows_host: format!("parallel-target-{index}.test"),
                windows_port: "22".into(),
                windows_user: "user".into(),
                windows_root: temp.path().display().to_string(),
                target_options: crate::config::NodeOptions::linux(),
                ..Default::default()
            };
            let gate = gate.clone();
            workers.push(std::thread::spawn(move || {
                gate.wait();
                transfer(
                    &config,
                    &source,
                    target.to_str().unwrap(),
                    false,
                    &AtomicBool::new(false),
                    |_| panic!("unexpected authentication in fixture"),
                )
                .unwrap();
                assert_eq!(
                    std::fs::read(source).unwrap(),
                    std::fs::read(target).unwrap()
                );
            }));
        }
        gate.wait();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
