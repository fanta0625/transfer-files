//! Two ordinary SCP hops, with a private staging directory on the jump host.
use super::{
    Answer, Prompt,
    terminal::{OPTIONS, quote, run_command},
};
use crate::config::RemoteConfig;
use portable_pty::CommandBuilder;
use std::{
    io,
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn token() -> String {
    format!(
        "tf-{}-{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

fn environment(c: &mut CommandBuilder) {
    c.env("LC_ALL", "C");
    c.env("SSH_ASKPASS_REQUIRE", "never");
    c.env_remove("SSH_ASKPASS");
}

fn endpoint(user: &str, host: &str, path: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("{user}@{host}:{path}")
}

fn scp_args(port: &str, source: &str, target: &str) -> Vec<String> {
    let mut args = vec!["-r".into()];
    args.extend(OPTIONS.iter().map(|v| v.to_string()));
    args.extend([
        "-P".into(),
        port.into(),
        "--".into(),
        source.into(),
        target.into(),
    ]);
    args
}

fn local_scp_node(
    node: &crate::config::SshNode,
    local: &Path,
    remote_path: &str,
    download: bool,
) -> CommandBuilder {
    let remote = endpoint(&node.user, &node.host, remote_path);
    let local = local.to_string_lossy();
    let (source, target) = if download {
        (remote.as_str(), local.as_ref())
    } else {
        (local.as_ref(), remote.as_str())
    };
    let mut c = CommandBuilder::new("scp");
    c.args(scp_args(&node.port, source, target));
    environment(&mut c);
    c.env("TF_NODE_LABEL", node.label());
    c
}
fn edge_scp(
    nodes: &[crate::config::SshNode],
    index: usize,
    remote_path: &str,
    staging: &str,
    id: &str,
    download: bool,
) -> CommandBuilder {
    let next = &nodes[index + 1];
    let remote = endpoint(&next.user, &next.host, remote_path);
    let (source, target) = if download {
        (remote.as_str(), staging)
    } else {
        (staging, remote.as_str())
    };
    let script = format!(
        "stty -echo; printf '%s\\n' {}; exec env LC_ALL=C SSH_ASKPASS_REQUIRE=never scp {}",
        quote(&super::terminal::marker(next, id)),
        scp_args(&next.port, source, target)
            .iter()
            .map(|v| quote(v))
            .collect::<Vec<_>>()
            .join(" ")
    );
    super::terminal::chain_command(&nodes[..=index], &script, id, true)
}
#[cfg(test)]
fn nested_scp(
    config: &RemoteConfig,
    windows: &str,
    staging: &str,
    id: &str,
    download: bool,
) -> CommandBuilder {
    edge_scp(
        &config.nodes().unwrap(),
        0,
        &windows.replace('\\', "/"),
        staging,
        id,
        download,
    )
}
fn transfer(
    config: &RemoteConfig,
    local: &Path,
    windows: &str,
    download: bool,
    mut execute: impl FnMut(CommandBuilder, &str) -> io::Result<String>,
) -> io::Result<()> {
    config.validate().map_err(io::Error::other)?;
    let nodes = config.nodes().map_err(io::Error::other)?;
    let id = token();
    let windows = windows.replace('\\', "/");
    if nodes.len() == 1 {
        execute(local_scp_node(&nodes[0], local, &windows, download), &id)?;
        return Ok(());
    }
    let directory = format!("/tmp/transfer-files-{id}");
    let payload = format!("{directory}/payload");
    let intermediates = nodes.len() - 1;
    for index in 0..intermediates {
        let cmd = super::terminal::chain_command(
            &nodes[..=index],
            &format!("umask 077; mkdir -- {}", quote(&directory)),
            &id,
            true,
        );
        execute(cmd, &id).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "{} 创建暂存目录失败：{e}；此前节点可能保留 {directory}",
                    nodes[index].label()
                ),
            )
        })?;
    }
    let mut stages = Vec::new();
    if !download {
        stages.push((
            local_scp_node(&nodes[0], local, &payload, false),
            format!("本地 → {}", nodes[0].label()),
        ));
    }
    let indices: Vec<_> = if download {
        (0..intermediates).rev().collect()
    } else {
        (0..intermediates).collect()
    };
    for index in indices {
        let remote_path = if index == intermediates - 1 {
            &windows
        } else {
            &payload
        };
        let stage = if download {
            format!("{} → {}", nodes[index + 1].label(), nodes[index].label())
        } else {
            format!("{} → {}", nodes[index].label(), nodes[index + 1].label())
        };
        stages.push((
            edge_scp(&nodes, index, remote_path, &payload, &id, download),
            stage,
        ));
    }
    if download {
        stages.push((
            local_scp_node(&nodes[0], local, &payload, true),
            format!("{} → 本地", nodes[0].label()),
        ));
    }
    for (cmd, stage) in stages {
        execute(cmd, &id).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{stage} 失败：{e}；中转节点可能保留 {directory}"),
            )
        })?;
    }
    for index in (0..intermediates).rev() {
        let cmd = super::terminal::chain_command(
            &nodes[..=index],
            &format!("rm -rf -- {}", quote(&directory)),
            &id,
            true,
        );
        execute(cmd, &id).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "传输完成，但 {} 暂存清理失败：{directory}；{e}",
                    nodes[index].label()
                ),
            )
        })?;
    }
    Ok(())
}

