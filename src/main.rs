mod config;
mod files;
mod remote;

use slint::{
    ComponentHandle, DataTransfer, Model, ModelRc, Timer, TimerMode, VecModel, language::DragAction,
};
use std::{
    cell::RefCell,
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use config::RemoteConfig;
use zeroize::Zeroizing;

slint::include_modules!();

#[derive(Default)]
struct Pane {
    path: PathBuf,
    remote: bool,
    entries: Vec<files::Entry>,
    selected: HashSet<PathBuf>,
    generation: u64,
    loading: bool,
    snapshot_time: Option<String>,
}

enum Event {
    Directory(usize, u64, std::io::Result<(PathBuf, Vec<files::Entry>)>),
    RemotePrompt(u64, remote::Prompt, Sender<remote::Answer>),
    Progress(files::Progress),
    Finished(files::Report),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    RemoteTransferFinished(u64, Result<String, String>),
}

struct State {
    panes: [Pane; 2],
    remote_config: RemoteConfig,
    remote_cancel: Arc<AtomicBool>,
    remote_worker: Option<thread::JoinHandle<()>>,
    auth_reply: Option<Sender<remote::Answer>>,
    credentials: Arc<Mutex<remote::CredentialCache>>,
    tx: Sender<Event>,
    cancel: Arc<AtomicBool>,
    busy: bool,
    worker: Option<thread::JoinHandle<()>>,
    // Native winit file drops arrive one path at a time. Coalesce into one batch.
    external: Option<(Instant, PathBuf, Vec<PathBuf>)>,
}

type Shared = Rc<RefCell<State>>;

#[cfg(target_os = "linux")]
fn native_drop_pane(window: &slint::Window, ui: &AppWindow) -> Option<i32> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    use x11rb::protocol::xproto::ConnectionExt;
    window
        .with_winit_window(|window| {
            let xid = match window.window_handle().ok()?.as_raw() {
                RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok()?,
                RawWindowHandle::Xcb(handle) => handle.window.get(),
                _ => return None,
            };
            let (connection, _) = x11rb::connect(None).ok()?;
            let pointer = connection.query_pointer(xid).ok()?.reply().ok()?;
            if !pointer.same_screen {
                return Some(-1);
            }
            let scale = window.scale_factor() as f32;
            Some(ui.invoke_pane_at(pointer.win_x as f32 / scale, pointer.win_y as f32 / scale))
        })
        .flatten()
}

#[cfg(not(target_os = "linux"))]
fn native_drop_pane(_window: &slint::Window, _ui: &AppWindow) -> Option<i32> {
    None
}

fn paint_pane(ui: &AppWindow, state: &Shared, side: usize) {
    let (rows, path, summary, loading) = {
        let state = state.borrow();
        let pane = &state.panes[side];
        let rows = pane
            .entries
            .iter()
            .map(|entry| FileRow {
                name: entry.name.clone().into(),
                kind: (if entry.link {
                    "链接"
                } else if entry.directory {
                    "目录"
                } else {
                    "文件"
                })
                .into(),
                size: (if entry.directory || entry.link {
                    "—".into()
                } else {
                    files::size_label(entry.size)
                })
                .into(),
                modified: entry.modified.clone().into(),
                directory: entry.directory,
                selected: pane.selected.contains(&entry.path),
            })
            .collect::<Vec<_>>();
        (
            rows,
            pane.path.display().to_string(),
            format!(
                "{} 项 / 已选 {} 项",
                pane.entries.len(),
                pane.selected.len()
            ),
            pane.loading,
        )
    };
    let model = ModelRc::new(VecModel::from(rows));
    if side == 0 {
        ui.set_left_rows(model);
        ui.set_left_path(path.into());
        ui.set_left_summary(summary.into());
        ui.set_left_loading(loading);
    } else {
        ui.set_right_rows(model);
        ui.set_right_path(path.into());
        ui.set_right_summary(summary.into());
        ui.set_right_loading(loading);
        let state = state.borrow();
        ui.set_right_remote(state.panes[1].remote);
        ui.set_remote_snapshot(
            state.panes[1]
                .snapshot_time
                .clone()
                .map(|time| format!("已断开 · 目录快照 {time}"))
                .unwrap_or_else(|| "未读取 · 点击「读取远程目录」".into())
                .into(),
        );
    }
    ui.set_revision(ui.get_revision().wrapping_add(1));
}

fn paint_selection(ui: &AppWindow, state: &Shared, side: usize) {
    let (selection, summary) = {
        let state = state.borrow();
        let pane = &state.panes[side];
        (
            pane.entries
                .iter()
                .map(|e| pane.selected.contains(&e.path))
                .collect::<Vec<_>>(),
            format!(
                "{} 项 / 已选 {} 项",
                pane.entries.len(),
                pane.selected.len()
            ),
        )
    };
    let model = if side == 0 {
        ui.get_left_rows()
    } else {
        ui.get_right_rows()
    };
    // Keep delegate identities stable: replacing the entire model on every click
    // would reset TouchArea's double-click state and the user's scroll position.
    if let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() {
        for (index, selected) in selection.into_iter().enumerate() {
            if let Some(mut row) = model.row_data(index)
                && row.selected != selected
            {
                row.selected = selected;
                model.set_row_data(index, row);
            }
        }
    }
    if side == 0 {
        ui.set_left_summary(summary.into());
    } else {
        ui.set_right_summary(summary.into());
    }
    ui.set_revision(ui.get_revision().wrapping_add(1));
}

