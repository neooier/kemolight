use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
        Arc, Mutex,
    },
    thread,
};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::{
    brightness::{
        apply_step, clamp_percent, create_backend, BrightnessBackend, BrightnessValue, DisplayId,
        DisplayInfo, DisplayKind, Result,
    },
    config::AppConfig,
    profiles::{persistent_id, profiles_path, BrightnessProfiles, ProfileSlot},
    tray, ui,
};

const APP_TITLE: &str = "Brightness Controller";
const APP_ID: &str = "dev.kemolight.BrightnessController";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayState {
    pub info: DisplayInfo,
    pub pending: bool,
    pub error: Option<String>,
    latest_request_id: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSnapshot {
    pub displays: Vec<DisplayState>,
    pub last_error: Option<String>,
    pub profiles: BrightnessProfiles,
    pub profile_message: Option<String>,
    pub profile_error: Option<String>,
    pub hotkey_error: Option<String>,
    pub(crate) lid_closed: bool,
    #[cfg(target_os = "windows")]
    #[serde(skip)]
    pub(crate) monitor_targets: Vec<(isize, DisplayId)>,
    #[cfg(target_os = "windows")]
    #[serde(skip)]
    pub(crate) monitor_generation: u64,
}

#[cfg(target_os = "windows")]
impl AppSnapshot {
    fn wheel_target(&self, monitor: isize) -> Option<&DisplayState> {
        let (_, id) = self
            .monitor_targets
            .iter()
            .find(|(handle, _)| *handle == monitor)?;
        self.displays
            .iter()
            .find(|display| display.info.id == *id && display.info.supported)
    }
}

#[derive(Debug, Clone)]
enum BrightnessCommand {
    Refresh,
    SaveProfile {
        slot: ProfileSlot,
        displays: Vec<DisplayInfo>,
        path: Option<std::path::PathBuf>,
    },
    Set {
        display_id: DisplayId,
        value: u32,
        request_id: u64,
    },
    Adjust {
        display_id: DisplayId,
        delta: i32,
        request_id: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum AppCommand {
    OpenWindow,
    HideWindow,
    Quit,
    #[cfg(target_os = "windows")]
    UiClosed(u64),
}

#[derive(Clone)]
pub struct AppController {
    inner: Arc<AppControllerInner>,
}

struct AppControllerInner {
    config: AppConfig,
    snapshot: Arc<Mutex<AppSnapshot>>,
    brightness_tx: Sender<BrightnessCommand>,
    state_tx: SyncSender<()>,
    state_rx: Mutex<Receiver<()>>,
    repaint_ctx: Arc<Mutex<Option<egui::Context>>>,
    tray_update_tx: Arc<Mutex<Option<SyncSender<()>>>>,
    app_tx: Sender<AppCommand>,
    app_rx: Mutex<Receiver<AppCommand>>,
    tray_available: AtomicBool,
    is_quitting: AtomicBool,
    next_request_id: AtomicU64,
    profiles_path: Option<std::path::PathBuf>,
    window_geometry: Mutex<WindowGeometry>,
    #[cfg(target_os = "windows")]
    remote: Option<crate::ui_process::RemoteClient>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct WindowGeometry {
    pub(crate) inner_size: Option<[f32; 2]>,
    pub(crate) outer_position: Option<[f32; 2]>,
}

impl AppController {
    pub fn new(config: AppConfig) -> Self {
        let backend = create_backend(config.preferred_backend);
        Self::with_backend_and_profiles_path(config, backend, profiles_path())
    }

    pub fn with_backend(config: AppConfig, backend: Arc<dyn BrightnessBackend>) -> Self {
        Self::with_backend_and_profiles_path(config, backend, None)
    }

    fn with_backend_and_profiles_path(
        config: AppConfig,
        backend: Arc<dyn BrightnessBackend>,
        profiles_path: Option<std::path::PathBuf>,
    ) -> Self {
        let mut initial = AppSnapshot::default();
        if let Some(path) = &profiles_path {
            match BrightnessProfiles::load(path) {
                Ok(profiles) => initial.profiles = profiles,
                Err(error) => initial.profile_error = Some(error),
            }
        }
        let (controller, brightness_rx) = Self::with_snapshot(config, initial, profiles_path);
        let inner = &controller.inner;
        spawn_brightness_worker(
            backend,
            Arc::clone(&inner.snapshot),
            brightness_rx,
            inner.state_tx.clone(),
            Arc::clone(&inner.repaint_ctx),
            Arc::clone(&inner.tray_update_tx),
            inner.config.normalized_step(),
        );
        controller
    }

    fn with_snapshot(
        config: AppConfig,
        initial: AppSnapshot,
        profiles_path: Option<std::path::PathBuf>,
    ) -> (Self, Receiver<BrightnessCommand>) {
        let (brightness_tx, brightness_rx) = mpsc::channel();
        // These are wakeups, not state: one pending notification is sufficient.
        let (state_tx, state_rx) = mpsc::sync_channel(1);
        let (app_tx, app_rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(initial));
        let repaint_ctx = Arc::new(Mutex::new(None));
        let tray_update_tx = Arc::new(Mutex::new(None));

        (
            Self {
                inner: Arc::new(AppControllerInner {
                    config,
                    snapshot,
                    brightness_tx,
                    state_tx,
                    state_rx: Mutex::new(state_rx),
                    repaint_ctx,
                    tray_update_tx,
                    app_tx,
                    app_rx: Mutex::new(app_rx),
                    tray_available: AtomicBool::new(false),
                    is_quitting: AtomicBool::new(false),
                    next_request_id: AtomicU64::new(1),
                    profiles_path,
                    window_geometry: Mutex::new(WindowGeometry::default()),
                    #[cfg(target_os = "windows")]
                    remote: None,
                }),
            },
            brightness_rx,
        )
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn with_remote(
        config: AppConfig,
        snapshot: AppSnapshot,
        geometry: WindowGeometry,
        tray_available: bool,
        remote: crate::ui_process::RemoteClient,
    ) -> Self {
        let (mut controller, _) = Self::with_snapshot(config, snapshot, None);
        Arc::get_mut(&mut controller.inner)
            .expect("new controller is unshared")
            .remote = Some(remote);
        controller.set_window_geometry(geometry);
        controller.set_tray_available(tray_available);
        controller
    }

    pub fn config(&self) -> &AppConfig {
        &self.inner.config
    }

    pub fn set_repaint_context(&self, ctx: egui::Context) {
        if let Ok(mut repaint_ctx) = self.inner.repaint_ctx.lock() {
            *repaint_ctx = Some(ctx.clone());
        }
        ctx.request_repaint();
    }

    pub(crate) fn clear_repaint_context(&self) {
        if let Ok(mut repaint_ctx) = self.inner.repaint_ctx.lock() {
            *repaint_ctx = None;
        }
    }

    pub(crate) fn remember_window_geometry(&self, ctx: &egui::Context) {
        if let Ok(mut geometry) = self.inner.window_geometry.lock() {
            ctx.input(|input| {
                let viewport = input.viewport();
                geometry.inner_size = viewport.inner_rect.map(|rect| rect.size().into());
                geometry.outer_position = viewport.outer_rect.map(|rect| rect.min.into());
            });
        }
        #[cfg(target_os = "windows")]
        if let Some(remote) = &self.inner.remote {
            remote.send(
                self,
                crate::ui_process::UiCommand::Closing(self.window_geometry()),
            );
        }
    }

    pub(crate) fn window_geometry(&self) -> WindowGeometry {
        self.inner
            .window_geometry
            .lock()
            .map(|geometry| *geometry)
            .unwrap_or_default()
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn set_window_geometry(&self, geometry: WindowGeometry) {
        if let Ok(mut current) = self.inner.window_geometry.lock() {
            *current = geometry;
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn window_shown(&self) {
        if let Some(remote) = &self.inner.remote {
            remote.send(self, crate::ui_process::UiCommand::Shown);
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn replace_snapshot(&self, value: AppSnapshot) {
        if let Ok(mut snapshot) = self.inner.snapshot.lock() {
            *snapshot = value;
        }
        notify_state_changed(&self.inner.state_tx);
        self.request_repaint();
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn wait_for_state_event(&self) -> bool {
        self.inner.state_rx.lock().is_ok_and(|rx| rx.recv().is_ok())
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn wait_for_app_command(&self) -> Option<AppCommand> {
        self.inner.app_rx.lock().ok().and_then(|rx| rx.recv().ok())
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn ui_closed(&self, session: u64) {
        let _ = self.inner.app_tx.send(AppCommand::UiClosed(session));
    }

    pub fn snapshot(&self) -> AppSnapshot {
        self.inner
            .snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|_| AppSnapshot {
                displays: Vec::new(),
                last_error: Some("application state lock was poisoned".to_string()),
                ..Default::default()
            })
    }

    pub fn refresh(&self) {
        #[cfg(target_os = "windows")]
        if let Some(remote) = &self.inner.remote {
            remote.send(self, crate::ui_process::UiCommand::Refresh);
            return;
        }
        let _ = self.inner.brightness_tx.send(BrightnessCommand::Refresh);
    }

    pub fn can_save_profile(&self) -> bool {
        let snapshot = self.snapshot();
        !snapshot.displays.iter().any(|display| display.pending)
            && snapshot.displays.iter().any(|display| {
                display.info.supported
                    && display.info.current.is_some()
                    && display.error.is_none()
                    && persistent_id(&display.info.id)
            })
    }

    pub fn can_apply_profile(&self, slot: ProfileSlot) -> bool {
        let snapshot = self.snapshot();
        let displays: Vec<_> = snapshot
            .displays
            .iter()
            .map(|display| display.info.clone())
            .collect();
        !snapshot.profiles.targets(slot, &displays).is_empty()
    }

    pub fn save_profile(&self, slot: ProfileSlot) {
        #[cfg(target_os = "windows")]
        if let Some(remote) = &self.inner.remote {
            remote.send(self, crate::ui_process::UiCommand::SaveProfile(slot));
            return;
        }
        let snapshot = self.snapshot();
        if snapshot.displays.iter().any(|display| display.pending) {
            self.profile_feedback(
                None,
                Some("Wait for brightness changes to finish before saving.".into()),
            );
            return;
        }
        let displays = snapshot
            .displays
            .into_iter()
            .filter(|display| display.error.is_none())
            .map(|display| display.info)
            .collect();
        let _ = self
            .inner
            .brightness_tx
            .send(BrightnessCommand::SaveProfile {
                slot,
                displays,
                path: self.inner.profiles_path.clone(),
            });
    }

    pub fn apply_profile(&self, slot: ProfileSlot) {
        #[cfg(target_os = "windows")]
        if let Some(remote) = &self.inner.remote {
            remote.send(self, crate::ui_process::UiCommand::ApplyProfile(slot));
            return;
        }
        let snapshot = self.snapshot();
        let displays: Vec<_> = snapshot
            .displays
            .into_iter()
            .map(|display| display.info)
            .collect();
        let targets = snapshot.profiles.targets(slot, &displays);
        if targets.is_empty() {
            self.profile_feedback(
                None,
                Some(format!(
                    "{} has no saved brightness for connected displays.",
                    slot.label()
                )),
            );
            return;
        }
        let count = targets.len();
        for (id, brightness) in targets {
            self.set_brightness(id, brightness);
        }
        self.profile_feedback(
            Some(format!("Applying {} to {count} display(s).", slot.label())),
            None,
        );
    }

    fn profile_feedback(&self, message: Option<String>, error: Option<String>) {
        if let Ok(mut snapshot) = self.inner.snapshot.lock() {
            snapshot.profile_message = message;
            snapshot.profile_error = error;
        }
        notify_state_changed(&self.inner.state_tx);
        self.request_repaint();
        self.request_tray_update();
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn set_hotkey_error(&self, error: Option<String>) {
        if let Ok(mut snapshot) = self.inner.snapshot.lock() {
            snapshot.hotkey_error = error;
        }
        notify_state_changed(&self.inner.state_tx);
        self.request_repaint();
    }

    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn set_lid_closed(&self, closed: bool) {
        if let Ok(mut snapshot) = self.inner.snapshot.lock() {
            snapshot.lid_closed = closed;
            if closed {
                snapshot
                    .displays
                    .retain(|display| display.info.kind != DisplayKind::InternalBacklight);
            }
        }
        notify_state_changed(&self.inner.state_tx);
        self.request_repaint();
        self.request_tray_update();
    }

    pub fn set_brightness(&self, display_id: DisplayId, value: u32) {
        let value = clamp_percent(value);
        #[cfg(target_os = "windows")]
        if let Some(remote) = &self.inner.remote {
            remote.set_brightness(self, display_id, value);
            return;
        }
        let request_id = self.preview_brightness(display_id.clone(), value);
        self.commit_brightness(display_id, value, request_id);
    }

    pub fn adjust_brightness(&self, display_id: DisplayId, delta: i32) {
        if let Some(current) = self.current_brightness(&display_id) {
            self.set_brightness(
                display_id,
                apply_step(current, delta, self.inner.config.normalized_step()),
            );
        } else {
            #[cfg(target_os = "windows")]
            if let Some(remote) = &self.inner.remote {
                remote.send(
                    self,
                    crate::ui_process::UiCommand::Adjust { display_id, delta },
                );
                return;
            }
            let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
            begin_brightness_update(&self.inner.snapshot, &display_id, request_id);
            notify_state_changed(&self.inner.state_tx);
            self.request_repaint();
            let _ = self.inner.brightness_tx.send(BrightnessCommand::Adjust {
                display_id,
                delta,
                request_id,
            });
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn wheel_target_generation(&self, monitor: isize) -> Option<u64> {
        // This runs in the mouse hook. Contention must never block input.
        let snapshot = self.inner.snapshot.try_lock().ok()?;
        snapshot.wheel_target(monitor)?;
        Some(snapshot.monitor_generation)
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn adjust_wheel_target(
        &self,
        monitor: isize,
        generation: u64,
        delta: i32,
    ) -> Option<u32> {
        let id = {
            let snapshot = self.inner.snapshot.lock().ok()?;
            if snapshot.monitor_generation != generation {
                return None;
            }
            snapshot.wheel_target(monitor)?.info.id.clone()
        };
        self.adjust_brightness(id, delta);
        self.wheel_brightness(monitor, generation)
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn wheel_brightness(&self, monitor: isize, generation: u64) -> Option<u32> {
        let snapshot = self.inner.snapshot.lock().ok()?;
        if snapshot.monitor_generation != generation {
            return None;
        }
        let display = snapshot.wheel_target(monitor)?;
        if display.error.is_some() {
            return None;
        }
        display.info.current
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn invalidate_monitor_targets(&self) {
        if let Ok(mut snapshot) = self.inner.snapshot.lock() {
            snapshot.monitor_targets.clear();
            snapshot.monitor_generation = snapshot.monitor_generation.wrapping_add(1);
        }
    }

    pub fn preview_brightness(&self, display_id: DisplayId, value: u32) -> u64 {
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        update_pending_brightness(
            &self.inner.snapshot,
            &display_id,
            clamp_percent(value),
            request_id,
        );
        notify_state_changed(&self.inner.state_tx);
        self.request_repaint();
        request_id
    }

    pub fn commit_brightness(&self, display_id: DisplayId, value: u32, request_id: u64) {
        #[cfg(target_os = "windows")]
        if let Some(remote) = &self.inner.remote {
            remote.set_brightness(self, display_id, clamp_percent(value));
            return;
        }
        let _ = self.inner.brightness_tx.send(BrightnessCommand::Set {
            display_id,
            value: clamp_percent(value),
            request_id,
        });
    }

    pub fn request_open_window(&self) {
        let _ = self.inner.app_tx.send(AppCommand::OpenWindow);
        self.request_repaint();
    }

    pub fn request_hide_window(&self) {
        let _ = self.inner.app_tx.send(AppCommand::HideWindow);
        self.request_repaint();
    }

    pub fn request_quit(&self) {
        let _ = self.inner.app_tx.send(AppCommand::Quit);
        self.request_repaint();
    }

    pub fn shutdown(&self) {
        if self.inner.is_quitting.swap(true, Ordering::Relaxed) {
            return;
        }
        #[cfg(target_os = "windows")]
        if let Some(remote) = &self.inner.remote {
            remote.send(self, crate::ui_process::UiCommand::Quit);
        }
        notify_state_changed(&self.inner.state_tx);
        self.set_tray_update_sender(None);
        let _ = self.inner.brightness_tx.send(BrightnessCommand::Shutdown);
    }

    pub fn set_tray_available(&self, available: bool) {
        self.inner
            .tray_available
            .store(available, Ordering::Relaxed);
    }

    pub fn tray_available(&self) -> bool {
        self.inner.tray_available.load(Ordering::Relaxed)
    }

    pub fn is_quitting(&self) -> bool {
        self.inner.is_quitting.load(Ordering::Relaxed)
    }

    fn set_tray_update_sender(&self, update_tx: Option<SyncSender<()>>) {
        let Ok(mut current) = self.inner.tray_update_tx.lock() else {
            return;
        };
        *current = update_tx;
    }

    fn request_tray_update(&self) {
        notify_tray_changed(&self.inner.tray_update_tx);
    }

    fn request_repaint(&self) {
        request_repaint(&self.inner.repaint_ctx);
    }

    pub fn drain_state_events(&self) -> bool {
        let Ok(rx) = self.inner.state_rx.lock() else {
            return true;
        };
        let mut changed = false;
        while rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }

    pub fn drain_app_commands(&self) -> Vec<AppCommand> {
        let Ok(rx) = self.inner.app_rx.lock() else {
            return vec![AppCommand::Quit];
        };
        let mut commands = Vec::new();
        while let Ok(command) = rx.try_recv() {
            commands.push(command);
        }
        commands
    }

    fn current_brightness(&self, display_id: &DisplayId) -> Option<u32> {
        self.inner.snapshot.lock().ok().and_then(|snapshot| {
            snapshot
                .displays
                .iter()
                .find(|display| &display.info.id == display_id)
                .and_then(|display| display.info.current)
        })
    }
}

pub fn run() {
    let mut config = AppConfig::load().unwrap_or_else(|err| {
        eprintln!("Failed to load config, using defaults: {err}");
        AppConfig::default()
    });
    config.start_hidden |= crate::config::start_hidden_requested();
    // The brightness worker performs initial discovery before processing commands.
    let controller = AppController::new(config);

    // A separate hidden window receives broadcasts even without a tray icon.
    #[cfg(target_os = "windows")]
    let _display_watcher = crate::display_events::DisplayWatcher::start(controller.clone())
        .map_err(|err| eprintln!("Display change notifications unavailable: {err}"))
        .ok();

    let tray_handle = tray::start_tray(controller.clone());
    controller.set_tray_available(tray_handle.is_available());
    controller.set_tray_update_sender(tray_handle.update_sender());
    controller.request_tray_update();

    #[cfg(target_os = "windows")]
    let result = crate::ui_process::run_sessions(&controller);

    #[cfg(not(target_os = "windows"))]
    let result = {
        let prefer_x11 = should_prefer_x11_for_hide_to_tray(controller.tray_available());
        run_window_sessions(&controller, || {
            let mut result = run_main_window(controller.clone(), prefer_x11);
            if result.is_err() && prefer_x11 {
                eprintln!(
                "Failed to start the X11/XWayland UI backend; falling back to the default backend."
            );
                result = run_main_window(controller.clone(), false);
            }
            result
        })
    };

    if let Err(err) = result {
        eprintln!("Failed to run UI: {err}");
    }

    controller.shutdown();
    drop(tray_handle);
}

#[cfg(any(not(target_os = "windows"), test))]
fn run_window_sessions(
    controller: &AppController,
    mut show_window: impl FnMut() -> eframe::Result,
) -> eframe::Result {
    let mut open = !controller.config().start_hidden || !controller.tray_available();
    loop {
        if open {
            let result = show_window();
            // The controller otherwise keeps egui's font atlas, layout caches,
            // and repaint callback alive after the renderer has been destroyed.
            controller.clear_repaint_context();
            result?;
        }
        if controller.is_quitting() || !controller.tray_available() {
            return Ok(());
        }

        // The tray and hardware workers run independently. Sleep without
        // creating a window/graphics context or polling while hidden.
        let command = controller
            .inner
            .app_rx
            .lock()
            .ok()
            .and_then(|rx| rx.recv().ok());
        match command {
            Some(AppCommand::OpenWindow) => open = true,
            Some(AppCommand::HideWindow) => open = false,
            Some(AppCommand::Quit) | None => return Ok(()),
            #[cfg(target_os = "windows")]
            Some(AppCommand::UiClosed(_)) => open = false,
        }
    }
}

pub(crate) fn run_main_window(controller: AppController, prefer_x11: bool) -> eframe::Result {
    let geometry = controller.window_geometry();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_TITLE)
        .with_app_id(APP_ID)
        .with_icon(crate::logo::app_icon(64))
        .with_inner_size(geometry.inner_size.unwrap_or([560.0, 320.0]))
        .with_min_inner_size([420.0, 240.0]);
    if let Some(position) = geometry.outer_position {
        viewport = viewport.with_position(position);
    }
    let mut native_options = eframe::NativeOptions {
        viewport,
        centered: geometry.outer_position.is_none(),
        run_and_return: true,
        ..Default::default()
    };
    configure_linux_backend(&mut native_options, prefer_x11);

    let app_controller = controller.clone();
    eframe::run_native(
        APP_TITLE,
        native_options,
        Box::new(move |creation_context| {
            app_controller.set_repaint_context(creation_context.egui_ctx.clone());
            Ok(Box::new(ui::main_window::MainWindow::new(
                app_controller.clone(),
                true,
            )))
        }),
    )
}

#[cfg(target_os = "linux")]
fn should_prefer_x11_for_hide_to_tray(tray_available: bool) -> bool {
    let display = std::env::var("DISPLAY").ok();
    should_prefer_x11_for_hide_to_tray_with_display(tray_available, display.as_deref())
}

#[cfg(target_os = "linux")]
fn should_prefer_x11_for_hide_to_tray_with_display(
    tray_available: bool,
    display: Option<&str>,
) -> bool {
    tray_available && display.is_some_and(|display| !display.is_empty())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn should_prefer_x11_for_hide_to_tray(_tray_available: bool) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn configure_linux_backend(native_options: &mut eframe::NativeOptions, prefer_x11: bool) {
    if !prefer_x11 {
        return;
    }

    // winit cannot hide and later re-show a Wayland window. X11/XWayland supports
    // the reversible unmap/map behavior needed for a tray-first desktop app.
    native_options.event_loop_builder = Some(Box::new(|builder| {
        use winit::platform::x11::EventLoopBuilderExtX11;

        builder.with_x11();
    }));
}

#[cfg(not(target_os = "linux"))]
fn configure_linux_backend(_native_options: &mut eframe::NativeOptions, _prefer_x11: bool) {}

fn spawn_brightness_worker(
    backend: Arc<dyn BrightnessBackend>,
    snapshot: Arc<Mutex<AppSnapshot>>,
    rx: Receiver<BrightnessCommand>,
    state_tx: SyncSender<()>,
    repaint_ctx: Arc<Mutex<Option<egui::Context>>>,
    tray_update_tx: Arc<Mutex<Option<SyncSender<()>>>>,
    step: u32,
) {
    thread::spawn(move || {
        refresh_snapshot(backend.as_ref(), &snapshot);
        notify_state_changed(&state_tx);
        request_repaint(&repaint_ctx);
        notify_tray_changed(&tray_update_tx);

        while let Ok(command) = rx.recv() {
            match command {
                BrightnessCommand::Refresh => refresh_snapshot(backend.as_ref(), &snapshot),
                BrightnessCommand::SaveProfile {
                    slot,
                    displays,
                    path,
                } => {
                    save_profile_snapshot(&snapshot, slot, &displays, path.as_deref());
                }
                BrightnessCommand::Set {
                    display_id,
                    value,
                    request_id,
                } => set_display_brightness(
                    backend.as_ref(),
                    &snapshot,
                    &display_id,
                    value,
                    request_id,
                ),
                BrightnessCommand::Adjust {
                    display_id,
                    delta,
                    request_id,
                } => adjust_display_brightness(
                    backend.as_ref(),
                    &snapshot,
                    &display_id,
                    delta,
                    step,
                    request_id,
                ),
                BrightnessCommand::Shutdown => break,
            }
            notify_state_changed(&state_tx);
            request_repaint(&repaint_ctx);
            notify_tray_changed(&tray_update_tx);
        }
    });
}

fn notify_state_changed(state_tx: &SyncSender<()>) {
    let _ = state_tx.try_send(());
}

fn request_repaint(repaint_ctx: &Arc<Mutex<Option<egui::Context>>>) {
    let Ok(repaint_ctx) = repaint_ctx.lock() else {
        return;
    };
    if let Some(ctx) = repaint_ctx.as_ref() {
        ctx.request_repaint();
    }
}

fn notify_tray_changed(tray_update_tx: &Arc<Mutex<Option<SyncSender<()>>>>) {
    let Ok(current) = tray_update_tx.lock() else {
        return;
    };
    if let Some(update_tx) = current.as_ref() {
        let _ = update_tx.try_send(());
    }
}

fn refresh_snapshot(backend: &dyn BrightnessBackend, snapshot: &Arc<Mutex<AppSnapshot>>) {
    #[cfg(target_os = "windows")]
    let generation = match snapshot.lock() {
        Ok(snapshot) => snapshot.monitor_generation,
        Err(_) => return,
    };
    let result = backend.list_displays();
    #[cfg(target_os = "windows")]
    let targets = result
        .as_deref()
        .map(crate::brightness::windows::monitor_targets)
        .unwrap_or_default();
    let Ok(mut snapshot) = snapshot.lock() else {
        return;
    };
    #[cfg(target_os = "windows")]
    if generation == snapshot.monitor_generation && targets != snapshot.monitor_targets {
        snapshot.monitor_targets = targets;
        snapshot.monitor_generation = generation.wrapping_add(1);
    }

    match result {
        Ok(displays) => {
            let previous_displays = std::mem::take(&mut snapshot.displays)
                .into_iter()
                .map(|display| (display.info.id.clone(), display))
                .collect::<HashMap<_, _>>();

            snapshot.displays = displays
                .into_iter()
                // Read the latest lid state after discovery: a close notification
                // may arrive while the worker is blocked in WMI or DDC/CI.
                .filter(|info| !snapshot.lid_closed || info.kind != DisplayKind::InternalBacklight)
                .map(|mut info| {
                    let previous = previous_displays.get(&info.id);
                    let pending = previous.is_some_and(|display| display.pending);
                    if pending {
                        info.current = previous.and_then(|display| display.info.current);
                    }
                    DisplayState {
                        info,
                        pending,
                        error: previous.and_then(|display| display.error.clone()),
                        latest_request_id: previous.map_or(0, |display| display.latest_request_id),
                    }
                })
                .collect();
            snapshot.last_error = None;
        }
        Err(error) => {
            snapshot.last_error = Some(error.to_string());
            for display in &mut snapshot.displays {
                display.pending = false;
            }
        }
    }
}

fn save_profile_snapshot(
    snapshot: &Arc<Mutex<AppSnapshot>>,
    slot: ProfileSlot,
    displays: &[DisplayInfo],
    path: Option<&std::path::Path>,
) {
    let Ok(mut profiles) = snapshot.lock().map(|snapshot| snapshot.profiles.clone()) else {
        return;
    };
    let count = profiles.capture(slot, displays);
    let result = if count == 0 {
        Err("No readable displays with a stable identity to save.".to_string())
    } else if let Some(path) = path {
        profiles.save(path)
    } else {
        Ok(())
    };
    if let Ok(mut snapshot) = snapshot.lock() {
        match result {
            Ok(()) => {
                snapshot.profiles = profiles;
                snapshot.profile_message =
                    Some(format!("Saved {} for {count} display(s).", slot.label()));
                snapshot.profile_error = None;
            }
            Err(error) => {
                snapshot.profile_message = None;
                snapshot.profile_error = Some(error);
            }
        }
    }
}

fn set_display_brightness(
    backend: &dyn BrightnessBackend,
    snapshot: &Arc<Mutex<AppSnapshot>>,
    display_id: &DisplayId,
    value: u32,
    request_id: u64,
) {
    // Dragging can enqueue values faster than the hardware can apply them.
    // Only the latest request for each display needs a hardware write.
    if !begin_brightness_update(snapshot, display_id, request_id) {
        return;
    }
    let result = backend
        .set_brightness(display_id, value)
        .and_then(|_| backend.get_brightness(display_id));
    apply_brightness_result(snapshot, display_id, result, request_id);
}

fn adjust_display_brightness(
    backend: &dyn BrightnessBackend,
    snapshot: &Arc<Mutex<AppSnapshot>>,
    display_id: &DisplayId,
    delta: i32,
    step: u32,
    request_id: u64,
) {
    if !begin_brightness_update(snapshot, display_id, request_id) {
        return;
    }
    let current = snapshot
        .lock()
        .ok()
        .and_then(|snapshot| {
            snapshot
                .displays
                .iter()
                .find(|display| &display.info.id == display_id)
                .and_then(|display| display.info.current)
        })
        .map(Ok)
        .unwrap_or_else(|| {
            backend
                .get_brightness(display_id)
                .map(BrightnessValue::percent)
        });

    match current {
        Ok(current) => {
            let value = apply_step(current, delta, step);
            update_pending_brightness(snapshot, display_id, value, request_id);
            set_display_brightness(backend, snapshot, display_id, value, request_id);
        }
        Err(error) => apply_brightness_result(snapshot, display_id, Err(error), request_id),
    }
}

fn begin_brightness_update(
    snapshot: &Arc<Mutex<AppSnapshot>>,
    display_id: &DisplayId,
    request_id: u64,
) -> bool {
    let Ok(mut snapshot) = snapshot.lock() else {
        return false;
    };

    let Some(display) = snapshot
        .displays
        .iter_mut()
        .find(|display| &display.info.id == display_id)
    else {
        // A queued slider/menu action must not write to a hidden or removed screen.
        return false;
    };
    if request_id < display.latest_request_id {
        return false;
    }
    display.latest_request_id = request_id;
    display.pending = true;
    display.error = None;
    true
}

fn update_pending_brightness(
    snapshot: &Arc<Mutex<AppSnapshot>>,
    display_id: &DisplayId,
    value: u32,
    request_id: u64,
) {
    let Ok(mut snapshot) = snapshot.lock() else {
        return;
    };

    if let Some(display) = snapshot
        .displays
        .iter_mut()
        .find(|display| &display.info.id == display_id)
    {
        if request_id < display.latest_request_id {
            return;
        }
        display.info.current = Some(clamp_percent(value));
        display.pending = true;
        display.error = None;
        display.latest_request_id = request_id;
    }
}

fn apply_brightness_result(
    snapshot: &Arc<Mutex<AppSnapshot>>,
    display_id: &DisplayId,
    result: Result<BrightnessValue>,
    request_id: u64,
) {
    let Ok(mut snapshot) = snapshot.lock() else {
        return;
    };

    match result {
        Ok(value) => {
            if let Some(display) = snapshot
                .displays
                .iter_mut()
                .find(|display| &display.info.id == display_id)
            {
                if request_id < display.latest_request_id {
                    return;
                }
                display.info.current = Some(value.percent());
                display.pending = false;
                display.error = None;
            }
        }
        Err(error) => {
            if let Some(display) = snapshot
                .displays
                .iter_mut()
                .find(|display| &display.info.id == display_id)
            {
                if request_id < display.latest_request_id {
                    return;
                }
                display.pending = false;
                display.error = Some(error.to_string());
            } else {
                snapshot.last_error = Some(error.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brightness::{BrightnessError, UnsupportedBackend};
    use std::time::Duration;

    #[test]
    fn state_notifications_stay_bounded_without_a_window() {
        let (tx, rx) = mpsc::sync_channel(1);
        for _ in 0..10_000 {
            notify_state_changed(&tx);
        }
        assert_eq!(rx.try_iter().count(), 1);
        notify_state_changed(&tx);
        assert!(rx.try_recv().is_ok());
    }

    fn session_controller(start_hidden: bool, tray_available: bool) -> AppController {
        let controller = AppController::with_backend(
            AppConfig {
                start_hidden,
                ..Default::default()
            },
            Arc::new(UnsupportedBackend::new("test backend")),
        );
        controller.set_tray_available(tray_available);
        controller
    }

    #[test]
    fn tray_sessions_create_ui_only_on_open_and_release_context_on_close() {
        let controller = session_controller(true, true);
        controller.request_hide_window();
        controller.request_open_window();
        let mut sessions = 0;
        run_window_sessions(&controller, || {
            assert!(controller.inner.repaint_ctx.lock().unwrap().is_none());
            sessions += 1;
            controller.set_repaint_context(egui::Context::default());
            if sessions == 1 {
                controller.request_open_window();
            } else {
                controller.request_quit();
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(sessions, 2);
        assert!(controller.inner.repaint_ctx.lock().unwrap().is_none());
        controller.shutdown();
    }

    #[test]
    fn hidden_start_without_tray_still_opens_a_window() {
        let controller = session_controller(true, false);
        let mut sessions = 0;
        run_window_sessions(&controller, || {
            sessions += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(sessions, 1);
        controller.shutdown();
    }

    #[test]
    fn tray_can_quit_without_creating_ui() {
        let controller = session_controller(true, true);
        controller.request_quit();
        run_window_sessions(&controller, || panic!("hidden start must not create UI")).unwrap();
        controller.shutdown();
    }

    #[test]
    fn quitting_in_window_does_not_wait_for_another_tray_command() {
        let controller = session_controller(false, true);
        run_window_sessions(&controller, || {
            controller.shutdown();
            Ok(())
        })
        .unwrap();
        assert!(controller.is_quitting());
    }

    #[test]
    fn failed_window_creation_releases_context() {
        let controller = session_controller(false, true);
        let result = run_window_sessions(&controller, || {
            controller.set_repaint_context(egui::Context::default());
            Err(eframe::Error::AppCreation("test failure".into()))
        });
        assert!(result.is_err());
        assert!(controller.inner.repaint_ctx.lock().unwrap().is_none());
        controller.shutdown();
    }

    #[test]
    fn backend_error_updates_state_without_panic() {
        let snapshot = Arc::new(Mutex::new(AppSnapshot::default()));
        let backend = UnsupportedBackend::new("test backend unavailable");

        refresh_snapshot(&backend, &snapshot);

        let snapshot = snapshot.lock().map(|snapshot| snapshot.clone());
        assert!(snapshot.is_ok());
        let snapshot = snapshot.map_err(|_| BrightnessError::Other("poisoned".into()));
        assert!(snapshot.is_ok());
        let snapshot = snapshot.unwrap_or_default();
        assert!(snapshot.last_error.is_some());
        assert!(snapshot.displays.is_empty());
    }

    #[test]
    fn controller_can_request_tray_update_event() {
        let controller = AppController::with_backend(
            AppConfig::default(),
            Arc::new(UnsupportedBackend::new("test backend unavailable")),
        );
        let (update_tx, update_rx) = mpsc::sync_channel(1);

        controller.set_tray_update_sender(Some(update_tx));
        controller.request_tray_update();

        assert!(update_rx.recv_timeout(Duration::from_secs(1)).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_hide_backend_is_used_only_when_tray_and_display_are_available() {
        assert!(should_prefer_x11_for_hide_to_tray_with_display(
            true,
            Some(":0")
        ));
        assert!(!should_prefer_x11_for_hide_to_tray_with_display(
            false,
            Some(":0")
        ));
        assert!(!should_prefer_x11_for_hide_to_tray_with_display(true, None));
        assert!(!should_prefer_x11_for_hide_to_tray_with_display(
            true,
            Some("")
        ));
    }

    #[cfg(feature = "mock-backend")]
    mod live_updates {
        use super::*;
        use crate::brightness::mock::MockBrightnessBackend;
        use std::time::Instant;

        struct BlockingBackend {
            inner: MockBrightnessBackend,
            writes: Sender<(DisplayId, u32)>,
            resume: Mutex<Receiver<()>>,
            block_next_write: AtomicBool,
        }

        impl BrightnessBackend for BlockingBackend {
            fn list_displays(&self) -> Result<Vec<DisplayInfo>> {
                self.inner.list_displays()
            }

            fn get_brightness(&self, display_id: &DisplayId) -> Result<BrightnessValue> {
                self.inner.get_brightness(display_id)
            }

            fn set_brightness(&self, display_id: &DisplayId, value: u32) -> Result<()> {
                self.writes.send((display_id.clone(), value)).unwrap();
                if self.block_next_write.swap(false, Ordering::Relaxed) {
                    self.resume
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(3))
                        .expect("test did not release the hardware write");
                }
                self.inner.set_brightness(display_id, value)
            }
        }

        fn wait_until_idle(controller: &AppController) {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let snapshot = controller.snapshot();
                if !snapshot.displays.is_empty()
                    && snapshot.displays.iter().all(|display| !display.pending)
                {
                    return;
                }
                controller
                    .inner
                    .state_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("brightness worker did not finish");
            }
        }

        #[test]
        #[cfg(target_os = "windows")]
        fn wheel_adjusts_only_the_target_and_rejects_a_reused_monitor_handle() {
            let backend = Arc::new(MockBrightnessBackend::default());
            let controller = AppController::with_backend(AppConfig::default(), backend.clone());
            wait_until_idle(&controller);

            let internal = DisplayId::new("mock:internal");
            let external = DisplayId::new("mock:external");
            controller.inner.snapshot.lock().unwrap().monitor_targets =
                vec![(1, internal.clone()), (2, external.clone())];
            let generation = controller.wheel_target_generation(2).unwrap();
            assert_eq!(controller.adjust_wheel_target(2, generation, 2), Some(80));
            wait_until_idle(&controller);
            assert_eq!(backend.get_brightness(&internal).unwrap().percent(), 60);
            assert_eq!(backend.get_brightness(&external).unwrap().percent(), 80);

            controller.invalidate_monitor_targets();
            controller.inner.snapshot.lock().unwrap().monitor_targets = vec![(2, internal.clone())];
            assert_ne!(controller.wheel_target_generation(2), Some(generation));
            assert_eq!(controller.adjust_wheel_target(2, generation, -1), None);
            assert!(!controller
                .snapshot()
                .displays
                .iter()
                .any(|display| display.pending));
            assert_eq!(backend.get_brightness(&internal).unwrap().percent(), 60);
            controller.shutdown();
        }

        #[test]
        fn closing_lid_during_discovery_hides_internal_and_opening_restores_it() {
            struct GatedDiscovery {
                inner: MockBrightnessBackend,
                started: Sender<()>,
                resume: Mutex<Receiver<()>>,
                block: AtomicBool,
            }
            impl BrightnessBackend for GatedDiscovery {
                fn list_displays(&self) -> Result<Vec<DisplayInfo>> {
                    let displays = self.inner.list_displays()?;
                    if self.block.swap(false, Ordering::SeqCst) {
                        self.started.send(()).unwrap();
                        self.resume
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(3))
                            .unwrap();
                    }
                    Ok(displays)
                }
                fn get_brightness(&self, id: &DisplayId) -> Result<BrightnessValue> {
                    self.inner.get_brightness(id)
                }
                fn set_brightness(&self, id: &DisplayId, value: u32) -> Result<()> {
                    self.inner.set_brightness(id, value)
                }
            }

            let (started_tx, started_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let backend = Arc::new(GatedDiscovery {
                inner: MockBrightnessBackend::default(),
                started: started_tx,
                resume: Mutex::new(resume_rx),
                block: AtomicBool::new(true),
            });
            let controller = AppController::with_backend(AppConfig::default(), backend.clone());
            started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            controller.set_lid_closed(true);
            resume_tx.send(()).unwrap();
            wait_until_idle(&controller);
            assert_eq!(controller.snapshot().displays.len(), 1);
            assert_eq!(
                controller.snapshot().displays[0].info.kind,
                DisplayKind::ExternalDdcCi
            );

            // Refreshing while closed cannot reintroduce the WMI panel.
            refresh_snapshot(backend.as_ref(), &controller.inner.snapshot);
            assert_eq!(controller.snapshot().displays.len(), 1);

            controller.set_lid_closed(false);
            refresh_snapshot(backend.as_ref(), &controller.inner.snapshot);
            assert_eq!(controller.snapshot().displays.len(), 2);

            // Close removes an already visible panel immediately. An action
            // queued before the close must not change its hardware brightness.
            let internal = DisplayId::new("mock:internal");
            controller.set_lid_closed(true);
            assert_eq!(controller.snapshot().displays.len(), 1);
            set_display_brightness(
                backend.as_ref(),
                &controller.inner.snapshot,
                &internal,
                20,
                1,
            );
            assert_eq!(backend.get_brightness(&internal).unwrap().percent(), 60);
            controller.shutdown();
        }

        #[test]
        fn saved_profiles_reload_and_apply_each_display_without_touching_hidden_panels() {
            fn wait_for(mut ready: impl FnMut() -> bool) {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !ready() {
                    assert!(Instant::now() < deadline, "profile update timed out");
                    thread::sleep(Duration::from_millis(1));
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("profiles.toml");
            let backend = Arc::new(MockBrightnessBackend::default());
            let internal = DisplayId::new("mock:internal");
            let external = DisplayId::new("mock:external");
            backend.set_brightness(&internal, 15).unwrap();
            backend.set_brightness(&external, 25).unwrap();
            let controller = AppController::with_backend_and_profiles_path(
                AppConfig::default(),
                backend.clone(),
                Some(path.clone()),
            );
            wait_until_idle(&controller);
            controller.save_profile(ProfileSlot::Dark);
            wait_for(|| controller.can_apply_profile(ProfileSlot::Dark));
            controller.set_brightness(internal.clone(), 85);
            controller.set_brightness(external.clone(), 95);
            wait_until_idle(&controller);
            controller.save_profile(ProfileSlot::Bright);
            wait_for(|| controller.can_apply_profile(ProfileSlot::Bright));
            controller.shutdown();

            let restored = AppController::with_backend_and_profiles_path(
                AppConfig::default(),
                backend.clone(),
                Some(path.clone()),
            );
            wait_until_idle(&restored);
            restored.apply_profile(ProfileSlot::Dark);
            wait_until_idle(&restored);
            assert_eq!(backend.get_brightness(&internal).unwrap().percent(), 15);
            assert_eq!(backend.get_brightness(&external).unwrap().percent(), 25);
            restored.apply_profile(ProfileSlot::Bright);
            wait_until_idle(&restored);
            assert_eq!(backend.get_brightness(&internal).unwrap().percent(), 85);
            assert_eq!(backend.get_brightness(&external).unwrap().percent(), 95);

            restored.set_lid_closed(true);
            restored.apply_profile(ProfileSlot::Dark);
            wait_until_idle(&restored);
            assert_eq!(backend.get_brightness(&internal).unwrap().percent(), 85);
            assert_eq!(backend.get_brightness(&external).unwrap().percent(), 25);
            restored.set_brightness(external.clone(), 35);
            wait_until_idle(&restored);
            restored.save_profile(ProfileSlot::Dark);
            wait_for(|| restored.snapshot().profiles.dark[external.as_str()].brightness == 35);
            let saved = BrightnessProfiles::load(&path).unwrap();
            assert_eq!(saved.dark[internal.as_str()].brightness, 15);
            assert_eq!(saved.dark[external.as_str()].brightness, 35);
            restored.shutdown();
        }

        #[test]
        fn failed_profile_save_preserves_existing_profile_and_reports_error() {
            let backend = MockBrightnessBackend::default();
            let displays = backend.list_displays().unwrap();
            let mut initial = AppSnapshot::default();
            initial.profiles.capture(ProfileSlot::Dark, &displays);
            let original = initial.profiles.clone();
            let snapshot = Arc::new(Mutex::new(initial));
            let mut changed = displays;
            changed[0].current = Some(10);
            let dir = tempfile::tempdir().unwrap();
            save_profile_snapshot(&snapshot, ProfileSlot::Dark, &changed, Some(dir.path()));
            let snapshot = snapshot.lock().unwrap();
            assert!(snapshot.profile_error.is_some());
            assert!(snapshot.profile_message.is_none());
            assert_eq!(snapshot.profiles, original);
        }

        #[test]
        fn live_updates_write_immediately_and_skip_superseded_values_per_display() {
            let (writes_tx, writes_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let backend = Arc::new(BlockingBackend {
                inner: MockBrightnessBackend::default(),
                writes: writes_tx,
                resume: Mutex::new(resume_rx),
                block_next_write: AtomicBool::new(true),
            });
            let controller = AppController::with_backend(AppConfig::default(), backend.clone());
            wait_until_idle(&controller);
            let internal = DisplayId::new("mock:internal");
            let external = DisplayId::new("mock:external");

            // The first value reaches the hardware before the interaction ends.
            controller.set_brightness(internal.clone(), 20);
            assert_eq!(
                writes_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
                (internal.clone(), 20)
            );
            controller.set_brightness(internal.clone(), 30);
            controller.set_brightness(internal.clone(), 40);
            controller.set_brightness(external.clone(), 55);
            controller.set_brightness(internal.clone(), 80);

            resume_tx.send(()).unwrap();
            assert_eq!(
                writes_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
                (external.clone(), 55)
            );
            assert_eq!(
                writes_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
                (internal.clone(), 80)
            );
            wait_until_idle(&controller);
            assert!(writes_rx.try_recv().is_err());
            assert_eq!(backend.get_brightness(&internal).unwrap().percent(), 80);
            assert_eq!(backend.get_brightness(&external).unwrap().percent(), 55);
            controller.shutdown();
        }

        #[test]
        fn completed_update_rejects_late_older_commits() {
            let backend = MockBrightnessBackend::default();
            let snapshot = Arc::new(Mutex::new(AppSnapshot::default()));
            refresh_snapshot(&backend, &snapshot);
            let display_id = DisplayId::new("mock:internal");
            update_pending_brightness(&snapshot, &display_id, 80, 2);
            set_display_brightness(&backend, &snapshot, &display_id, 80, 2);

            set_display_brightness(&backend, &snapshot, &display_id, 20, 1);
            update_pending_brightness(&snapshot, &display_id, 20, 1);
            assert_eq!(backend.get_brightness(&display_id).unwrap().percent(), 80);
            let snapshot = snapshot.lock().unwrap();
            let display = &snapshot.displays[0];
            assert_eq!(display.info.current, Some(80));
            assert!(!display.pending);
            assert_eq!(display.latest_request_id, 2);
        }

        #[test]
        fn adjustment_without_cached_value_uses_a_new_request_id() {
            let backend = Arc::new(MockBrightnessBackend::default());
            let controller = AppController::with_backend(AppConfig::default(), backend.clone());
            wait_until_idle(&controller);
            let display_id = DisplayId::new("mock:internal");

            for expected in [65, 70] {
                controller.inner.snapshot.lock().unwrap().displays[0]
                    .info
                    .current = None;
                controller.adjust_brightness(display_id.clone(), 1);
                wait_until_idle(&controller);
                assert_eq!(
                    backend.get_brightness(&display_id).unwrap().percent(),
                    expected
                );
                assert_eq!(
                    controller.snapshot().displays[0].info.current,
                    Some(expected)
                );
            }
            controller.shutdown();
        }

        #[test]
        fn stale_readbacks_and_errors_do_not_replace_latest_preview() {
            let backend = MockBrightnessBackend::default();
            let snapshot = Arc::new(Mutex::new(AppSnapshot::default()));
            refresh_snapshot(&backend, &snapshot);
            let display_id = DisplayId::new("mock:internal");
            update_pending_brightness(&snapshot, &display_id, 20, 1);
            update_pending_brightness(&snapshot, &display_id, 80, 2);
            apply_brightness_result(
                &snapshot,
                &display_id,
                Ok(BrightnessValue {
                    current: 20,
                    max: 100,
                }),
                1,
            );
            apply_brightness_result(&snapshot, &display_id, Err(BrightnessError::Unsupported), 1);
            {
                let snapshot = snapshot.lock().unwrap();
                let display = &snapshot.displays[0];
                assert_eq!(display.info.current, Some(80));
                assert!(display.pending);
                assert!(display.error.is_none());
            }

            apply_brightness_result(&snapshot, &display_id, Err(BrightnessError::Unsupported), 2);
            let snapshot = snapshot.lock().unwrap();
            let display = &snapshot.displays[0];
            assert!(!display.pending);
            assert!(display.error.is_some());
            assert_eq!(display.info.current, Some(80));
        }

        #[test]
        fn refresh_preserves_pending_value_and_latest_request_id() {
            let backend = MockBrightnessBackend::default();
            let snapshot = Arc::new(Mutex::new(AppSnapshot::default()));
            refresh_snapshot(&backend, &snapshot);
            let display_id = DisplayId::new("mock:internal");
            update_pending_brightness(&snapshot, &display_id, 80, 2);

            refresh_snapshot(&backend, &snapshot);
            {
                let snapshot = snapshot.lock().unwrap();
                let display = &snapshot.displays[0];
                assert_eq!(display.info.current, Some(80));
                assert!(display.pending);
                assert_eq!(display.latest_request_id, 2);
            }

            set_display_brightness(&backend, &snapshot, &display_id, 80, 2);
            refresh_snapshot(&backend, &snapshot);
            let snapshot = snapshot.lock().unwrap();
            let display = &snapshot.displays[0];
            assert_eq!(display.info.current, Some(80));
            assert!(!display.pending);
            assert_eq!(display.latest_request_id, 2);
        }
    }
}