pub fn upload(
    config: &RemoteConfig,
    source: &Path,
    target_dir: &str,
    cancel: &AtomicBool,
    mut ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<()> {
    let source = source.canonicalize()?;
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::other("源路径没有文件名"))?
        .to_string_lossy();
    let target = format!(
        "{}/{}",
        target_dir.replace('\\', "/").trim_end_matches('/'),
        name
    );
    transfer(config, &source, &target, false, |cmd, id| {
        run_command(cmd, id, cancel, &mut ask, Duration::from_secs(60))
    })
}

pub fn download(
    config: &RemoteConfig,
    source: &str,
    name: &str,
    target_dir: &Path,
    cancel: &AtomicBool,
    mut ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(io::Error::other("无效的远程文件名"));
    }
    let target = target_dir.canonicalize()?.join(name);
    // Do not let SCP overwrite existing local files or merge directories.
    if target.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "本地存在同名项目，请更换目标目录",
        ));
    }
    transfer(config, &target, source, true, |cmd, id| {
        run_command(cmd, id, cancel, &mut ask, Duration::from_secs(60))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_and_recursive_transfer_plans_use_every_node_in_order() {
        for download in [true, false] {
            for count in [1, 2, 4] {
                let mut config = config();
                config.route = Some(
                    (0..count - 1)
                        .map(|i| format!("hop{i}.test {} user{i}", 2200 + i))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                let mut calls = Vec::new();
                transfer(
                    &config,
                    Path::new("/tmp/item"),
                    "C:/item",
                    download,
                    |cmd, _| {
                        calls.push(cmd.get_argv().clone());
                        Ok(String::new())
                    },
                )
                .unwrap();
                if count == 1 {
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0][0], "scp");
                    let remote = endpoint(&config.windows_user, &config.windows_host, "C:/item");
                    assert_eq!(
                        calls[0][calls[0].len() - if download { 2 } else { 1 }],
                        remote.as_str()
                    );
                } else {
                    assert_eq!(calls.len(), count + 2 * (count - 1));
                    // Every edge transfers exactly once. The outer SCP is first
                    // when uploading and last when downloading.
                    let first_transfer = count - 1;
                    let local_index = first_transfer + if download { count - 1 } else { 0 };
                    assert_eq!(calls[local_index][0], "scp");
                    assert!(
                        calls[first_transfer..first_transfer + count]
                            .iter()
                            .all(|args| !args.last().unwrap().to_string_lossy().contains("rm -rf"))
                    );
                }
            }
        }
    }

    fn config() -> RemoteConfig {
        RemoteConfig {
            route: None,
            jump_host: "jump.test".into(),
            jump_port: "2201".into(),
            jump_user: "alice".into(),
            windows_host: "100.64.0.2".into(),
            windows_port: "2202".into(),
            windows_user: "home user".into(),
            windows_root: "C:/Users/home".into(),
        }
    }

    #[test]
    fn nested_commands_execute_with_correct_direction_ports_and_literal_paths() {
        let config = config();
        for download in [true, false] {
            let cmd = nested_scp(
                &config,
                r"C:\Users\home\资料 ' $(exit 81).txt",
                "/tmp/stage/payload",
                "test",
                download,
            );
            let remote_script = cmd.get_argv().last().unwrap().to_str().unwrap();
            // A real shell and a fake scp executable: the old exec 'scp ...'
            // fails here, as does reversed direction or broken argument quoting.
            let temp = tempfile::tempdir().unwrap();
            let executable = temp.path().join("scp");
            std::fs::write(&executable, "#!/bin/sh\nprintf '%s\\0' \"$@\"\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            let output = std::process::Command::new("sh")
                .args(["-c", remote_script])
                .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            let stdout = String::from_utf8(output.stdout).unwrap();
            let marker = format!(
                "{}\n",
                super::super::terminal::marker(&config.nodes().unwrap()[1], "test")
            );
            assert!(stdout.starts_with(&marker));
            let args = stdout
                .trim_start_matches(&marker)
                .split('\0')
                .collect::<Vec<_>>();
            assert!(args.windows(2).any(|v| v == ["-P", "2202"]));
            let endpoints = &args[args.len() - 3..args.len() - 1];
            let windows = "home user@100.64.0.2:C:/Users/home/资料 ' $(exit 81).txt";
            assert_eq!(
                endpoints,
                if download {
                    [windows, "/tmp/stage/payload"]
                } else {
                    ["/tmp/stage/payload", windows]
                }
            );
            assert!(!remote_script.contains("rm -rf"));
        }
    }

    #[test]
    fn both_hops_complete_before_cleanup_and_failures_keep_staging() {
        for download in [true, false] {
            for fail_at in [None, Some(1), Some(2)] {
                let mut calls = Vec::new();
                let result = transfer(
                    &config(),
                    Path::new("/tmp/local item"),
                    "C:/文件.txt",
                    download,
                    |cmd, _| {
                        calls.push(cmd.get_argv().clone());
                        if Some(calls.len() - 1) == fail_at {
                            Err(io::Error::other("simulated failure"))
                        } else {
                            Ok(String::new())
                        }
                    },
                );
                assert_eq!(result.is_ok(), fail_at.is_none());
                assert!(
                    calls[0]
                        .last()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with("umask 077; mkdir -- ")
                );
                if let Some(index) = fail_at {
                    assert_eq!(calls.len(), index + 1);
                    assert!(
                        !calls.iter().any(|a| a
                            .last()
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .starts_with("rm "))
                    );
                } else {
                    assert_eq!(calls.len(), 4);
                    assert!(
                        calls[3]
                            .last()
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .starts_with("rm -rf -- '/tmp/transfer-files-tf-")
                    );
                    let local = &calls[if download { 2 } else { 1 }];
                    assert!(local.windows(2).any(|v| v[0] == "-P" && v[1] == "2201"));
                    assert_eq!(
                        local[local.len() - if download { 1 } else { 2 }],
                        "/tmp/local item"
                    );
                }
            }
        }
    }

    #[test]
    fn file_and_directory_roundtrip_through_real_shell_and_staging() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let windows = temp.path().join("windows");
        let local = temp.path().join("local");
        let bin = temp.path().join("bin");
        for dir in [&windows, &local, &bin] {
            std::fs::create_dir(dir).unwrap();
        }
        // Replace network transport only. Execute the generated shell commands
        // and copy actual bytes/trees, including the real staging cleanup.
        for (name, script) in [
            (
                "ssh",
                "#!/usr/bin/python3\nimport os,sys\nos.execv('/bin/sh', ['sh','-c',sys.argv[-1]])\n",
            ),
            (
                "scp",
                r#"#!/usr/bin/python3
import os,sys,shutil
def path(value):
    if '@' in value and ':' in value:
        host, value = value.split(':', 1)
        if '100.64.0.2' in host:
            assert value.startswith('C:/')
            value = os.path.join(os.environ['TF_TEST_WINDOWS'], value[3:])
    return value
src,dst = map(path, sys.argv[-2:])
assert os.path.exists(src), src
assert not os.path.exists(dst), dst
if os.path.isdir(src): shutil.copytree(src,dst)
else: shutil.copyfile(src,dst)
"#,
            ),
        ] {
            let path = bin.join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for directory in [false, true] {
            let name = if directory {
                "目录 ' 空格"
            } else {
                "文件 ' 空格.txt"
            };
            let source = local.join(name);
            if directory {
                std::fs::create_dir_all(source.join("空目录")).unwrap();
                std::fs::write(source.join("内容.txt"), "往返内容").unwrap();
            } else {
                std::fs::write(&source, "往返内容").unwrap();
            }
            let remote = format!("C:/{name}");
            let destination = local.join(format!("下载-{name}"));
            let mut staging = Vec::new();
            for (download, local_path) in [(false, &source), (true, &destination)] {
                transfer(&config(), local_path, &remote, download, |mut cmd, id| {
                    let program = cmd.get_argv()[0].clone();
                    cmd.get_argv_mut()[0] = bin.join(program).into_os_string();
                    cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
                    cmd.env("TF_TEST_WINDOWS", &windows);
                    staging.push(format!("/tmp/transfer-files-{id}"));
                    run_command(
                        cmd,
                        id,
                        &AtomicBool::new(false),
                        |_| panic!("unexpected authentication"),
                        Duration::from_secs(5),
                    )
                })
                .unwrap();
            }
            let downloaded_file = if directory {
                destination.join("内容.txt")
            } else {
                destination.clone()
            };
            assert_eq!(
                std::fs::read(downloaded_file).unwrap(),
                "往返内容".as_bytes()
            );
            if directory {
                assert!(destination.join("空目录").is_dir());
            }
            assert!(source.exists(), "source must be preserved");
            assert!(
                windows.join(name).exists(),
                "Windows source must be preserved"
            );
            assert!(staging.iter().all(|path| !Path::new(path).exists()));
        }
    }
}
