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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferState {
    Started,
    Stage,
    Running,
    Resumed,
    Done,
    Skipped,
    Failed,
}

/// One real sample from the transfer process. The GUI only formats these
/// values; it never invents progress from timers or expected sizes.
#[derive(Clone, Debug)]
pub struct TransferUpdate {
    pub state: TransferState,
    pub file: String,
    pub file_done: u64,
    pub file_total: u64,
    pub done: u64,
    pub total: u64,
    pub files_done: usize,
    pub files_total: usize,
    pub edge: usize,
    pub edges: usize,
    pub speed: f64,
    pub eta: Option<u64>,
}

struct MeterFile {
    name: String,
    label: String,
    size: u64,
    done: u64,
    complete: bool,
    resumed: bool,
    skipped: bool,
    done_reported: bool,
}

/// Parses the progress meter emitted by OpenSSH sftp. The meter is enabled by
/// the `progress` batch command and reports the bytes actually moved by SFTP.
struct Meter {
    files: Vec<MeterFile>,
    active: Vec<usize>,
    edge: usize,
    edges: usize,
    buffer: String,
    speed: f64,
    last_done: u64,
    last_sample: std::time::Instant,
}
impl Meter {
    fn new(files: Vec<MeterFile>, edge: usize, edges: usize) -> Self {
        let last_done = files.iter().map(|f| f.done).sum();
        Self {
            files,
            active: Vec::new(),
            edge,
            edges,
            buffer: String::new(),
            speed: 0.0,
            last_done,
            last_sample: std::time::Instant::now(),
        }
    }
    fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
    fn done(&self) -> u64 {
        self.files.iter().map(|f| f.done).sum()
    }
    fn files_done(&self) -> usize {
        self.files.iter().filter(|f| f.complete).count()
    }
    fn update(&self, state: TransferState, index: Option<usize>) -> TransferUpdate {
        let (file, file_done, file_total) = index
            .and_then(|i| self.files.get(i))
            .map_or((String::new(), 0, 0), |f| (f.label.clone(), f.done, f.size));
        let done = self.done();
        let total = self.total();
        let eta = if done >= total {
            Some(0)
        } else if self.speed > 0.0 {
            Some(((total - done) as f64 / self.speed).ceil() as u64)
        } else {
            None
        };
        TransferUpdate {
            state,
            file,
            file_done,
            file_total,
            done,
            total,
            files_done: self.files_done(),
            files_total: self.files.len(),
            edge: self.edge,
            edges: self.edges,
            speed: self.speed,
            eta,
        }
    }
    fn emit(
        &self,
        state: TransferState,
        index: Option<usize>,
        report: &mut dyn FnMut(TransferUpdate),
    ) {
        report(self.update(state, index));
    }
    fn stage(&self, report: &mut dyn FnMut(TransferUpdate)) {
        report(self.update(TransferState::Stage, None));
    }
    fn begin_batch(&mut self, active: &[usize]) {
        self.active.clear();
        self.active.extend_from_slice(active);
    }
    fn pick(&self, name: &str) -> Option<usize> {
        let mut fallback = None;
        for &index in &self.active {
            let file = &self.files[index];
            if file.complete {
                continue;
            }
            if file.name == name {
                return Some(index);
            }
            fallback.get_or_insert(index);
        }
        fallback
    }
    fn feed(&mut self, text: &str, report: &mut dyn FnMut(TransferUpdate)) {
        self.buffer.push_str(text);
        while let Some(end) = self.buffer.find(['\r', '\n']) {
            let line = self.buffer[..end].to_string();
            self.buffer.drain(..=end);
            self.consume(&line, report);
        }
        if self.buffer.len() > 16 * 1024 {
            let keep = self
                .buffer
                .char_indices()
                .rev()
                .nth(4096)
                .map_or(0, |(i, _)| i);
            self.buffer.drain(..keep);
        }
    }
    fn consume(&mut self, line: &str, report: &mut dyn FnMut(TransferUpdate)) {
        let Some((name, percent, bytes, speed)) = parse_meter(line) else {
            return;
        };
        let Some(index) = self.pick(&name) else {
            return;
        };
        let file = &mut self.files[index];
        let observed = bytes.max(file.size.saturating_mul(u64::from(percent)) / 100);
        file.done = file.done.max(observed.min(file.size));
        if percent >= 100 {
            file.done = file.size;
        }
        if file.done >= file.size {
            file.complete = true;
        }
        if speed > 0.0 {
            self.speed = if self.speed == 0.0 {
                speed
            } else {
                self.speed * 0.7 + speed * 0.3
            };
        } else {
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(self.last_sample).as_secs_f64();
            let done = self.done();
            if elapsed >= 0.2 && done > self.last_done {
                let sample = (done - self.last_done) as f64 / elapsed;
                self.speed = if self.speed == 0.0 {
                    sample
                } else {
                    self.speed * 0.7 + sample * 0.3
                };
                self.last_done = done;
                self.last_sample = now;
            }
        }
        self.emit(TransferState::Running, Some(index), report);
        if self.files[index].complete && !self.files[index].done_reported {
            self.files[index].done_reported = true;
            self.emit(TransferState::Done, Some(index), report);
        }
    }
    fn complete_batch(&mut self, report: &mut dyn FnMut(TransferUpdate)) {
        let active = std::mem::take(&mut self.active);
        for index in active {
            let was_complete = self.files[index].complete;
            self.files[index].done = self.files[index].size;
            self.files[index].complete = true;
            if !was_complete || !self.files[index].done_reported {
                self.files[index].done_reported = true;
                self.emit(TransferState::Done, Some(index), report);
            }
        }
    }
    fn fail_batch(&mut self, report: &mut dyn FnMut(TransferUpdate)) {
        if let Some(index) = self
            .active
            .iter()
            .copied()
            .find(|&index| !self.files[index].complete)
        {
            self.emit(TransferState::Failed, Some(index), report);
        }
        self.active.clear();
    }
}

