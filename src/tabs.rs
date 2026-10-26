//! Each tab owns its workers, event receiver, credentials and presentation state.
use super::*;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

#[derive(Clone, Serialize, Deserialize)]
struct SavedTabs {
    version: u32,
    active: usize,
    connections: Vec<RemoteConfig>,
}
impl SavedTabs {
    fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let value: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if value.version != 1
                    || value.connections.is_empty()
                    || value.active >= value.connections.len()
                {
                    return Err(io::Error::other("标签配置格式或版本无效"));
                }
                Ok(value)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
                version: 1,
                active: 0,
                connections: vec![RemoteConfig::load_from(
                    &path.with_file_name("config.conf"),
                )?],
            }),
            Err(e) => Err(e),
        }
    }
    fn save(&self, path: &Path) -> io::Result<PathBuf> {
        use std::io::Write;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("配置路径无父目录"))?;
        fs::create_dir_all(parent)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = parent.join(format!("profiles-{}-{stamp}.tmp", std::process::id()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)?;
            Ok(path.to_path_buf())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

#[derive(Clone)]
struct Presentation {
    status: slint::SharedString,
    detail: slint::SharedString,
    history: slint::SharedString,
    progress: f32,
    active_pane: i32,
    busy: bool,
    remote_reading: bool,
    auth_title: slint::SharedString,
    auth_detail: slint::SharedString,
    auth_visible: bool,
    auth_host_key: bool,
}
impl Default for Presentation {
    fn default() -> Self {
        Self {
            status: "准备就绪".into(),
            detail: "每个标签独立连接；切换后任务继续，关闭标签会取消其任务。".into(),
            history: "".into(),
            progress: 0.0,
            active_pane: 1,
            busy: false,
            remote_reading: false,
            auth_title: "".into(),
            auth_detail: "".into(),
            auth_visible: false,
            auth_host_key: false,
        }
    }
}
impl Presentation {
    fn capture(ui: &AppWindow) -> Self {
        Self {
            status: ui.get_status(),
            detail: ui.get_detail(),
            history: ui.get_history(),
            progress: ui.get_progress(),
            active_pane: ui.get_active_pane(),
            busy: ui.get_busy(),
            remote_reading: ui.get_remote_reading(),
            auth_title: ui.get_auth_title(),
            auth_detail: ui.get_auth_detail(),
            auth_visible: ui.get_auth_visible(),
            auth_host_key: ui.get_auth_host_key(),
        }
    }
    fn apply(&self, ui: &AppWindow) {
        ui.set_status(self.status.clone());
        ui.set_detail(self.detail.clone());
        ui.set_history(self.history.clone());
        ui.set_progress(self.progress);
        ui.set_active_pane(self.active_pane);
        ui.set_busy(self.busy);
        ui.set_remote_reading(self.remote_reading);
        ui.set_auth_title(self.auth_title.clone());
        ui.set_auth_detail(self.auth_detail.clone());
        ui.set_auth_visible(self.auth_visible);
        ui.set_auth_host_key(self.auth_host_key);
        // Never copy a typed password between views or tabs.
        ui.set_auth_secret("".into());
    }
}

struct Tab {
    id: u64,
    state: Shared,
    rx: Receiver<Event>,
    presentation: Presentation,
}
impl Tab {
    fn new(id: u64, config: RemoteConfig, local: &Path, local_right: Option<&Path>) -> Self {
        let remote = config.is_configured() && local_right.is_none();
        let (tx, rx) = mpsc::channel();
        let mut credentials = remote::CredentialCache::default();
        credentials.set_policy(config.cache_policy);
        let state = Rc::new(RefCell::new(State {
            config_writer: None,
            panes: [
                Pane {
                    path: local.into(),
                    ..Default::default()
                },
                Pane {
                    path: if remote {
                        PathBuf::from(&config.windows_root)
                    } else {
                        local_right.unwrap_or(local).into()
                    },
                    remote,
                    ..Default::default()
                },
            ],
            remote_config: config,
            remote_cancel: Arc::new(AtomicBool::new(false)),
            remote_worker: None,
            auth_reply: None,
            credentials: Arc::new(Mutex::new(credentials)),
            tx,
            cancel: Arc::new(AtomicBool::new(false)),
            busy: false,
            worker: None,
            external: None,
        }));
        Self {
            id,
            state,
            rx,
            presentation: Presentation::default(),
        }
    }
    fn title(&self) -> String {
        let state = self.state.borrow();
        let config = &state.remote_config;
        if !config.name.is_empty() {
            config.name.clone()
        } else if !config.windows_host.is_empty() {
            config.windows_host.clone()
        } else {
            "新连接".into()
        }
    }
    fn stop(&self) {
        let mut state = self.state.borrow_mut();
        state.cancel.store(true, Ordering::Relaxed);
        state.remote_cancel.store(true, Ordering::Relaxed);
        state.auth_reply.take();
        state.external.take();
        state.credentials.lock().unwrap().clear();
    }
    fn reap(&self, wait: bool) -> bool {
        let mut state = self.state.borrow_mut();
        if state
            .worker
            .as_ref()
            .is_some_and(|w| wait || w.is_finished())
        {
            let _ = state.worker.take().unwrap().join();
        }
        if state
            .remote_worker
            .as_ref()
            .is_some_and(|w| wait || w.is_finished())
        {
            let _ = state.remote_worker.take().unwrap().join();
        }
        state.worker.is_none() && state.remote_worker.is_none()
    }
}

struct Workspace {
    stopping: std::cell::Cell<bool>,
    tabs: Vec<Tab>,
    retired: Vec<Tab>,
    active: usize,
    next_id: u64,
    path: PathBuf,
    local: PathBuf,
    // One unshown view is an event-rendering sink, not one hidden window per tab.
    // It lets background events use the same reducer as the visible tab.
    background: AppWindow,
}
type SharedWorkspace = Rc<RefCell<Workspace>>;
impl Workspace {
    fn new(
        saved: SavedTabs,
        path: PathBuf,
        local: PathBuf,
        local_right: Option<PathBuf>,
    ) -> Result<SharedWorkspace, slint::PlatformError> {
        let mut tabs = Vec::new();
        for (index, config) in saved.connections.into_iter().enumerate() {
            tabs.push(Tab::new(
                index as u64,
                config,
                &local,
                if index == 0 {
                    local_right.as_deref()
                } else {
                    None
                },
            ));
        }
        let workspace = Rc::new(RefCell::new(Self {
            stopping: std::cell::Cell::new(false),
            next_id: tabs.len() as u64,
            tabs,
            retired: Vec::new(),
            active: saved.active,
            path,
            local,
            background: AppWindow::new()?,
        }));
        {
            let ws = workspace.borrow();
            for tab in &ws.tabs {
                Self::bind_writer(&workspace, tab);
            }
        }
        workspace.borrow_mut().initialize();
        Ok(workspace)
    }
    fn bind_writer(workspace: &SharedWorkspace, tab: &Tab) {
        let weak = Rc::downgrade(workspace);
        let id = tab.id;
        tab.state.borrow_mut().config_writer = Some(Rc::new(move |config| {
            let workspace = weak
                .upgrade()
                .ok_or_else(|| io::Error::other("窗口已关闭"))?;
            let ws = workspace.borrow();
            let mut saved = ws.saved();
            let index = ws
                .tabs
                .iter()
                .position(|t| t.id == id)
                .ok_or_else(|| io::Error::other("标签已关闭"))?;
            saved.connections[index] = config.clone();
            saved.save(&ws.path)
        }));
    }
    fn initialize_tab(background: &AppWindow, tab: &mut Tab) {
        tab.presentation.apply(background);
        for side in 0..2 {
            let (remote, path) = {
                let s = tab.state.borrow();
                (s.panes[side].remote, s.panes[side].path.clone())
            };
            if remote {
                paint_pane(background, &tab.state, side);
            } else {
                navigate(background, &tab.state, side, path);
            }
        }
        tab.presentation = Presentation::capture(background);
    }
    fn initialize(&mut self) {
        for tab in &mut self.tabs {
            Self::initialize_tab(&self.background, tab);
        }
    }
    fn saved(&self) -> SavedTabs {
        SavedTabs {
            version: 1,
            active: self.active,
            connections: self
                .tabs
                .iter()
                .map(|t| t.state.borrow().remote_config.clone())
                .collect(),
        }
    }
    fn show(&self, ui: &AppWindow) {
        let tab = &self.tabs[self.active];
        tab.presentation.apply(ui);
        ui.set_settings_visible(false);
        ui.set_settings_error("".into());
        paint_remote_config(ui, &tab.state.borrow().remote_config);
        paint_pane(ui, &tab.state, 0);
        paint_pane(ui, &tab.state, 1);
        wire(ui, &tab.state);
        self.paint_tabs(ui);
    }
    fn paint_tabs(&self, ui: &AppWindow) {
        let rows: Vec<_> = self
            .tabs
            .iter()
            .map(|tab| {
                let state = tab.state.borrow();
                RemoteTabRow {
                    title: tab.title().into(),
                    busy: state.busy || state.remote_worker.is_some(),
                    attention: state.auth_reply.is_some(),
                }
            })
            .collect();
        let current: Vec<_> = ui.get_tabs().iter().collect();
        if current != rows {
            ui.set_tabs(ModelRc::new(VecModel::from(rows)));
        }
        ui.set_selected_tab(self.active as i32);
    }
    fn select(&mut self, ui: &AppWindow, index: usize) {
        if index >= self.tabs.len()
            || index == self.active
            || ui.get_settings_visible()
            || ui.get_auth_visible()
        {
            return;
        }
        self.tabs[self.active].presentation = Presentation::capture(ui);
        self.active = index;
        self.show(ui);
        if let Err(e) = self.saved().save(&self.path) {
            ui.set_history(format!("标签已切换，但保存失败：{e}").into());
        }
    }
    fn add(workspace: &SharedWorkspace, ui: &AppWindow) -> io::Result<()> {
        if ui.get_settings_visible() || ui.get_auth_visible() {
            return Ok(());
        }
        let mut ws = workspace.borrow_mut();
        let config = RemoteConfig {
            route: Some(String::new()),
            jump_port: "22".into(),
            windows_port: "22".into(),
            windows_root: "C:/".into(),
            ..Default::default()
        };
        let mut saved = ws.saved();
        saved.active = saved.connections.len();
        saved.connections.push(config.clone());
        saved.save(&ws.path)?;
        let active = ws.active;
        ws.tabs[active].presentation = Presentation::capture(ui);
        let mut tab = Tab::new(ws.next_id, config, &ws.local, None);
        ws.next_id += 1;
        Self::bind_writer(workspace, &tab);
        Self::initialize_tab(&ws.background, &mut tab);
        ws.tabs.push(tab);
        ws.active = ws.tabs.len() - 1;
        ws.show(ui);
        ui.invoke_show_settings();
        Ok(())
    }
    fn close(workspace: &SharedWorkspace, ui: &AppWindow, index: usize) -> io::Result<()> {
        if ui.get_settings_visible() || ui.get_auth_visible() {
            return Ok(());
        }
        let mut ws = workspace.borrow_mut();
        if index >= ws.tabs.len() {
            return Ok(());
        }
        let mut saved = ws.saved();
        saved.connections.remove(index);
        let replacement = saved.connections.is_empty();
        if replacement {
            saved.connections.push(RemoteConfig {
                jump_port: "22".into(),
                windows_port: "22".into(),
                windows_root: "C:/".into(),
                ..Default::default()
            });
        }
        saved.active = if index < ws.active {
            ws.active - 1
        } else {
            ws.active.min(saved.connections.len() - 1)
        };
        saved.save(&ws.path)?;
        let active = ws.active;
        ws.tabs[active].presentation = Presentation::capture(ui);
        let removed = ws.tabs.remove(index);
        removed.stop();
        ws.retired.push(removed);
        if replacement {
            let mut tab = Tab::new(ws.next_id, saved.connections[0].clone(), &ws.local, None);
            ws.next_id += 1;
            Self::bind_writer(workspace, &tab);
            Self::initialize_tab(&ws.background, &mut tab);
            ws.tabs.push(tab);
        }
        ws.active = saved.active;
        ws.show(ui);
        Ok(())
    }
    fn tick(&mut self, ui: &AppWindow) {
        if self.stopping.get() {
            return;
        }
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if index == self.active {
                poll(ui, &tab.state, &tab.rx);
                tab.presentation = Presentation::capture(ui);
            } else {
                tab.presentation.apply(&self.background);
                poll(&self.background, &tab.state, &tab.rx);
                tab.presentation = Presentation::capture(&self.background);
            }
        }
        self.retired.retain(|tab| !tab.reap(false));
        self.paint_tabs(ui);
    }
    fn stop_all(&self) {
        self.stopping.set(true);
        for tab in self.tabs.iter().chain(&self.retired) {
            tab.stop();
        }
    }
    fn join_all(&self) {
        for tab in self.tabs.iter().chain(&self.retired) {
            tab.reap(true);
        }
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let ui = AppWindow::new()?;
    let initial = std::env::current_dir()?;
    let args: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let path = config::config_path().with_file_name("profiles.json");
    let (mut saved, error) = match SavedTabs::load(&path) {
        Ok(saved) => (saved, None),
        Err(e) => (
            SavedTabs {
                version: 1,
                active: 0,
                connections: vec![RemoteConfig::default()],
            },
            Some(e),
        ),
    };
    if args.len() >= 2 {
        saved.active = 0;
    }
    let workspace = Workspace::new(
        saved,
        path,
        args.first().cloned().unwrap_or(initial),
        args.get(1).cloned(),
    )?;
    workspace.borrow().show(&ui);
    if let Some(error) = error {
        ui.set_history(format!("读取标签配置失败：{error}").into());
    }
    let weak = ui.as_weak();
    let shared = workspace.clone();
    ui.on_select_tab(move |index| {
        if index >= 0 {
            shared.borrow_mut().select(&weak.unwrap(), index as usize);
        }
    });
    let weak = ui.as_weak();
    let shared = workspace.clone();
    ui.on_add_tab(move || {
        let ui = weak.unwrap();
        if let Err(e) = Workspace::add(&shared, &ui) {
            ui.set_history(format!("新增标签失败：{e}").into());
        }
    });
    let weak = ui.as_weak();
    let shared = workspace.clone();
    ui.on_close_tab(move |index| {
        let ui = weak.unwrap();
        if index >= 0
            && let Err(e) = Workspace::close(&shared, &ui, index as usize)
        {
            ui.set_history(format!("关闭标签失败：{e}").into());
        }
    });
    let timer = Timer::default();
    let weak = ui.as_weak();
    let shared = workspace.clone();
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        if let Some(ui) = weak.upgrade() {
            shared.borrow_mut().tick(&ui);
        }
    });
    let shared = workspace.clone();
    ui.window().on_close_requested(move || {
        shared.borrow().stop_all();
        slint::CloseRequestResponse::HideWindow
    });
    let result = ui.run();
    workspace.borrow().stop_all();
    workspace.borrow().join_all();
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(name: &str) -> RemoteConfig {
        RemoteConfig {
            name: name.into(),
            route: Some(String::new()),
            windows_host: format!("{name}.test"),
            windows_port: "22".into(),
            windows_user: "user".into(),
            windows_root: "C:/".into(),
            ..Default::default()
        }
    }
    fn setup(root: &Path) -> (AppWindow, SharedWorkspace) {
        use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
        slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
            mock_time: true,
            threading: false,
            renderer_name: Some("software".into()),
        })))
        .unwrap();
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1180.0, 800.0));
        let workspace = Workspace::new(
            SavedTabs {
                version: 1,
                active: 0,
                connections: vec![connection("alpha"), connection("beta")],
            },
            root.join("profiles.json"),
            root.into(),
            None,
        )
        .unwrap();
        workspace.borrow().show(&ui);
        (ui, workspace)
    }
    #[test]
    fn migrates_legacy_configuration_and_roundtrips_multiple_profiles() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("config.conf");
        let contents = "windows_host=old.test\nwindows_user=user\nwindows_port=22\nwindows_root=C:/Users/user\ncredential_ttl=1\n";
        fs::write(&legacy, contents).unwrap();
        let path = root.path().join("profiles.json");
        let mut saved = SavedTabs::load(&path).unwrap();
        assert_eq!(saved.connections.len(), 1);
        assert_eq!(saved.connections[0].windows_host, "old.test");
        assert_eq!(saved.connections[0].cache_policy, CachePolicy::Session);
        saved.connections.push(connection("new"));
        saved.active = 1;
        saved.save(&path).unwrap();
        saved.save(&path).unwrap();
        let loaded = SavedTabs::load(&path).unwrap();
        assert_eq!(loaded.active, 1);
        assert_eq!(loaded.connections, saved.connections);
        assert_eq!(fs::read_to_string(legacy).unwrap(), contents);
        fs::write(&path, "broken").unwrap();
        assert!(SavedTabs::load(&path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
    }

    #[test]
    fn concurrent_results_progress_and_authentication_stay_in_their_tabs() {
        let root = tempfile::tempdir().unwrap();
        let (ui, workspace) = setup(root.path());
        let states: Vec<_> = workspace
            .borrow()
            .tabs
            .iter()
            .map(|t| t.state.clone())
            .collect();
        let gate = Arc::new(std::sync::Barrier::new(3));
        for (index, shared) in states.iter().enumerate() {
            let mut state = shared.borrow_mut();
            state.panes[1].generation = 1;
            state.panes[1].loading = true;
            state.credentials.lock().unwrap().put(
                "same-user@host:22",
                Zeroizing::new(format!("tab-secret-{index}")),
            );
            let tx = state.tx.clone();
            let gate = gate.clone();
            state.remote_worker = Some(thread::spawn(move || {
                gate.wait();
                tx.send(Event::Directory(
                    1,
                    1,
                    Ok((PathBuf::from(format!("C:/tab-{index}")), vec![])),
                ))
                .unwrap();
            }));
        }
        gate.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        while states.iter().any(|s| s.borrow().remote_worker.is_some()) {
            workspace.borrow_mut().tick(&ui);
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(ui.get_right_path(), "C:/tab-0");
        states[0]
            .borrow()
            .tx
            .send(Event::Progress(files::Progress {
                bytes: 1,
                total: 4,
                ..Default::default()
            }))
            .unwrap();
        states[1]
            .borrow()
            .tx
            .send(Event::Progress(files::Progress {
                bytes: 3,
                total: 4,
                ..Default::default()
            }))
            .unwrap();
        workspace.borrow_mut().tick(&ui);
        assert_eq!(ui.get_progress(), 0.25);
        workspace.borrow_mut().select(&ui, 1);
        assert_eq!(ui.get_right_path(), "C:/tab-1");
        assert_eq!(ui.get_progress(), 0.75);
        assert_eq!(
            states[0]
                .borrow()
                .credentials
                .lock()
                .unwrap()
                .get("same-user@host:22")
                .unwrap()
                .as_str(),
            "tab-secret-0"
        );
        assert_eq!(
            states[1]
                .borrow()
                .credentials
                .lock()
                .unwrap()
                .get("same-user@host:22")
                .unwrap()
                .as_str(),
            "tab-secret-1"
        );
        let (reply_a, answer_a) = mpsc::channel();
        let (reply_b, answer_b) = mpsc::channel();
        states[0]
            .borrow()
            .tx
            .send(Event::RemotePrompt(
                1,
                remote::Prompt::Secret { hop: "A".into() },
                reply_a,
            ))
            .unwrap();
        states[1]
            .borrow()
            .tx
            .send(Event::RemotePrompt(
                1,
                remote::Prompt::Secret { hop: "B".into() },
                reply_b,
            ))
            .unwrap();
        workspace.borrow_mut().tick(&ui);
        assert!(ui.get_auth_title().starts_with('B'));
        assert!(ui.get_tabs().row_data(0).unwrap().attention);
        ui.set_auth_secret("answer-B".into());
        ui.invoke_auth_submit();
        assert_eq!(answer_b.recv().unwrap().unwrap().as_str(), "answer-B");
        assert!(matches!(
            answer_a.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        workspace.borrow_mut().select(&ui, 0);
        assert!(ui.get_auth_title().starts_with('A'));
        assert!(ui.get_auth_secret().is_empty());
        ui.set_auth_secret("answer-A".into());
        ui.invoke_auth_submit();
        assert_eq!(answer_a.recv().unwrap().unwrap().as_str(), "answer-A");

        // Saving the selected profile does not overwrite another tab's settings.
        ui.invoke_show_settings();
        ui.set_setting_tab_name("renamed-alpha".into());
        ui.invoke_save_settings(
            ui.get_setting_jump_host(),
            ui.get_setting_jump_port(),
            ui.get_setting_jump_user(),
            ui.get_setting_windows_host(),
            ui.get_setting_windows_port(),
            ui.get_setting_windows_user(),
            ui.get_setting_windows_root(),
            ui.get_setting_route(),
        );
        assert!(!ui.get_settings_visible(), "{}", ui.get_settings_error());
        let saved = SavedTabs::load(&root.path().join("profiles.json")).unwrap();
        assert_eq!(saved.connections[0].name, "renamed-alpha");
        assert_eq!(saved.connections[1].name, "beta");
        assert!(
            !fs::read_to_string(root.path().join("profiles.json"))
                .unwrap()
                .contains("tab-secret")
        );

        workspace.borrow_mut().tick(&ui);
        ui.show().unwrap();
        let pixels = ui.window().take_snapshot().unwrap();
        fs::create_dir_all("target/qa").unwrap();
        image::save_buffer(
            "target/qa/remote-tabs.png",
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();

        Workspace::close(&workspace, &ui, 1).unwrap();
        assert!(states[1].borrow().remote_cancel.load(Ordering::Relaxed));
        assert!(!states[0].borrow().remote_cancel.load(Ordering::Relaxed));
        let _ = states[1].borrow().tx.send(Event::Directory(
            1,
            1,
            Ok(("C:/late-closed-result".into(), vec![])),
        ));
        workspace.borrow_mut().tick(&ui);
        assert_ne!(ui.get_right_path(), "C:/late-closed-result");
        Workspace::close(&workspace, &ui, 0).unwrap();
        assert_eq!(workspace.borrow().tabs.len(), 1);
        assert!(
            workspace.borrow().tabs[0]
                .state
                .borrow()
                .remote_config
                .windows_host
                .is_empty()
        );
        workspace.borrow().stop_all();
        workspace.borrow().join_all();
        ui.hide().unwrap();
    }

    #[test]
    fn closing_a_running_tab_does_not_cancel_another_worker() {
        let root = tempfile::tempdir().unwrap();
        let (ui, workspace) = setup(root.path());
        let states: Vec<_> = workspace
            .borrow()
            .tabs
            .iter()
            .map(|t| t.state.clone())
            .collect();
        let gate = Arc::new(std::sync::Barrier::new(3));
        for state in &states {
            let cancel = state.borrow().remote_cancel.clone();
            let gate = gate.clone();
            state.borrow_mut().remote_worker = Some(thread::spawn(move || {
                gate.wait();
                while !cancel.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(5));
                }
            }));
        }
        gate.wait();
        Workspace::close(&workspace, &ui, 1).unwrap();
        assert!(states[1].borrow().remote_cancel.load(Ordering::Relaxed));
        assert!(!states[0].borrow().remote_cancel.load(Ordering::Relaxed));
        assert!(
            !states[0]
                .borrow()
                .remote_worker
                .as_ref()
                .unwrap()
                .is_finished()
        );
        workspace.borrow().stop_all();
        let generation = states[0].borrow().panes[0].generation;
        states[0]
            .borrow()
            .tx
            .send(Event::Finished(files::Report::default()))
            .unwrap();
        workspace.borrow_mut().tick(&ui);
        assert_eq!(
            states[0].borrow().panes[0].generation,
            generation,
            "shutdown must not start refresh workers from late completion events"
        );
        workspace.borrow().join_all();
    }

    #[test]
    fn adding_closing_and_reloading_preserves_the_correct_configuration() {
        let root = tempfile::tempdir().unwrap();
        let (ui, workspace) = setup(root.path());
        workspace.borrow_mut().select(&ui, 1);
        let beta_id = workspace.borrow().tabs[1].id;
        Workspace::close(&workspace, &ui, 0).unwrap();
        assert_eq!(workspace.borrow().tabs[0].id, beta_id);
        assert_eq!(ui.get_setting_windows_host(), "beta.test");
        Workspace::add(&workspace, &ui).unwrap();
        assert!(ui.get_settings_visible());
        assert_eq!(workspace.borrow().tabs.len(), 2);
        ui.set_setting_tab_name("delta".into());
        ui.set_setting_windows_host("delta.test".into());
        ui.set_setting_windows_user("user".into());
        ui.invoke_save_settings(
            ui.get_setting_jump_host(),
            ui.get_setting_jump_port(),
            ui.get_setting_jump_user(),
            ui.get_setting_windows_host(),
            ui.get_setting_windows_port(),
            ui.get_setting_windows_user(),
            ui.get_setting_windows_root(),
            ui.get_setting_route(),
        );
        assert!(!ui.get_settings_visible(), "{}", ui.get_settings_error());
        let saved = SavedTabs::load(&root.path().join("profiles.json")).unwrap();
        assert_eq!(saved.active, 1);
        assert_eq!(
            saved
                .connections
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["beta", "delta"]
        );
        let reopened = Workspace::new(
            saved,
            root.path().join("profiles.json"),
            root.path().into(),
            None,
        )
        .unwrap();
        assert_eq!(reopened.borrow().active, 1);
        assert!(
            reopened
                .borrow()
                .tabs
                .iter()
                .all(|t| t.state.borrow().remote_worker.is_none())
        );
        workspace.borrow().stop_all();
        workspace.borrow().join_all();
        reopened.borrow().stop_all();
        reopened.borrow().join_all();
    }
}
