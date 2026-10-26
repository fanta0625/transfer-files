//! PTY adapter: OpenSSH reads passwords from a controlling terminal, never argv.
use super::shell::invoke;
pub(crate) use super::shell::{identity_args, quote};
use super::{Answer, Listing, Prompt};
use crate::config::{RemoteConfig, SshNode};
use portable_pty::{CommandBuilder, PtySize};
use std::{
    io::{self, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

pub(crate) const OPTIONS: &[&str] = &[
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ConnectionAttempts=1",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=2",
    "-o",
    "StrictHostKeyChecking=ask",
    "-o",
    "NumberOfPasswordPrompts=1",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "ProxyCommand=none",
    "-o",
    "ProxyJump=none",
    "-o",
    "PreferredAuthentications=publickey,password",
    "-o",
    "BatchMode=no",
    "-o",
    "LogLevel=ERROR",
];
const LIMIT: usize = 16 * 1024 * 1024;

/// ConPTY on Windows hops adds CSI/OSC sequences, sometimes across read boundaries.
#[derive(Default)]
struct TerminalText {
    escape: u8,
}
impl TerminalText {
    fn push(&mut self, input: &[u8]) -> String {
        let mut plain = Vec::with_capacity(input.len());
        for &byte in input {
            match self.escape {
                0 if byte == 27 => self.escape = 1,
                0 => {
                    if byte >= 32 || matches!(byte, b'\r' | b'\n' | b'\t') {
                        plain.push(byte);
                    }
                }
                1 => {
                    self.escape = match byte {
                        b'[' => 2,
                        b']' => 3,
                        _ => 0,
                    }
                }
                2 => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape = 0;
                    }
                }
                3 => {
                    if byte == 7 {
                        self.escape = 0;
                    } else if byte == 27 {
                        self.escape = 4;
                    }
                }
                _ => self.escape = if byte == b'\\' { 0 } else { 3 },
            }
        }
        String::from_utf8_lossy(&plain).into_owned()
    }
}

pub(crate) fn marker(node: &crate::config::SshNode, token: &str) -> String {
    use base64::Engine as _;
    format!(
        "TF_NODE_{token}:{}",
        base64::engine::general_purpose::STANDARD.encode(node.label())
    )
}
pub(crate) fn chain_command(
    nodes: &[SshNode],
    script: &str,
    token: &str,
    tty: bool,
) -> CommandBuilder {
    assert!(!nodes.is_empty());
    let mut script = script.to_string();
    for index in (1..nodes.len()).rev() {
        let node = &nodes[index];
        let mut args = vec![
            "ssh".into(),
            if index == nodes.len() - 1 && !tty {
                "-T"
            } else {
                "-tt"
            }
            .into(),
            "-e".into(),
            "none".into(),
        ];
        args.extend(OPTIONS.iter().map(|v| v.to_string()));
        args.extend(identity_args(node));
        args.extend([
            "-p".into(),
            node.port.clone(),
            "-l".into(),
            node.user.clone(),
            "--".into(),
            node.host.clone(),
            script,
        ]);
        script = invoke(nodes[index - 1].options.os, &args, &marker(node, token));
    }
    let first = &nodes[0];
    let mut cmd = CommandBuilder::new("ssh");
    cmd.args([
        if nodes.len() == 1 && !tty {
            "-T"
        } else {
            "-tt"
        },
        "-e",
        "none",
    ]);
    cmd.args(OPTIONS);
    cmd.args(identity_args(first));
    cmd.args([
        "-p",
        &first.port,
        "-l",
        &first.user,
        "--",
        &first.host,
        &script,
    ]);
    cmd.env("TF_NODE_LABEL", first.label());
    cmd.env(
        "TF_AUTH_SEQUENCE",
        serde_json::to_string(&nodes[1..].iter().map(SshNode::label).collect::<Vec<_>>()).unwrap(),
    );
    cmd.env("LC_ALL", "C");
    cmd.env("SSH_ASKPASS_REQUIRE", "never");
    cmd.env_remove("SSH_ASKPASS");
    cmd
}
fn command(config: &RemoteConfig, path: &str, token: &str) -> CommandBuilder {
    let script = super::operations::command(
        config.target_options.os,
        &serde_json::json!({"op":"list", "path":path}),
        token,
    );
    chain_command(
        &config.nodes().expect("validated configuration"),
        &script,
        token,
        false,
    )
}