fn paint_remote_config(ui: &AppWindow, config: &RemoteConfig) {
    ui.set_setting_route(config.route_text().into());
    ui.set_remote_label(config.windows_host.clone().into());
    ui.set_setting_jump_host(config.jump_host.clone().into());
    ui.set_setting_jump_port(config.jump_port.clone().into());
    ui.set_setting_jump_user(config.jump_user.clone().into());
    ui.set_setting_windows_host(config.windows_host.clone().into());
    ui.set_setting_windows_port(config.windows_port.clone().into());
    ui.set_setting_windows_user(config.windows_user.clone().into());
    ui.set_setting_windows_root(config.windows_root.clone().into());
}

fn ask_remote(
    tx: &Sender<Event>,
    generation: u64,
    cancel: &AtomicBool,
    cache: &Arc<Mutex<remote::CredentialCache>>,
    prompt: remote::Prompt,
) -> std::io::Result<remote::Answer> {
    let is_secret = matches!(prompt, remote::Prompt::Secret { .. });
    let hop = match &prompt {
        remote::Prompt::Secret { hop } | remote::Prompt::HostKey { hop, .. } => hop.clone(),
    };
    if is_secret && let Some(answer) = cache.lock().unwrap().get(&hop) {
        return Ok(Some(answer));
    }
    let (reply, answer) = mpsc::channel();
    tx.send(Event::RemotePrompt(generation, prompt, reply))
        .map_err(|_| std::io::Error::other("界面已关闭"))?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "认证已取消或等待超过两分钟",
            ));
        }
        match answer.recv_timeout(Duration::from_millis(50)) {
            Ok(Some(value)) => {
                if is_secret {
                    cache.lock().unwrap().put(&hop, value.clone());
                }
                return Ok(Some(value));
            }
            Ok(None) => return Ok(None),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return Ok(None),
        }
    }
}

fn navigate(ui: &AppWindow, state: &Shared, side: usize, path: PathBuf) {
    let (generation, tx, remote_pane, remote_config, cancel) = {
        let mut state = state.borrow_mut();
        if state.panes[side].remote && (state.busy || state.remote_worker.is_some()) {
            return;
        }
        if state.panes[side].remote {
            state.remote_cancel = Arc::new(AtomicBool::new(false));
        }
        let pane = &mut state.panes[side];
        pane.generation += 1;
        pane.loading = true;
        let generation = pane.generation;
        let remote_pane = pane.remote;
        (
            generation,
            state.tx.clone(),
            remote_pane,
            state.remote_config.clone(),
            state.remote_cancel.clone(),
        )
    };
    let credentials = state.borrow().credentials.clone();
    if remote_pane {
        ui.set_status(
            format!(
                "正在连接 {}…",
                remote_config
                    .nodes()
                    .unwrap_or_default()
                    .iter()
                    .map(|n| n.host.clone())
                    .collect::<Vec<_>>()
                    .join(" → ")
            )
            .into(),
        );
        ui.set_remote_reading(true);
    }
    paint_pane(ui, state, side);
    let worker = thread::spawn(move || {
        let result = if remote_pane {
            remote::read_directory(&remote_config, &path.to_string_lossy(), &cancel, |prompt| {
                ask_remote(&tx, generation, &cancel, &credentials, prompt)
            })
        } else {
            files::read_directory(&path)
        };
        let _ = tx.send(Event::Directory(side, generation, result));
    });
    if remote_pane {
        state.borrow_mut().remote_worker = Some(worker);
    }
}

fn selected(state: &State, side: usize) -> Vec<PathBuf> {
    state.panes[side]
        .entries
        .iter()
        .filter(|entry| state.panes[side].selected.contains(&entry.path))
        .map(|entry| entry.path.clone())
        .collect()
}

fn data_paths(data: &DataTransfer) -> Vec<PathBuf> {
    if let Some(value) = data.user_data()
        && let Some(paths) = value.downcast_ref::<Vec<PathBuf>>()
    {
        return paths.clone();
    }
    data.file_paths()
        .map(|paths| paths.map(PathBuf::from).collect())
        .unwrap_or_default()
}

fn start_copy(ui: &AppWindow, state: &Shared, sources: Vec<PathBuf>, target: PathBuf) {
    if state.borrow().panes[1].remote {
        start_remote_upload(ui, state, sources, target);
        return;
    }
    if state.borrow().busy {
        return;
    }
    if sources.is_empty() {
        ui.set_status("请先选择要复制的文件或目录".into());
        return;
    }
    let mut state = state.borrow_mut();
    state.cancel = Arc::new(AtomicBool::new(false));
    state.busy = true;
    let cancel = state.cancel.clone();
    let tx = state.tx.clone();
    ui.set_busy(true);
    ui.set_progress(0.0);
    ui.set_status(format!("正在扫描 {} 个源项目…", sources.len()).into());
    ui.set_detail(format!("目标：{}", target.display()).into());
    ui.set_history("同名项目跳过（目录不合并）；符号链接不复制。".into());
    state.worker = Some(thread::spawn(move || {
        let mut last = Instant::now() - Duration::from_secs(1);
        let report = files::copy_batch(&sources, &target, &cancel, |progress| {
            if last.elapsed() >= Duration::from_millis(50) || progress.files == progress.total_files
            {
                let _ = tx.send(Event::Progress(progress.clone()));
                last = Instant::now();
            }
        });
        let _ = tx.send(Event::Finished(report));
    }));
}