fn parse_size(token: &str) -> Option<u64> {
    let token = token.trim().trim_end_matches("/s");
    let split = token
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(token.len());
    let value: f64 = token[..split].parse().ok()?;
    let unit = token[split..].trim().trim_end_matches('B');
    let multiplier = match unit.to_ascii_uppercase().as_str() {
        "" => 1.0,
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * multiplier).round() as u64)
}

/// OpenSSH emits: `name ... 42%   19MB 1.5MB/s 00:03 ETA\r`.
fn parse_meter(line: &str) -> Option<(String, u32, u64, f64)> {
    if line.contains("TF_") || !line.contains('%') {
        return None;
    }
    for (percent_end, _) in line.match_indices('%').rev() {
        let before = &line[..percent_end];
        let percent_start = before
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |index| index + 1);
        let percent_text = &line[percent_start..percent_end];
        let Ok(percent) = percent_text.parse::<u32>() else {
            continue;
        };
        if percent > 100 {
            continue;
        }
        let rest = &line[percent_end + 1..];
        if !rest.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let mut fields = rest.split_whitespace();
        let Some(size_field) = fields.next() else {
            continue;
        };
        let Some(speed_field) = fields.next() else {
            continue;
        };
        if !speed_field.ends_with("/s") {
            continue;
        }
        let Some(bytes) = parse_size(size_field) else {
            continue;
        };
        let speed = parse_size(speed_field).unwrap_or(0) as f64;
        let name = line[..percent_start].trim();
        if name.is_empty() {
            continue;
        }
        let name = name.rsplit('/').next().unwrap_or(name).to_string();
        return Some((name, percent, bytes, speed));
    }
    None
}