pub fn read_directory(
    config: &RemoteConfig,
    path: &str,
    cancel: &AtomicBool,
    ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<Listing> {
    let token = format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let output = run_command(
        command(config, path, &token),
        &token,
        cancel,
        ask,
        Duration::from_secs(60),
    )?;
    super::operations::listing(&output, &token)
}

struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn run_command(
    cmd: CommandBuilder,
    token: &str,
    cancel: &AtomicBool,
    mut ask: impl FnMut(Prompt) -> io::Result<Answer>,
    timeout: Duration,
) -> io::Result<String> {
    let initial_hop = cmd
        .get_env("TF_NODE_LABEL")
        .and_then(|s| s.to_str())
        .unwrap_or("SSH")
        .to_string();
    let mut expected: std::collections::VecDeque<String> = cmd
        .get_env("TF_AUTH_SEQUENCE")
        .and_then(|s| s.to_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let enforce_sequence = cmd.get_env("TF_AUTH_SEQUENCE").is_some();
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 4096,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io::Error::other(format!("无法创建 SSH 终端：{e}")))?;
    let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
    let mut writer = pair.master.take_writer().map_err(io::Error::other)?;
    let mut child = ChildGuard(pair.slave.spawn_command(cmd).map_err(|e| {
        io::Error::other(format!(
            "无法启动命令，请确认已安装 OpenSSH 客户端和 Python 3：{e}"
        ))
    })?);
    drop(pair.slave);
    let (tx, rx) = mpsc::sync_channel(16);
    let reader_thread = thread::spawn(move || {
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if tx.send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
                // Linux PTYs return EIO after the slave is closed.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    let result = (|| {
        let mut output = Zeroizing::new(String::new());
        let mut prompt_buffer = String::new();
        let mut hop = initial_hop;
        let mut data_started = false;
        let mut idle = Instant::now();
        let mut attempts = 0;
        let mut terminal_text = TerminalText::default();
        let mut received = 0usize;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "远程读取已取消，SSH 已断开",
                ));
            }
            if idle.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "远程操作超时；检查网络、SSH 和远端运行环境",
                ));
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(bytes) => {
                    idle = Instant::now();
                    received += bytes.len();
                    if received > LIMIT {
                        return Err(io::Error::other("终端响应超过 16 MiB，请分批操作"));
                    }
                    let text = terminal_text.push(&bytes);
                    // Protocol and OpenSSH LC_ALL=C prompts are ASCII. File names
                    // and server-side errors are carried as base64 UTF-8.
                    output.push_str(&text);
                    if output.len() > LIMIT {
                        return Err(io::Error::other("目录响应超过 16 MiB，请选择更小的目录"));
                    }
                    prompt_buffer.push_str(&text);
                    let prefix = format!("TF_NODE_{token}:");
                    while let Some(start) = prompt_buffer.find(&prefix) {
                        let rest = &prompt_buffer[start + prefix.len()..];
                        let Some(end) = rest.find(['\r', '\n']) else {
                            break;
                        };
                        use base64::Engine as _;
                        if let Ok(bytes) =
                            base64::engine::general_purpose::STANDARD.decode(&rest[..end])
                            && let Ok(label) = String::from_utf8(bytes)
                        {
                            if enforce_sequence && expected.pop_front().as_ref() != Some(&label) {
                                return Err(io::Error::other(
                                    "远端返回非预期认证节点，已拒绝发送凭据",
                                ));
                            }
                            hop = label;
                            data_started = false;
                        }
                        prompt_buffer = rest[end + 1..].to_string();
                    }
                    if prompt_buffer.contains(&format!("TF_READY_{token}"))
                        || prompt_buffer.contains(&format!("TF_RPC_{token}_BEGIN"))
                    {
                        data_started = true;
                    }
                    if !data_started && let Some(host_key) = prompt_kind(&prompt_buffer) {
                        attempts += 1;
                        if attempts > 32 {
                            return Err(io::Error::other("SSH 认证请求过多，请检查认证配置"));
                        }
                        let prompt = if host_key {
                            Prompt::HostKey {
                                hop: hop.clone(),
                                details: fingerprint_details(&prompt_buffer),
                            }
                        } else {
                            if prompt_buffer
                                .trim_end()
                                .rsplit(['\r', '\n'])
                                .next()
                                .unwrap_or_default()
                                .to_ascii_lowercase()
                                .starts_with("enter passphrase for key ")
                            {
                                Prompt::KeyPassphrase {
                                    hop: hop.clone(),
                                    key: prompt_buffer
                                        .trim()
                                        .rsplit(['\r', '\n'])
                                        .next()
                                        .unwrap_or_default()
                                        .into(),
                                }
                            } else {
                                Prompt::Secret { hop: hop.clone() }
                            }
                        };
                        let answer = ask(prompt)?.ok_or_else(|| {
                            io::Error::new(io::ErrorKind::Interrupted, "已取消认证，SSH 已断开")
                        })?;
                        if answer.bytes().any(|b| b < 32 || b == 127) {
                            return Err(io::Error::other("当前终端认证不支持控制字符"));
                        }
                        if host_key && answer.as_str() != "yes" {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "未信任主机密钥，SSH 已断开",
                            ));
                        }
                        // OpenSSH disables terminal echo before asking. Do not
                        // keep or publish the terminal transcript of credentials.
                        writer.write_all(answer.as_bytes())?;
                        writer.write_all(b"\n")?;
                        writer.flush()?;
                        output.clear();
                        prompt_buffer.clear();
                        idle = Instant::now();
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = child.0.try_wait()?;
                    if let Some(status) = status {
                        if !status.success() {
                            // Decode a framed PowerShell error, otherwise offer
                            // useful OpenSSH diagnostics without a raw transcript.
                            if output.contains(&format!("TF_RPC_{token}_BEGIN")) {
                                return super::operations::parse(&output, token)
                                    .map(|_| String::new());
                            }
                            return Err(io::Error::other(ssh_error(&output, &hop)));
                        }
                        return Ok(output.to_string());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    })();
    // Kill/wait our own ssh before dropping the terminal and joining its reader.
    drop(child);
    drop(writer);
    drop(pair.master);
    drop(rx);
    let _ = reader_thread.join();
    result
}