#[cfg(target_os = "linux")]
fn start_remote_download(ui: &AppWindow, state: &Shared, sources: Vec<PathBuf>, target: PathBuf) {
    if sources.is_empty() || state.borrow().busy || state.borrow().remote_worker.is_some() {
        if sources.is_empty() {
            ui.set_status("请先选择要下载的文件或目录".into());
        }
        return;
    }
    let (generation, tx, config, cancel, credentials) = {
        let mut state = state.borrow_mut();
        state.remote_cancel = Arc::new(AtomicBool::new(false));
        state.panes[1].generation += 1;
        state.panes[1].loading = true;
        state.busy = true;
        (
            state.panes[1].generation,
            state.tx.clone(),
            state.remote_config.clone(),
            state.remote_cancel.clone(),
            state.credentials.clone(),
        )
    };
    ui.set_busy(true);
    ui.set_remote_reading(true);
    ui.set_progress(0.0);
    ui.set_status(format!("正在下载 {} 个项目…", sources.len()).into());
    let worker = thread::spawn(move || {
        let result = (|| {
            for (index, source) in sources.iter().enumerate() {
                let name =
                    remote::file_name(&source.to_string_lossy()).map_err(|e| e.to_string())?;
                remote::transfer::download(
                    &config,
                    &source.to_string_lossy(),
                    &name,
                    &target,
                    &cancel,
                    |prompt| ask_remote(&tx, generation, &cancel, &credentials, prompt),
                )
                .map_err(|e| e.to_string())?;
                let _ = tx.send(Event::Progress(files::Progress {
                    files: index + 1,
                    total_files: sources.len(),
                    current: name,
                    ..Default::default()
                }));
            }
            Ok("下载完成，SSH 已断开".into())
        })();
        let _ = tx.send(Event::RemoteTransferFinished(generation, result));
    });
    state.borrow_mut().remote_worker = Some(worker);
}

#[cfg(target_os = "linux")]
fn start_remote_upload(ui: &AppWindow, state: &Shared, sources: Vec<PathBuf>, target: PathBuf) {
    if sources.is_empty() || state.borrow().busy || state.borrow().remote_worker.is_some() {
        if sources.is_empty() {
            ui.set_status("请先选择要上传的文件或目录".into());
        }
        return;
    }
    let (generation, tx, config, cancel, credentials) = {
        let mut state = state.borrow_mut();
        state.remote_cancel = Arc::new(AtomicBool::new(false));
        state.panes[1].generation += 1;
        state.panes[1].loading = true;
        state.busy = true;
        (
            state.panes[1].generation,
            state.tx.clone(),
            state.remote_config.clone(),
            state.remote_cancel.clone(),
            state.credentials.clone(),
        )
    };
    ui.set_busy(true);
    ui.set_remote_reading(true);
    ui.set_progress(0.0);
    ui.set_status(format!("正在上传 {} 个项目…", sources.len()).into());
    ui.set_detail(format!("目标：{}；密码缓存 5 分钟", target.display()).into());
    let worker = thread::spawn(move || {
        let result = (|| {
            for (index, source) in sources.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    return Err("上传已取消，SSH 已断开".into());
                }
                remote::transfer::upload(
                    &config,
                    source,
                    &target.to_string_lossy(),
                    &cancel,
                    |prompt| ask_remote(&tx, generation, &cancel, &credentials, prompt),
                )
                .map_err(|e| e.to_string())?;
                let _ = tx.send(Event::Progress(files::Progress {
                    files: index + 1,
                    total_files: sources.len(),
                    current: source.display().to_string(),
                    ..Default::default()
                }));
            }
            Ok("上传完成，SSH 已断开".into())
        })();
        let _ = tx.send(Event::RemoteTransferFinished(generation, result));
    });
    state.borrow_mut().remote_worker = Some(worker);
}

fn set_remote_not_ready(ui: &AppWindow) {
    ui.set_status("当前操作不可用".into());
    ui.set_detail("远程面板支持从左侧上传、从右侧下载；请先选择文件或目录。".into());
}

