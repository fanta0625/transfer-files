//! Two ordinary scp hops. No forwarding or persistent connection is used.
use super::{Answer, Prompt};
use crate::config::RemoteConfig;
use portable_pty::CommandBuilder;
use std::{
    io,
    path::Path,
    sync::atomic::AtomicBool,
    time::{SystemTime, UNIX_EPOCH},
};

fn token() -> String {
    format!(
        "tf-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

fn local_scp(config: &RemoteConfig, source: &Path, target: &str) -> CommandBuilder {
    let mut c = CommandBuilder::new("scp");
    c.args([
        "-r",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "NumberOfPasswordPrompts=1",
        "-o",
        "StrictHostKeyChecking=ask",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "--",
    ]);
    c.args([
        source.to_string_lossy().as_ref(),
        &format!("{}@{}:{target}", config.jump_user, config.jump_host),
    ]);
    c
}

fn nested_scp(config: &RemoteConfig, source: &str, target: &str, id: &str) -> CommandBuilder {
    let inner = format!(
        "scp -r {} {}@{}:{} && rm -rf {}",
        super::terminal::quote(source),
        config.windows_user,
        config.windows_host,
        super::terminal::quote(target),
        super::terminal::quote(source)
    );
    let nested = format!(
        "stty -echo; printf '\\nTF_HOP2_{id}\\n'; exec {}",
        super::terminal::quote(&inner)
    );
    let mut c = CommandBuilder::new("ssh");
    c.args(["-tt", "-e", "none"]);
    c.args(super::terminal::OPTIONS);
    c.args([
        "-p",
        &config.jump_port,
        "-l",
        &config.jump_user,
        "--",
        &config.jump_host,
        &nested,
    ]);
    c
}

fn nested_pull(config: &RemoteConfig, source: &str, target: &str, id: &str) -> CommandBuilder {
    let inner = format!(
        "scp -r {} {}@{}:{} && rm -rf {}",
        super::terminal::quote(source),
        config.windows_user,
        config.windows_host,
        super::terminal::quote(target),
        super::terminal::quote(target)
    );
    let nested = format!(
        "stty -echo; printf '\\nTF_HOP2_{id}\\n'; exec {}",
        super::terminal::quote(&inner)
    );
    let mut c = CommandBuilder::new("ssh");
    c.args(["-tt", "-e", "none"]);
    c.args(super::terminal::OPTIONS);
    c.args([
        "-p",
        &config.jump_port,
        "-l",
        &config.jump_user,
        "--",
        &config.jump_host,
        &nested,
    ]);
    c
}

fn local_pull(config: &RemoteConfig, staging: &str, target: &Path) -> CommandBuilder {
    let mut c = CommandBuilder::new("scp");
    c.args([
        "-r",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "NumberOfPasswordPrompts=1",
        "-o",
        "StrictHostKeyChecking=ask",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "--",
    ]);
    c.args([
        &format!("{}@{}:{staging}", config.jump_user, config.jump_host),
        target.to_string_lossy().as_ref(),
    ]);
    c
}

/// Upload one local file or directory. The caller may invoke this repeatedly for a selection.
pub fn upload(
    config: &RemoteConfig,
    source: &Path,
    target_dir: &str,
    cancel: &AtomicBool,
    mut ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<()> {
    let id = token();
    let staging = format!("/tmp/transfer-files-{id}");
    super::terminal::run_command(
        local_scp(config, source, &staging),
        &id,
        cancel,
        &mut ask,
        std::time::Duration::from_secs(60),
    )?;
    let target = format!(
        "{}/{}",
        target_dir.trim_end_matches(['/', '\\']),
        source
            .file_name()
            .ok_or_else(|| io::Error::other("源路径没有文件名"))?
            .to_string_lossy()
    );
    super::terminal::run_command(
        nested_scp(config, &staging, &target, &id),
        &id,
        cancel,
        ask,
        std::time::Duration::from_secs(60),
    )?;
    Ok(())
}

/// Download one remote file or directory into a local directory.
pub fn download(
    config: &RemoteConfig,
    source: &str,
    name: &str,
    target_dir: &Path,
    cancel: &AtomicBool,
    mut ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<()> {
    let id = token();
    let staging = format!("/tmp/transfer-files-{id}");
    super::terminal::run_command(
        nested_pull(config, source, &staging, &id),
        &id,
        cancel,
        &mut ask,
        std::time::Duration::from_secs(60),
    )?;
    let target = target_dir.join(name);
    super::terminal::run_command(
        local_pull(config, &staging, &target),
        &id,
        cancel,
        ask,
        std::time::Duration::from_secs(60),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn staging_name_is_private() {
        assert!(super::token().starts_with("tf-"));
    }
}