fn prompt_kind(buffer: &str) -> Option<bool> {
    let tail = buffer.trim_end();
    if tail.ends_with("(yes/no/[fingerprint])?") || tail.ends_with("(yes/no)?") {
        return Some(true);
    }
    let line = tail
        .rsplit(['\r', '\n'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if line.ends_with("password:")
        || (line.starts_with("enter passphrase for key ") && line.ends_with(':'))
    {
        Some(false)
    } else {
        None
    }
}

fn fingerprint_details(buffer: &str) -> String {
    // Do not show arbitrary banners or previous secret prompts as host identity.
    buffer
        .lines()
        .filter(|line| {
            line.contains("authenticity of host")
                || line.contains("key fingerprint is")
                || line.contains("SHA256:")
                || line.contains("Are you sure")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ssh_error(output: &str, hop: &str) -> String {
    let lower = output.to_ascii_lowercase();
    let reason = if lower.contains("remote host identification has changed")
        || lower.contains("host key verification failed")
    {
        "主机密钥校验失败；请先核实该端 known_hosts 中的密钥"
    } else if lower.contains("permission denied") {
        "认证或文件权限不足；检查用户名、密码和源/目标目录权限"
    } else if lower.contains("no such file") || lower.contains("cannot stat") {
        "文件或目录不存在；刷新目录并检查路径"
    } else if lower.contains("no space left") {
        "磁盘空间不足；检查跳板机和目标磁盘"
    } else if lower.contains("subsystem request failed") {
        "SFTP 子系统不可用；检查该端 OpenSSH 的 SFTP 配置"
    } else if lower.contains("could not resolve hostname") {
        "无法解析主机名"
    } else if lower.contains("connection refused") {
        "连接被拒绝，请检查 SSH 端口"
    } else if lower.contains("timed out") || lower.contains("no route to host") {
        "网络不可达或连接超时"
    } else if lower.contains("not found") || lower.contains("not recognized") {
        "找不到 ssh、sftp、python3 或 powershell.exe；检查节点运行环境"
    } else {
        "SSH/SFTP 命令失败；检查网络、认证、SFTP 服务与文件路径"
    };
    format!("{hop}：{reason}；连接已关闭")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_console_sequences_can_split_across_reads() {
        let mut parser = TerminalText::default();
        assert_eq!(parser.push(b"\x1b[?25"), "");
        assert_eq!(parser.push(b"huser password: \x1b[0m"), "user password: ");
        assert_eq!(parser.push(b"\x1b]0;window"), "");
        assert_eq!(
            parser.push(b" title\x1b\\TF_READY_test\r\n"),
            "TF_READY_test\r\n"
        );
    }
    #[test]
    fn rejects_out_of_order_credentials_and_ignores_prompts_after_ready() {
        let mut cmd = CommandBuilder::new("python3");
        cmd.args([
            "-c",
            "print('TF_NODE_test:dmljdGltQGhvc3Q6MjI=\\npassword: ', end='', flush=True)",
        ]);
        cmd.env("TF_AUTH_SEQUENCE", "[]");
        let result = run_command(
            cmd,
            "test",
            &AtomicBool::new(false),
            |_| panic!("must not disclose a secret"),
            Duration::from_secs(3),
        );
        assert!(result.unwrap_err().to_string().contains("非预期认证节点"));
        let mut cmd = CommandBuilder::new("python3");
        cmd.args([
            "-c",
            "print('TF_READY_test\\npassword: ', end='', flush=True)",
        ]);
        cmd.env("TF_AUTH_SEQUENCE", "[]");
        assert!(
            run_command(
                cmd,
                "test",
                &AtomicBool::new(false),
                |_| panic!("remote output is not an authentication prompt"),
                Duration::from_secs(3)
            )
            .is_ok()
        );
    }

    #[test]
    fn cancelling_helper_with_child_process_releases_terminal() {
        let mut cmd = CommandBuilder::new("python3");
        cmd.args(["-c", "import subprocess,time; subprocess.Popen(['sleep','30']); print('user password: ',end='',flush=True); time.sleep(30)"]);
        let started = Instant::now();
        assert!(
            run_command(
                cmd,
                "test",
                &AtomicBool::new(false),
                |_| Ok(None),
                Duration::from_secs(3)
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn recursive_chain_authentication_identifies_each_node() {
        use crate::config::SshNode;
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("ssh");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/python3
import sys,os,termios
args=sys.argv[1:]
host=args[args.index('--')+1]
attrs=termios.tcgetattr(0); attrs[3] &= ~termios.ECHO; termios.tcsetattr(0,termios.TCSANOW,attrs)
print("user@"+host+"'s password: ",end="",flush=True)
assert input() == "secret-"+host
os.execv('/bin/sh',['sh','-c',args[-1]])
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        for count in [1, 2, 4] {
            let nodes: Vec<_> = (0..count)
                .map(|i| SshNode {
                    host: format!("node{i}.test"),
                    port: format!("{}", 2200 + i),
                    user: "user".into(),
                    options: crate::config::NodeOptions::linux(),
                })
                .collect();
            let mut cmd = chain_command(&nodes, "printf 'finished\\n'", "test", false);
            cmd.get_argv_mut()[0] = fake.clone().into_os_string();
            cmd.env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()));
            let mut seen = Vec::new();
            let result = run_command(
                cmd,
                "test",
                &AtomicBool::new(false),
                |prompt| {
                    let Prompt::Secret { hop } = prompt else {
                        panic!("unexpected host key");
                    };
                    let index = seen.len();
                    assert_eq!(hop, nodes[index].label());
                    seen.push(hop);
                    Ok(Some(Zeroizing::new(format!(
                        "secret-{}",
                        nodes[index].host
                    ))))
                },
                Duration::from_secs(5),
            )
            .unwrap();
            assert!(result.contains("finished"));
            assert_eq!(seen.len(), count);
        }
    }

    #[test]
    fn nested_shell_preserves_user_host_and_encoded_path_as_single_arguments() {
        let config = RemoteConfig {
            route: None,
            jump_host: "jump.example".into(),
            jump_port: "22".into(),
            jump_user: "alice".into(),
            windows_host: "100.64.0.2".into(),
            windows_port: "2222".into(),
            windows_user: "a'b $(exit 81); & user".into(),
            windows_root: "C:/".into(),
            ..Default::default()
        };
        let path = "C:/资料 ' $(exit 82); & 文件";
        let cmd = command(&config, path, "test");
        let argv = cmd.get_argv();
        assert_eq!(argv[0], "ssh");
        assert!(argv.iter().any(|arg| arg == "ClearAllForwardings=yes"));
        assert!(argv.iter().any(|arg| arg == "ProxyJump=none"));
        // A shell function replaces only the nested SSH. Execute the exact
        // command through a real POSIX shell to verify argument boundaries.
        let script = format!(
            "ssh() {{ printf '%s\\0' \"$@\"; }}; {}",
            argv.last()
                .unwrap()
                .to_str()
                .unwrap()
                .replace("exec env LC_ALL=C SSH_ASKPASS_REQUIRE=never ", "")
        );
        let result = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(result.status.success());
        let args = result.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
        assert!(args.contains(&config.windows_user.as_bytes()));
        assert_eq!(args[args.len() - 3], config.windows_host.as_bytes());
        let encoded = crate::remote::operations::command(
            crate::config::RemoteOs::Windows,
            &serde_json::json!({"op":"list", "path":path}),
            "test",
        );
        assert!(
            std::str::from_utf8(args[args.len() - 2])
                .unwrap()
                .ends_with(&encoded)
        );
    }

    #[test]
    fn detects_split_prompts_but_not_partial_prompts_or_file_data() {
        assert_eq!(prompt_kind("user@host's pass"), None);
        assert_eq!(prompt_kind("user@host's password: "), Some(false));
        assert_eq!(
            prompt_kind("Are you sure you want to continue connecting (yes/no/[fingerprint])? "),
            Some(true)
        );
        assert_eq!(prompt_kind("Permission denied\r\n"), None);
    }

    #[test]
    fn pty_handles_both_authentication_hops_and_joins_child() {
        let program = r#"
import sys, termios
attrs=termios.tcgetattr(0); attrs[3] &= ~termios.ECHO; termios.tcsetattr(0, termios.TCSANOW, attrs)
print("The authenticity of host 'test' can't be established.", flush=True)
print("ED25519 key fingerprint is SHA256:test.", flush=True)
sys.stdout.write("Are you sure you want to continue connecting (yes/no/[fingerprint])? "); sys.stdout.flush()
assert input() == "yes"
sys.stdout.write("alice@jump's pass"); sys.stdout.flush()
sys.stdout.write("word: "); sys.stdout.flush()
assert input() == "test-secret"
print("\nTF_NODE_test:Ym9iQHdpbmRvd3M6MjI=", flush=True)
sys.stdout.write("bob@windows's password: "); sys.stdout.flush()
assert input() == "test-secret"
import base64,json
print("\nTF_RPC_test_BEGIN\n"+base64.b64encode(json.dumps({"ok":True,"value":{"path":"C:/","entries":[]}}).encode()).decode()+"\nTF_RPC_test_END",flush=True)
"#;
        let mut cmd = CommandBuilder::new("python3");
        cmd.args(["-c", program]);
        let mut hops = Vec::new();
        let output = run_command(
            cmd,
            "test",
            &AtomicBool::new(false),
            |prompt| {
                let answer = match prompt {
                    Prompt::HostKey { hop, details } => {
                        assert!(details.contains("SHA256:test"));
                        hops.push(hop);
                        "yes"
                    }
                    Prompt::Secret { hop } | Prompt::KeyPassphrase { hop, .. } => {
                        hops.push(hop);
                        "test-secret"
                    }
                };
                Ok(Some(Zeroizing::new(answer.to_string())))
            },
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(hops, ["SSH", "SSH", "bob@windows:22"]);
        assert!(!output.contains("test-secret"));
        assert!(super::super::operations::listing(&output, "test").is_ok());
    }

    #[test]
    fn cancelling_a_prompt_and_timeout_release_process() {
        let mut cmd = CommandBuilder::new("python3");
        cmd.args([
            "-c",
            "import time; print('user@host password: ', end='', flush=True); time.sleep(30)",
        ]);
        let started = Instant::now();
        assert!(
            run_command(
                cmd.clone(),
                "test",
                &AtomicBool::new(false),
                |_| Ok(None),
                Duration::from_secs(2)
            )
            .is_err()
        );
        assert!(
            run_command(
                cmd,
                "test",
                &AtomicBool::new(false),
                |_| unreachable!(),
                Duration::ZERO
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