fn wire(ui: &AppWindow, state: &Shared) {
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_auth_submit(move || {
        let ui = weak.unwrap();
        let answer = if ui.get_auth_host_key() {
            Zeroizing::new("yes".into())
        } else {
            Zeroizing::new(ui.get_auth_secret().to_string())
        };
        ui.set_auth_secret("".into());
        ui.set_auth_visible(false);
        if let Some(reply) = shared.borrow_mut().auth_reply.take() {
            let _ = reply.send(Some(answer));
        }
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_auth_cancel(move || {
        let ui = weak.unwrap();
        ui.set_auth_secret("".into());
        ui.set_auth_visible(false);
        if let Some(reply) = shared.borrow_mut().auth_reply.take() {
            let _ = reply.send(None);
        }
        shared.borrow().remote_cancel.store(true, Ordering::Relaxed);
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_show_settings(move || {
        let ui = weak.unwrap();
        let config = shared.borrow().remote_config.clone();
        paint_remote_config(&ui, &config);
        ui.set_settings_error("".into());
        ui.set_settings_visible(true);
    });
    let weak = ui.as_weak();
    ui.on_cancel_settings(move || weak.unwrap().set_settings_visible(false));
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_save_settings(
        move |jump_host,
              jump_port,
              jump_user,
              windows_host,
              windows_port,
              windows_user,
              windows_root,
              route| {
            if shared.borrow().busy || shared.borrow().remote_worker.is_some() {
                return;
            }
            let config = RemoteConfig {
                route: Some(route.to_string()),
                jump_host: jump_host.to_string(),
                jump_port: jump_port.to_string(),
                jump_user: jump_user.to_string(),
                windows_host: windows_host.to_string(),
                windows_port: windows_port.to_string(),
                windows_user: windows_user.to_string(),
                windows_root: windows_root.to_string(),
            };
            let ui = weak.unwrap();
            if let Err(error) = config.validate() {
                ui.set_settings_error(error.clone().into());
                ui.set_status("远程设置未保存".into());
                ui.set_detail(error.into());
                return;
            }
            match config.save() {
                Ok(path) => {
                    let remote_root = PathBuf::from(&config.windows_root);
                    {
                        let mut state = shared.borrow_mut();
                        state.credentials.lock().unwrap().clear();
                        ui.set_remote_label(config.windows_host.clone().into());
                        state.remote_config = config;
                        state.panes[1].remote = true;
                        state.panes[1].path = remote_root.clone();
                        state.panes[1].generation += 1;
                        state.panes[1].loading = false;
                        state.panes[1].entries.clear();
                        state.panes[1].selected.clear();
                        state.panes[1].snapshot_time = None;
                        state.external = None;
                    }
                    ui.set_settings_visible(false);
                    ui.set_status("远程设置已保存".into());
                    paint_pane(&ui, &shared, 1);
                    ui.set_detail(
                        format!("已保存到 {}；点击「读取远程目录」开始浏览", path.display()).into(),
                    );
                }
                Err(error) => {
                    ui.set_settings_error(error.to_string().into());
                    ui.set_status("远程设置未保存".into());
                    ui.set_detail(format!("写入配置失败：{error}").into());
                }
            }
        },
    );

    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_navigate(move |side, path| {
        let ui = weak.unwrap();
        let side = side as usize;
        let mut path = PathBuf::from(path.as_str());
        if shared.borrow().panes[side].remote {
            path = remote::resolve_path(
                &shared.borrow().panes[side].path.to_string_lossy(),
                path.to_string_lossy().as_ref(),
            );
        } else if path.is_relative() {
            path = shared.borrow().panes[side].path.join(path);
        }
        navigate(&ui, &shared, side, path);
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_parent_directory(move |side| {
        let side = side as usize;
        let path = {
            let state = shared.borrow();
            if state.panes[side].remote {
                remote::parent_path(&state.panes[side].path.to_string_lossy())
            } else {
                state.panes[side].path.parent().map(PathBuf::from)
            }
        };
        if let Some(path) = path {
            navigate(&weak.unwrap(), &shared, side, path);
        }
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_refresh(move |side| {
        let path = shared.borrow().panes[side as usize].path.clone();
        navigate(&weak.unwrap(), &shared, side as usize, path);
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_toggle(move |side, index| {
        let ui = weak.unwrap();
        let side = side as usize;
        let entry = shared.borrow().panes[side]
            .entries
            .get(index as usize)
            .cloned();
        if let Some(entry) = entry {
            let mut state = shared.borrow_mut();
            let selection = &mut state.panes[side].selected;
            if !selection.remove(&entry.path) {
                selection.insert(entry.path);
            }
        }
        ui.set_active_pane(side as i32);
        paint_selection(&ui, &shared, side);
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_open(move |side, index| {
        let entry = shared.borrow().panes[side as usize]
            .entries
            .get(index as usize)
            .cloned();
        if let Some(entry) = entry
            && entry.directory
            && !entry.link
        {
            navigate(&weak.unwrap(), &shared, side as usize, entry.path);
        }
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_select_all(move |side| {
        {
            let mut state = shared.borrow_mut();
            let pane = &mut state.panes[side as usize];
            pane.selected = pane.entries.iter().map(|e| e.path.clone()).collect();
        }
        paint_selection(&weak.unwrap(), &shared, side as usize);
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_clear_selection(move |side| {
        shared.borrow_mut().panes[side as usize].selected.clear();
        paint_selection(&weak.unwrap(), &shared, side as usize);
    });
    let weak = ui.as_weak();
    ui.on_activate(move |side| weak.unwrap().set_active_pane(side));
    let shared = state.clone();
    ui.on_make_payload(move |side, index, _revision| {
        let state = shared.borrow();
        let side = side as usize;
        if state.panes[side].remote {
            return DataTransfer::default();
        }
        let paths = match state.panes[side].entries.get(index as usize) {
            Some(entry) if state.panes[side].selected.contains(&entry.path) => {
                selected(&state, side)
            }
            Some(entry) => vec![entry.path.clone()],
            None => Vec::new(),
        };
        let mut data = DataTransfer::default();
        // In-memory data keeps internal dragging in this window. It intentionally
        // does not advertise native URLs for dragging simulated remote files out.
        data.set_user_data(Rc::new(paths));
        data
    });
    let shared = state.clone();
    ui.on_accept_drop(move |side, event| {
        let state = shared.borrow();
        if state.busy
            || state.panes[side as usize].remote && data_paths(&event.data).is_empty()
            || state.panes[side as usize].loading
            || data_paths(&event.data).is_empty()
        {
            DragAction::None
        } else {
            DragAction::Copy
        }
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_receive_drop(move |side, event| {
        let target = shared.borrow().panes[side as usize].path.clone();
        start_copy(&weak.unwrap(), &shared, data_paths(&event.data), target);
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_copy_selected(move |side| {
        if shared.borrow().panes[side as usize].remote {
            let (sources, target) = {
                let state = shared.borrow();
                (selected(&state, side as usize), state.panes[0].path.clone())
            };
            #[cfg(not(target_os = "linux"))]
            let _ = (&sources, &target);
            #[cfg(target_os = "linux")]
            start_remote_download(&weak.unwrap(), &shared, sources, target);
            #[cfg(not(target_os = "linux"))]
            set_remote_not_ready(&weak.unwrap());
            return;
        }
        let (sources, target) = {
            let state = shared.borrow();
            (
                selected(&state, side as usize),
                state.panes[1 - side as usize].path.clone(),
            )
        };
        start_copy(&weak.unwrap(), &shared, sources, target);
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_cancel(move || {
        shared.borrow().cancel.store(true, Ordering::Relaxed);
        shared.borrow().remote_cancel.store(true, Ordering::Relaxed);
        shared.borrow_mut().auth_reply.take();
        shared.borrow().credentials.lock().unwrap().clear();
        weak.unwrap().set_auth_secret("".into());
        weak.unwrap().set_auth_visible(false);
        weak.unwrap()
            .set_status("正在取消，等待当前读写结束…".into());
    });

    // Slint's in-window DropArea is separate from the OS file-drop events.
    // winit handles X11 XDND text/uri-list, including Dolphin multi-file drops.
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.window().on_winit_window_event(move |window, event| {
        if let WindowEvent::DroppedFile(path) = event {
            let ui = weak.unwrap();
            // winit 0.30 does not include pointer coordinates in DroppedFile.
            // On X11, query the pointer relative to our window at drop time.
            let side = native_drop_pane(window, &ui).unwrap_or(ui.get_active_pane());
            if side < 0 {
                return EventResult::PreventDefault;
            }
            let side = side as usize;
            let mut state = shared.borrow_mut();
            if (state.panes[side].remote && side != 1)
                || ui.get_settings_visible()
                || ui.get_auth_visible()
            {
                set_remote_not_ready(&ui);
                return EventResult::PreventDefault;
            }
            if state.busy || state.panes[side].loading {
                return EventResult::PreventDefault;
            }
            ui.set_active_pane(side as i32);
            let target = state.panes[side].path.clone();
            if state
                .external
                .as_ref()
                .is_some_and(|pending| pending.1 != target)
            {
                ui.set_history("上一批拖入尚在收集，请稍后再向另一栏拖入。".into());
                return EventResult::PreventDefault;
            }
            let pending = state
                .external
                .get_or_insert_with(|| (Instant::now(), target, Vec::new()));
            if !pending.2.contains(path) {
                pending.2.push(path.clone());
            }
            pending.0 = Instant::now();
            return EventResult::PreventDefault;
        }
        EventResult::Propagate
    });
}

fn poll(ui: &AppWindow, state: &Shared, rx: &Receiver<Event>) {
    // Keep an event-heavy directory or batch from monopolizing the GUI loop.
    for _ in 0..100 {
        let Ok(event) = rx.try_recv() else {
            break;
        };
        match event {
            Event::RemotePrompt(generation, prompt, reply) => {
                let mut state = state.borrow_mut();
                if generation != state.panes[1].generation
                    || state.remote_cancel.load(Ordering::Relaxed)
                {
                    let _ = reply.send(None);
                    continue;
                }
                let (title, detail, host_key) = match prompt {
                    remote::Prompt::Secret { hop } => (format!("{hop} · SSH 认证"),
                        "请输入该端 SSH 密码（或私钥口令）。仅用于本次读取，不保存；两分钟内未提交将断开。".to_string(), false),
                    remote::Prompt::HostKey { hop, details } => (format!("{hop} · 确认主机指纹"),
                        format!("{details}\n请通过可信方式核对指纹。确认后由 OpenSSH 记录至该端 known_hosts。"), true),
                };
                state.auth_reply = Some(reply);
                ui.set_auth_title(title.into());
                ui.set_auth_detail(detail.into());
                ui.set_auth_host_key(host_key);
                ui.set_auth_secret("".into());
                ui.set_auth_visible(true);
            }
            Event::Directory(side, generation, result) => {
                if generation != state.borrow().panes[side].generation {
                    continue;
                }
                let is_remote = state.borrow().panes[side].remote;
                if is_remote {
                    let worker = state.borrow_mut().remote_worker.take();
                    if let Some(worker) = worker {
                        let _ = worker.join();
                    }
                    state.borrow_mut().auth_reply.take();
                    ui.set_remote_reading(false);
                    ui.set_auth_visible(false);
                    ui.set_auth_secret("".into());
                }
                {
                    let mut state = state.borrow_mut();
                    let pane = &mut state.panes[side];
                    if generation != pane.generation {
                        continue;
                    }
                    pane.loading = false;
                    match result {
                        Ok((path, entries)) => {
                            pane.path = path;
                            pane.entries = entries;
                            pane.selected.clear();
                            if is_remote {
                                pane.snapshot_time = Some(
                                    chrono::Utc::now()
                                        .format("%Y-%m-%d %H:%M:%S UTC")
                                        .to_string(),
                                );
                                ui.set_status("远程目录已更新，SSH 已断开".into());
                                ui.set_history("右栏保留目录快照；双击、上级或刷新会重新连接，5 分钟内复用各节点密码。".into());
                            }
                        }
                        Err(error) => {
                            ui.set_history(format!("读取目录失败：{error}").into());
                            if is_remote {
                                ui.set_status("远程读取未完成，SSH 已断开；保留上次快照".into());
                            }
                        }
                    }
                }
                paint_pane(ui, state, side);
            }
            Event::Progress(progress) => {
                let fraction = if progress.total > 0 {
                    progress.bytes as f64 / progress.total as f64
                } else if progress.total_files > 0 {
                    progress.files as f64 / progress.total_files as f64
                } else {
                    0.0
                };
                ui.set_progress(fraction.clamp(0.0, 1.0) as f32);
                ui.set_status(
                    format!(
                        "复制中 · {} / {} 个文件 · {} / {}",
                        progress.files,
                        progress.total_files,
                        files::size_label(progress.bytes),
                        files::size_label(progress.total)
                    )
                    .into(),
                );
                ui.set_detail(progress.current.into());
            }
            Event::Finished(report) => {
                let worker = {
                    let mut state = state.borrow_mut();
                    state.busy = false;
                    state.worker.take()
                };
                if let Some(worker) = worker {
                    let _ = worker.join();
                }
                ui.set_busy(false);
                if !report.cancelled && report.errors.is_empty() {
                    ui.set_progress(1.0);
                }
                ui.set_status(
                    format!(
                        "{} · 成功 {} 个文件 / 跳过 {} 个源项目 / 错误 {} 项",
                        if report.cancelled {
                            "已取消"
                        } else if report.errors.is_empty() {
                            "批次完成"
                        } else {
                            "批次结束（有错误）"
                        },
                        report.copied,
                        report.skipped,
                        report.errors.len()
                    )
                    .into(),
                );
                ui.set_detail("源文件保留。同名目录整体跳过；取消时已完成的文件保留。".into());
                ui.set_history(if report.errors.is_empty() {
                    "无错误。目录复制不保留原权限和时间；这是本地原型，尚未连接 SSH。".into()
                } else {
                    report.errors.join("\n").into()
                });
                for side in 0..2 {
                    let path = state.borrow().panes[side].path.clone();
                    navigate(ui, state, side, path);
                }
            }
            Event::RemoteTransferFinished(generation, result) => {
                if generation != state.borrow().panes[1].generation {
                    continue;
                }
                let worker = state.borrow_mut().remote_worker.take();
                if let Some(worker) = worker {
                    let _ = worker.join();
                }
                {
                    let mut state = state.borrow_mut();
                    state.busy = false;
                    state.panes[1].loading = false;
                    state.auth_reply.take();
                }
                ui.set_busy(false);
                ui.set_remote_reading(false);
                ui.set_auth_visible(false);
                ui.set_auth_secret("".into());
                match result {
                    Ok(message) => {
                        ui.set_progress(1.0);
                        ui.set_status(message.into());
                        ui.set_detail("文件或目录传输完成；点击刷新更新远程目录快照。".into());
                        ui.set_history("连接链传输和暂存清理已完成，本次 SSH 会话已关闭。".into());
                    }
                    Err(error) => {
                        state.borrow().credentials.lock().unwrap().clear();
                        ui.set_status("远程传输未完成，SSH 已断开".into());
                        ui.set_history(error.clone().into());
                        ui.set_detail(error.into());
                    }
                }
                paint_pane(ui, state, 1);
                let local = state.borrow().panes[0].path.clone();
                navigate(ui, state, 0, local);
            }
        }
    }
    let external = {
        let mut state = state.borrow_mut();
        if !state.busy
            && state
                .external
                .as_ref()
                .is_some_and(|e| e.0.elapsed() >= Duration::from_millis(250))
        {
            state.external.take()
        } else {
            None
        }
    };
    if let Some((_, target, sources)) = external {
        start_copy(ui, state, sources, target);
    }
}

#[cfg(not(target_os = "linux"))]
fn start_remote_upload(ui: &AppWindow, _state: &Shared, _sources: Vec<PathBuf>, _target: PathBuf) {
    set_remote_not_ready(ui);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    let ui = AppWindow::new()?;
    let initial = std::env::current_dir()?;
    let args = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let (remote_config, load_error) = match RemoteConfig::load() {
        Ok(config) => (config, None),
        Err(error) => (RemoteConfig::default(), Some(error.to_string())),
    };
    let remote_enabled = remote_config.is_configured() && args.len() < 2;
    let paths = [
        args.first().cloned().unwrap_or_else(|| initial.clone()),
        if remote_enabled {
            PathBuf::from(&remote_config.windows_root)
        } else {
            args.get(1).cloned().unwrap_or_else(|| initial.clone())
        },
    ];
    let (tx, rx) = mpsc::channel();
    let state = Rc::new(RefCell::new(State {
        panes: [
            Pane {
                path: initial.clone(),
                remote: false,
                ..Pane::default()
            },
            Pane {
                path: if remote_enabled {
                    PathBuf::from(&remote_config.windows_root)
                } else {
                    initial
                },
                remote: remote_enabled,
                ..Pane::default()
            },
        ],
        remote_config,
        remote_cancel: Arc::new(AtomicBool::new(false)),
        remote_worker: None,
        auth_reply: None,
        credentials: Arc::new(Mutex::new(remote::CredentialCache::default())),
        tx,
        cancel: Arc::new(AtomicBool::new(false)),
        busy: false,
        worker: None,
        external: None,
    }));
    ui.set_active_pane(1);
    paint_remote_config(&ui, &state.borrow().remote_config);
    if let Some(error) = load_error {
        ui.set_history(format!("读取配置失败，请检查「远程设置」：{error}").into());
    }
    wire(&ui, &state);
    for (side, path) in paths.into_iter().enumerate() {
        if state.borrow().panes[side].remote {
            paint_pane(&ui, &state, side);
        } else {
            navigate(&ui, &state, side, path);
        }
    }
    let timer = Timer::default();
    let weak = ui.as_weak();
    let shared = state.clone();
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        if let Some(ui) = weak.upgrade() {
            poll(&ui, &shared, &rx);
        }
    });
    let shared = state.clone();
    ui.window().on_close_requested(move || {
        shared.borrow().cancel.store(true, Ordering::Relaxed);
        shared.borrow().remote_cancel.store(true, Ordering::Relaxed);
        shared.borrow_mut().auth_reply.take();
        slint::CloseRequestResponse::HideWindow
    });
    ui.run()?;
    state.borrow().cancel.store(true, Ordering::Relaxed);
    state.borrow().remote_cancel.store(true, Ordering::Relaxed);
    state.borrow_mut().auth_reply.take();
    if let Some(worker) = state.borrow_mut().worker.take() {
        let _ = worker.join();
    }
    if let Some(worker) = state.borrow_mut().remote_worker.take() {
        let _ = worker.join();
    }
    Ok(())
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
    use slint::{LogicalPosition, LogicalSize, platform::PointerEventButton};

    #[test]
    fn host_key_acceptance_never_becomes_a_cached_password() {
        let (tx, rx) = mpsc::channel();
        let cache = Arc::new(Mutex::new(remote::CredentialCache::default()));
        let responder = thread::spawn(move || {
            let Event::RemotePrompt(_, _, reply) = rx.recv().unwrap() else {
                panic!("expected prompt")
            };
            reply.send(Some(Zeroizing::new("yes".into()))).unwrap();
        });
        let answer = ask_remote(
            &tx,
            1,
            &AtomicBool::new(false),
            &cache,
            remote::Prompt::HostKey {
                hop: "家里 Windows".into(),
                details: "test fingerprint".into(),
            },
        )
        .unwrap();
        responder.join().unwrap();
        assert_eq!(answer.unwrap().as_str(), "yes");
        assert!(cache.lock().unwrap().get("家里 Windows").is_none());
    }

    fn wait_for(
        ui: &AppWindow,
        state: &Shared,
        rx: &Receiver<Event>,
        condition: impl Fn(&State) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            poll(ui, state, rx);
            if condition(&state.borrow()) {
                return;
            }
            assert!(Instant::now() < deadline, "UI worker timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn snapshot(ui: &AppWindow, name: &str) {
        let pixels = ui.window().take_snapshot().unwrap();
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/qa");
        std::fs::create_dir_all(&directory).unwrap();
        image::save_buffer(
            directory.join(name),
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }

    #[test]
    fn browse_multiselect_drag_copy_both_directions_and_native_payload() {
        slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
            mock_time: true,
            threading: false,
            renderer_name: Some("software".into()),
        })))
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let left = root.path().join("左侧目录");
        let right = root.path().join("右侧目录");
        std::fs::create_dir_all(left.join("资料 空格/空目录")).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("资料 空格/内容.txt"), "目录内容").unwrap();
        std::fs::write(left.join("测试文件.txt"), "文件内容").unwrap();
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(LogicalSize::new(1180.0, 800.0));
        let (tx, rx) = mpsc::channel();
        let state = Rc::new(RefCell::new(State {
            panes: [
                Pane {
                    path: left.clone(),
                    remote: false,
                    ..Pane::default()
                },
                Pane {
                    path: right.clone(),
                    remote: false,
                    ..Pane::default()
                },
            ],
            remote_config: RemoteConfig::default(),
            remote_cancel: Arc::new(AtomicBool::new(false)),
            remote_worker: None,
            auth_reply: None,
            credentials: Arc::new(Mutex::new(remote::CredentialCache::default())),
            tx,
            cancel: Arc::new(AtomicBool::new(false)),
            busy: false,
            worker: None,
            external: None,
        }));
        wire(&ui, &state);
        navigate(&ui, &state, 0, left.clone());
        navigate(&ui, &state, 1, right.clone());
        wait_for(&ui, &state, &rx, |s| s.panes.iter().all(|p| !p.loading));
        ui.show().unwrap();
        ui.invoke_select_all(0);
        snapshot(&ui, "prototype.png");
        assert_eq!(selected(&state.borrow(), 0).len(), 2);

        // Dispatch a real pointer gesture through DragArea/DropArea, not only callbacks.
        let row = ElementHandle::find_by_element_type_name(&ui, "DragArea")
            .next()
            .unwrap();
        let target = ElementHandle::find_by_element_id(&ui, "AppWindow::right-pane")
            .next()
            .unwrap();
        let position = target.absolute_position();
        let target = LogicalPosition::new(position.x + 100.0, position.y + 240.0);
        assert_eq!(ui.invoke_pane_at(target.x, target.y), 1);
        row.mock_drag(target, PointerEventButton::Left);
        assert!(state.borrow().busy, "internal drag must start a batch");
        wait_for(&ui, &state, &rx, |s| {
            !s.busy && s.panes.iter().all(|p| !p.loading)
        });
        assert_eq!(
            std::fs::read(right.join("测试文件.txt")).unwrap(),
            "文件内容".as_bytes()
        );
        assert!(right.join("资料 空格/空目录").is_dir());

        std::fs::write(right.join("反向.txt"), "reverse").unwrap();
        ui.invoke_refresh(1);
        wait_for(&ui, &state, &rx, |s| !s.panes[1].loading);
        let index = state.borrow().panes[1]
            .entries
            .iter()
            .position(|e| e.name == "反向.txt")
            .unwrap();
        ui.invoke_toggle(1, index as i32);
        ui.invoke_copy_selected(1);
        wait_for(&ui, &state, &rx, |s| {
            !s.busy && s.panes.iter().all(|p| !p.loading)
        });
        assert_eq!(std::fs::read(left.join("反向.txt")).unwrap(), b"reverse");

        let directory_name = ElementHandle::find_by_element_id(&ui, "FilePane::row-touch")
            .next()
            .unwrap();
        directory_name.mock_single_click(PointerEventButton::Left);
        directory_name.mock_single_click(PointerEventButton::Left);
        wait_for(&ui, &state, &rx, |s| !s.panes[0].loading);
        assert!(state.borrow().panes[0].path.ends_with("资料 空格"));
        ui.invoke_parent_directory(0);
        wait_for(&ui, &state, &rx, |s| !s.panes[0].loading);
        assert_eq!(state.borrow().panes[0].entries.len(), 3);

        let mut native = DataTransfer::default();
        native.set_file_paths([left.join("测试文件.txt"), left.join("资料 空格")]);
        assert_eq!(data_paths(&native).len(), 2);
        assert_eq!(data_paths(&DataTransfer::default()).len(), 0);
        snapshot(&ui, "prototype-complete.png");

        // Exercise remote UI events without connecting to any user host.
        {
            let mut state = state.borrow_mut();
            state.panes[1].remote = true;
            state.panes[1].generation += 1;
            state.remote_config = RemoteConfig {
                route: None,
                jump_host: "bastion.example.com".into(),
                jump_port: "22".into(),
                jump_user: "jump-user".into(),
                windows_host: "100.64.0.2".into(),
                windows_port: "22".into(),
                windows_user: "home-user".into(),
                windows_root: "C:/Users/home".into(),
            };
        }
        paint_remote_config(&ui, &state.borrow().remote_config);
        let generation = state.borrow().panes[1].generation;
        let entries = vec![files::Entry {
            name: "远程资料".into(),
            path: PathBuf::from("C:/Users/home/远程资料"),
            directory: true,
            link: false,
            size: 0,
            modified: "2026-10-21 10:00 UTC".into(),
        }];
        state
            .borrow()
            .tx
            .send(Event::Directory(
                1,
                generation,
                Ok((PathBuf::from("C:/Users/home"), entries)),
            ))
            .unwrap();
        poll(&ui, &state, &rx);
        assert!(ui.get_right_remote());
        assert_eq!(ui.get_right_rows().row_count(), 1);
        assert!(ui.get_remote_snapshot().contains("已断开"));
        snapshot(&ui, "remote-browser.png");
        // Remote copies are exercised against a real configured host only; this
        // UI test verifies that no remote path is exposed as a local payload.
        assert!(!state.borrow().busy);
        assert!(data_paths(&ui.invoke_make_payload(1, 0, ui.get_revision())).is_empty());
        state
            .borrow()
            .tx
            .send(Event::Directory(
                1,
                generation - 1,
                Ok((right.clone(), Vec::new())),
            ))
            .unwrap();
        poll(&ui, &state, &rx);
        assert_eq!(state.borrow().panes[1].path, PathBuf::from("C:/Users/home"));
        state
            .borrow()
            .tx
            .send(Event::Directory(
                1,
                generation,
                Err(std::io::Error::other("模拟认证失败")),
            ))
            .unwrap();
        poll(&ui, &state, &rx);
        assert_eq!(
            ui.get_right_rows().row_count(),
            1,
            "keep last snapshot on error"
        );
        snapshot(&ui, "remote-error.png");

        let (reply, answer) = mpsc::channel();
        state
            .borrow()
            .tx
            .send(Event::RemotePrompt(
                generation,
                remote::Prompt::HostKey {
                    hop: "跳板机".into(),
                    details: "ED25519 key fingerprint is SHA256:example.\n请核对主机指纹。".into(),
                },
                reply,
            ))
            .unwrap();
        poll(&ui, &state, &rx);
        assert!(ui.get_auth_visible() && ui.get_auth_host_key());
        snapshot(&ui, "remote-host-key.png");
        ui.invoke_auth_submit();
        assert_eq!(answer.recv().unwrap().unwrap().as_str(), "yes");

        let (reply, answer) = mpsc::channel();
        state
            .borrow()
            .tx
            .send(Event::RemotePrompt(
                generation,
                remote::Prompt::Secret {
                    hop: "家里 Windows".into(),
                },
                reply,
            ))
            .unwrap();
        poll(&ui, &state, &rx);
        ui.set_auth_secret("ui-test-secret".into());
        snapshot(&ui, "remote-auth.png");
        ui.invoke_auth_submit();
        assert_eq!(answer.recv().unwrap().unwrap().as_str(), "ui-test-secret");
        assert!(ui.get_auth_secret().is_empty());
        assert!(!ui.get_history().contains("ui-test-secret"));

        let (reply, answer) = mpsc::channel();
        state
            .borrow()
            .tx
            .send(Event::RemotePrompt(
                generation,
                remote::Prompt::Secret {
                    hop: "跳板机".into(),
                },
                reply,
            ))
            .unwrap();
        poll(&ui, &state, &rx);
        ui.invoke_auth_cancel();
        assert!(answer.recv().unwrap().is_none());
        assert!(state.borrow().remote_cancel.load(Ordering::Relaxed));
        ui.invoke_show_settings();
        ui.set_setting_jump_host("bastion.example.com".into());
        ui.set_setting_windows_host("100.64.0.2".into());
        ui.set_setting_windows_root("C:/Users/home".into());
        snapshot(&ui, "remote-settings.png");
        ui.invoke_cancel_settings();
        ui.hide().unwrap();
    }
}