struct Session<'a, F> {
    nodes: Vec<SshNode>,
    cancel: &'a AtomicBool,
    ask: F,
    token: String,
}
impl<F: FnMut(Prompt) -> io::Result<Answer>> Session<'_, F> {
    fn rpc(&mut self, node: Option<usize>, request: Value) -> io::Result<Value> {
        self.rpc_inner(node, request, None)
    }
    fn rpc_inner(
        &mut self,
        node: Option<usize>,
        request: Value,
        on_text: Option<&mut dyn FnMut(&str)>,
    ) -> io::Result<Value> {
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
        let Session {
            cancel,
            ask,
            token,
            nodes: _,
        } = self;
        let mut ignored = |_: &str| {};
        let sink: &mut dyn FnMut(&str) = match on_text {
            Some(callback) => callback,
            None => &mut ignored,
        };
        // Large-file hashing may be silent; cancellation is still polled every 50ms.
        let output = terminal::run_command_with_progress(
            cmd,
            token,
            cancel,
            ask,
            Duration::from_secs(1800),
            sink,
        )?;
        operations::parse(&output, token)
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
    fn batch(
        &mut self,
        runner: Option<usize>,
        remote: usize,
        batch: &str,
        meter: &mut Meter,
        active: &[usize],
        report: &mut dyn FnMut(TransferUpdate),
    ) -> io::Result<()> {
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
        // `progress` re-enables the real sftp meter that batch mode disables.
        let batch = format!("progress\n{batch}");
        meter.begin_batch(active);
        let result = {
            let mut feed = |text: &str| meter.feed(text, report);
            self.rpc_inner(
                runner,
                json!({"op":"batch", "args":args, "host":host, "batch":batch, "marker":terminal::marker(node, &self.token), "auth_node":node.label()}),
                Some(&mut feed),
            )
        };
        match result {
            Ok(_) => {
                meter.complete_batch(report);
                Ok(())
            }
            Err(error) => {
                meter.fail_batch(report);
                Err(error)
            }
        }
    }
    fn edge(
        &mut self,
        source: &Location,
        target: &Location,
        items: &[Item],
        edge: usize,
        edges: usize,
        report: &mut dyn FnMut(TransferUpdate),
    ) -> io::Result<()> {
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
        struct Planned {
            index: usize,
            source: Location,
            destination: Location,
            operation: &'static str,
        }
        let mut planned = Vec::new();
        let mut files = Vec::new();
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
            let mut skipped = false;
            let mut offset = 0;
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
                offset = size;
                if size == item.size && existing["hash"] == item.hash {
                    skipped = true;
                } else {
                    resume = true;
                }
            }
            let operation = match (download, resume) {
                (true, true) => "reget",
                (true, false) => "get",
                (false, true) => "reput",
                (false, false) => "put",
            };
            let index = files.len();
            files.push(MeterFile {
                name: src.path.rsplit('/').next().unwrap_or(&src.path).to_string(),
                label: if item.path.is_empty() {
                    src.path.clone()
                } else {
                    item.path.clone()
                },
                size: item.size,
                done: if skipped { item.size } else { offset },
                complete: skipped,
                resumed: resume,
                skipped,
                done_reported: false,
            });
            if !skipped {
                planned.push(Planned {
                    index,
                    source: src,
                    destination: dst,
                    operation,
                });
            }
        }
        let mut meter = Meter::new(files, edge, edges);
        meter.stage(report);
        for index in 0..meter.files.len() {
            let file = &meter.files[index];
            if file.skipped {
                meter.emit(TransferState::Skipped, Some(index), report);
            } else if file.resumed {
                meter.emit(TransferState::Resumed, Some(index), report);
            }
        }
        let mut batch = String::new();
        let mut active = Vec::new();
        for item in planned {
            batch.push_str(&format!(
                "{} {} {}\n",
                item.operation,
                sftp_quote(&item.source.path)?,
                sftp_quote(&item.destination.path)?
            ));
            active.push(item.index);
            if batch.len() >= 2048 {
                self.batch(runner, remote, &batch, &mut meter, &active, report)?;
                batch.clear();
                active.clear();
            }
        }
        if !batch.is_empty() {
            self.batch(runner, remote, &batch, &mut meter, &active, report)?;
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
    mut progress: impl FnMut(TransferUpdate),
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
    let files_total = items.iter().filter(|item| !item.directory).count();
    let total = items
        .iter()
        .filter(|item| !item.directory)
        .map(|item| item.size)
        .sum();
    let edges = order.len();
    progress(TransferUpdate {
        state: TransferState::Started,
        file: String::new(),
        file_done: 0,
        file_total: 0,
        done: 0,
        total,
        files_done: 0,
        files_total,
        edge: 0,
        edges,
        speed: 0.0,
        eta: None,
    });
    let mut staging = Vec::new();
    let result: io::Result<()> = (|| {
        let mut previous = source;
        for (edge, node) in order.into_iter().enumerate() {
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
            session.edge(&previous, &next, &items, edge + 1, edges, &mut progress)?;
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
    progress: impl FnMut(TransferUpdate),
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
        progress,
        ask,
    )
}
pub fn download(
    config: &RemoteConfig,
    source: &str,
    name: &str,
    target_dir: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(TransferUpdate),
    ask: impl FnMut(Prompt) -> io::Result<Answer>,
) -> io::Result<()> {
    valid_name(name)?;
    transfer(
        config,
        &std::path::absolute(target_dir)?.join(name),
        source,
        true,
        cancel,
        progress,
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
    fn parses_only_real_sftp_meter_lines_and_accumulates_bytes() {
        assert_eq!(parse_size("19MB"), Some(19 * 1024 * 1024));
        assert_eq!(parse_size("1.5MB/s"), Some(1_572_864));
        assert_eq!(parse_size("1234"), Some(1234));
        let line = "资料 空格/大文件.bin                                       42%   19MB   1.5MB/s 00:03 ETA";
        let (name, percent, bytes, speed) = parse_meter(line).unwrap();
        assert_eq!(name, "大文件.bin");
        assert_eq!(percent, 42);
        assert_eq!(bytes, 19 * 1024 * 1024);
        assert!(speed > 1_500_000.0);
        assert!(parse_meter("user@example password:").is_none());
        assert!(parse_meter("TF_RPC_abc_BEGIN").is_none());
        assert!(parse_meter("100% done").is_none());

        let mut meter = Meter::new(
            vec![MeterFile {
                name: "大文件.bin".into(),
                label: "资料/大文件.bin".into(),
                size: 1000,
                done: 200,
                complete: false,
                resumed: true,
                skipped: false,
                done_reported: false,
            }],
            2,
            3,
        );
        let mut updates = Vec::new();
        meter.begin_batch(&[0]);
        meter.feed(
            "大文件.bin      50%   500   1.0MB/s 00:01 ETA\r",
            &mut |update| updates.push(update),
        );
        let running = &updates[0];
        assert_eq!(running.state, TransferState::Running);
        assert_eq!(running.done, 500);
        assert_eq!(running.total, 1000);
        assert_eq!((running.edge, running.edges), (2, 3));
        assert_eq!(running.file, "资料/大文件.bin");
        assert!(running.speed > 0.0);
        meter.feed(
            "大文件.bin     100%  1000   1.0MB/s 00:00\r",
            &mut |update| updates.push(update),
        );
        assert!(updates.iter().any(|u| u.state == TransferState::Done));
        assert_eq!(updates.last().unwrap().done, 1000);
        assert_eq!(updates.last().unwrap().eta, Some(0));

        // A completed batch reports success even for files too small to draw a
        // meter; the bytes still come from the successful SFTP operation.
        let mut meter = Meter::new(
            vec![MeterFile {
                name: "tiny".into(),
                label: "tiny".into(),
                size: 3,
                done: 0,
                complete: false,
                resumed: false,
                skipped: false,
                done_reported: false,
            }],
            1,
            1,
        );
        let mut updates = Vec::new();
        meter.begin_batch(&[0]);
        meter.complete_batch(&mut |update| updates.push(update));
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].state, TransferState::Done);
        assert_eq!(updates[0].file_done, 3);
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
            let mut updates = Vec::new();
            transfer(
                &config,
                &source,
                remote.to_str().unwrap(),
                false,
                &cancel,
                |update| updates.push(update),
                ask,
            )
            .unwrap();
            assert!(
                updates
                    .iter()
                    .any(|u| u.state == TransferState::Started && u.total == 6)
            );
            assert!(updates.iter().any(|u| u.state == TransferState::Stage));
            assert!(updates.iter().any(|u| {
                u.state == TransferState::Done && u.file.ends_with("文件.txt") && u.file_done == 6
            }));
            transfer(
                &config,
                &downloaded,
                remote.to_str().unwrap(),
                true,
                &cancel,
                |_| {},
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
                    |_| {},
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
            let mut resume_updates = Vec::new();
            session
                .edge(&src, &dst, &items, 1, 1, &mut |update| {
                    resume_updates.push(update)
                })
                .unwrap();
            assert!(resume_updates.iter().any(|u| {
                u.state == TransferState::Resumed && u.file_done == 3 && u.file_total == 6
            }));
            assert!(
                resume_updates
                    .iter()
                    .any(|u| u.state == TransferState::Done && u.file_done == 6)
            );
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
            session
                .edge(&dst, &receive, &items, 1, 1, &mut |_| {})
                .unwrap();
            assert_eq!(std::fs::read(&receive.path).unwrap(), b"abcdef");
            std::fs::write(&dst.path, b"BAD").unwrap();
            assert!(
                session
                    .edge(&src, &dst, &items, 1, 1, &mut |_| {})
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
                    |_| {},
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
