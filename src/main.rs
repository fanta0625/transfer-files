mod files;

use slint::{
    ComponentHandle, DataTransfer, Model, ModelRc, Timer, TimerMode, VecModel, language::DragAction,
};
use std::{
    cell::RefCell,
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

slint::include_modules!();

#[derive(Default)]
struct Pane {
    path: PathBuf,
    entries: Vec<files::Entry>,
    selected: HashSet<PathBuf>,
    generation: u64,
    loading: bool,
}

enum Event {
    Directory(usize, u64, std::io::Result<(PathBuf, Vec<files::Entry>)>),
    Progress(files::Progress),
    Finished(files::Report),
}

struct State {
    panes: [Pane; 2],
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

fn navigate(ui: &AppWindow, state: &Shared, side: usize, path: PathBuf) {
    let (generation, tx) = {
        let mut state = state.borrow_mut();
        let pane = &mut state.panes[side];
        pane.generation += 1;
        pane.loading = true;
        (pane.generation, state.tx.clone())
    };
    paint_pane(ui, state, side);
    thread::spawn(move || {
        let result = files::read_directory(&path);
        let _ = tx.send(Event::Directory(side, generation, result));
    });
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

fn wire(ui: &AppWindow, state: &Shared) {
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_navigate(move |side, path| {
        let ui = weak.unwrap();
        let side = side as usize;
        let mut path = PathBuf::from(path.as_str());
        if path.is_relative() {
            path = shared.borrow().panes[side].path.join(path);
        }
        navigate(&ui, &shared, side, path);
    });
    let weak = ui.as_weak();
    let shared = state.clone();
    ui.on_parent_directory(move |side| {
        let side = side as usize;
        let path = shared.borrow().panes[side].path.parent().map(PathBuf::from);
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
        if state.busy || state.panes[side as usize].loading || data_paths(&event.data).is_empty() {
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
            Event::Directory(side, generation, result) => {
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
                        }
                        Err(error) => ui.set_history(format!("读取目录失败：{error}").into()),
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
    let paths = [
        args.first().cloned().unwrap_or_else(|| initial.clone()),
        args.get(1).cloned().unwrap_or_else(|| initial.clone()),
    ];
    let (tx, rx) = mpsc::channel();
    let state = Rc::new(RefCell::new(State {
        panes: [
            Pane {
                path: initial.clone(),
                ..Pane::default()
            },
            Pane {
                path: initial,
                ..Pane::default()
            },
        ],
        tx,
        cancel: Arc::new(AtomicBool::new(false)),
        busy: false,
        worker: None,
        external: None,
    }));
    ui.set_active_pane(1);
    wire(&ui, &state);
    for (side, path) in paths.into_iter().enumerate() {
        navigate(&ui, &state, side, path);
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
        slint::CloseRequestResponse::HideWindow
    });
    ui.run()?;
    state.borrow().cancel.store(true, Ordering::Relaxed);
    if let Some(worker) = state.borrow_mut().worker.take() {
        let _ = worker.join();
    }
    Ok(())
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
    use slint::{LogicalPosition, LogicalSize, platform::PointerEventButton};

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
                    ..Pane::default()
                },
                Pane {
                    path: right.clone(),
                    ..Pane::default()
                },
            ],
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
        ui.hide().unwrap();
    }
}
