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

fn jump(config: &RemoteConfig, script: &str) -> CommandBuilder {
    let mut c = CommandBuilder::new("ssh");
    c.args(["-tt", "-e", "none"]);
    c.args(OPTIONS);
    c.args([
        "-p",
        &config.jump_port,
        "-l",
        &config.jump_user,
        "--",
        &config.jump_host,
        script,
    ]);
    environment(&mut c);
    c
}

fn local_scp(config: &RemoteConfig, local: &Path, staging: &str, download: bool) -> CommandBuilder {
    let remote = endpoint(&config.jump_user, &config.jump_host, staging);
    let local = local.to_string_lossy();
    let (source, target) = if download {
        (remote.as_str(), local.as_ref())
    } else {
        (local.as_ref(), remote.as_str())
    };
    let mut c = CommandBuilder::new("scp");
    c.args(scp_args(&config.jump_port, source, target));
    environment(&mut c);
    c
}

fn nested_scp(
    config: &RemoteConfig,
    windows: &str,
    staging: &str,
    id: &str,
    download: bool,
) -> CommandBuilder {
    // Windows FullName uses backslashes. SCP/SFTP expects forward slashes.
    let windows = endpoint(
        &config.windows_user,
        &config.windows_host,
        &windows.replace('\\', "/"),
    );
    let (source, target) = if download {
        (windows.as_str(), staging)
    } else {
        (staging, windows.as_str())
    };
    let args = scp_args(&config.windows_port, source, target);
    // Quote individual arguments, never the entire command as one executable.
    jump(
        config,
        &format!(
            "stty -echo; printf '\\nTF_HOP2_{id}\\n'; exec env LC_ALL=C SSH_ASKPASS_REQUIRE=never scp {}",
            args.iter()
                .map(|arg| quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        ),
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
    let id = token();
    let directory = format!("/tmp/transfer-files-{id}");
    let payload = format!("{directory}/payload");
    // mkdir must succeed before any transfer; do not reuse an existing directory.
    execute(
        jump(
            config,
            &format!("umask 077; mkdir -- {}", quote(&directory)),
        ),
        &id,
    )
    .map_err(|e| io::Error::new(e.kind(), format!("创建跳板机暂存目录失败：{e}")))?;
    let stages = if download {
        [
            (
                nested_scp(config, windows, &payload, &id, true),
                "下载第 1 段（Windows → 跳板机）",
            ),
            (
                local_scp(config, local, &payload, true),
                "下载第 2 段（跳板机 → 本地）",
            ),
        ]
    } else {
        [
            (
                local_scp(config, local, &payload, false),
                "上传第 1 段（本地 → 跳板机）",
            ),
            (
                nested_scp(config, windows, &payload, &id, false),
                "上传第 2 段（跳板机 → Windows）",
            ),
        ]
    };
    for (cmd, stage) in stages {
        execute(cmd, &id).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{stage}失败：{e}；暂存可能保留在跳板机 {directory}"),
            )
        })?;
    }
    // The payload must survive BOTH hops. Delete only our successfully created
    // staging directory, never a user-supplied source or destination path.
    execute(
        jump(config, &format!("rm -rf -- {}", quote(&directory))),
        &id,
    )
    .map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("文件已传输，但跳板机暂存清理失败：{directory}；{e}"),
        )
    })?;
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

    fn config() -> RemoteConfig {
        RemoteConfig {
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
            assert!(stdout.starts_with("\nTF_HOP2_test\n"));
            let args = stdout
                .trim_start_matches("\nTF_HOP2_test\n")
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
