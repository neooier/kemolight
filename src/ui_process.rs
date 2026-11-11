//! Windows keeps hardware control in a small tray process. The same executable
//! runs the disposable UI with inherited anonymous pipes, without opening a
//! network listener or starting another hardware worker.

use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    os::windows::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    app::{run_main_window, AppCommand, AppController, AppSnapshot, WindowGeometry},
    brightness::DisplayId,
    config::AppConfig,
    profiles::ProfileSlot,
};

const CHILD_ARGUMENT: &str = "--ui-child";
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Serialize, Deserialize)]
struct InitialState {
    config: AppConfig,
    snapshot: AppSnapshot,
    geometry: WindowGeometry,
    tray_available: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct UiState {
    snapshot: AppSnapshot,
    acknowledged: u64,
}

#[derive(Debug, Serialize, Deserialize)]
enum ParentMessage {
    Initialize(Box<InitialState>),
    State(Box<UiState>),
    Open,
    Hide,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum UiCommand {
    Refresh,
    Set { display_id: DisplayId, value: u32 },
    Adjust { display_id: DisplayId, delta: i32 },
    SaveProfile(ProfileSlot),
    ApplyProfile(ProfileSlot),
    Closing(WindowGeometry),
    Shown,
    Quit,
}

#[derive(Debug, Serialize, Deserialize)]
struct UiRequest {
    sequence: u64,
    command: UiCommand,
}

fn invalid_message(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_message<T: DeserializeOwned>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut bytes = Vec::new();
    let read = reader
        .take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if read > MAX_MESSAGE_BYTES || bytes.last() != Some(&b'\n') {
        return Err(invalid_message("incomplete or oversized UI message"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

fn write_message(writer: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() >= MAX_MESSAGE_BYTES {
        return Err(invalid_message("oversized UI message"));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

struct PendingPreview {
    sequence: u64,
    value: u32,
}

struct ClientState {
    sequence: u64,
    acknowledged: u64,
    pending: HashMap<DisplayId, PendingPreview>,
}

impl ClientState {
    fn request(&mut self, command: UiCommand) -> UiRequest {
        self.sequence += 1;
        UiRequest {
            sequence: self.sequence,
            command,
        }
    }
}

struct ClientConnection {
    state: Mutex<ClientState>,
    writer: Mutex<Box<dyn Write + Send>>,
}

#[derive(Clone)]
pub(crate) struct RemoteClient(Arc<ClientConnection>);

impl RemoteClient {
    fn new(writer: impl Write + Send + 'static) -> Self {
        Self(Arc::new(ClientConnection {
            writer: Mutex::new(Box::new(writer)),
            state: Mutex::new(ClientState {
                sequence: 0,
                acknowledged: 0,
                pending: HashMap::new(),
            }),
        }))
    }

    pub(crate) fn send(&self, controller: &AppController, command: UiCommand) {
        let sent = self.0.writer.lock().is_ok_and(|mut writer| {
            let Ok(mut state) = self.0.state.lock() else {
                return false;
            };
            let request = state.request(command);
            drop(state);
            write_message(&mut *writer, &request).is_ok()
        });
        if !sent {
            controller.request_quit();
        }
    }

    pub(crate) fn set_brightness(
        &self,
        controller: &AppController,
        display_id: DisplayId,
        value: u32,
    ) {
        let Ok(mut writer) = self.0.writer.lock() else {
            controller.request_quit();
            return;
        };
        let Ok(mut state) = self.0.state.lock() else {
            controller.request_quit();
            return;
        };
        // Serialize previews and incoming snapshots under the same lock. An
        // older acknowledgment must never move a slider behind the pointer.
        controller.preview_brightness(display_id.clone(), value);
        let sequence = state.sequence + 1;
        state
            .pending
            .insert(display_id.clone(), PendingPreview { sequence, value });
        let request = state.request(UiCommand::Set { display_id, value });
        // Never hold the state lock during pipe I/O: incoming snapshots must
        // keep draining even when the outgoing pipe is temporarily full.
        drop(state);
        if write_message(&mut *writer, &request).is_err() {
            controller.request_quit();
        }
    }

    fn receive(&self, controller: &AppController, mut update: UiState) {
        let Ok(mut state) = self.0.state.lock() else {
            return;
        };
        if update.acknowledged < state.acknowledged {
            return;
        }
        state.acknowledged = update.acknowledged;
        state
            .pending
            .retain(|_, preview| preview.sequence > update.acknowledged);
        for display in &mut update.snapshot.displays {
            if let Some(preview) = state.pending.get(&display.info.id) {
                display.info.current = Some(preview.value);
                display.pending = true;
                display.error = None;
            }
        }
        controller.replace_snapshot(update.snapshot);
    }
}

struct ParentConnection {
    id: u64,
    writer: Box<dyn Write + Send>,
    acknowledged: u64,
    closing: bool,
    reopen_requested: bool,
}

impl ParentConnection {
    fn send_state(&mut self, controller: &AppController) -> io::Result<()> {
        write_message(
            &mut self.writer,
            &ParentMessage::State(Box::new(UiState {
                snapshot: controller.snapshot(),
                acknowledged: self.acknowledged,
            })),
        )
    }

    fn apply(&mut self, controller: &AppController, request: UiRequest) -> io::Result<()> {
        if request.sequence <= self.acknowledged {
            return Ok(());
        }
        match request.command {
            UiCommand::Refresh => controller.refresh(),
            UiCommand::Set { display_id, value } => controller.set_brightness(display_id, value),
            UiCommand::Adjust { display_id, delta } => {
                controller.adjust_brightness(display_id, delta)
            }
            UiCommand::SaveProfile(slot) => controller.save_profile(slot),
            UiCommand::ApplyProfile(slot) => controller.apply_profile(slot),
            UiCommand::Closing(geometry) => {
                controller.set_window_geometry(geometry);
                self.closing = true;
            }
            UiCommand::Shown => {
                self.closing = false;
                self.reopen_requested = false;
            }
            UiCommand::Quit => controller.request_quit(),
        }
        // The connection lock covers command application, acknowledgment, and
        // snapshot capture, including state updates from the hardware worker.
        self.acknowledged = request.sequence;
        self.send_state(controller)
    }
}

type SharedConnection = Arc<Mutex<Option<ParentConnection>>>;

struct UiChild {
    id: u64,
    process: Child,
    reader: Option<JoinHandle<()>>,
    connection: SharedConnection,
}

impl UiChild {
    fn start(
        controller: &AppController,
        connection: &SharedConnection,
        id: u64,
    ) -> io::Result<Self> {
        let process = Command::new(std::env::current_exe()?)
            .arg(CHILD_ARGUMENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let mut child = Self {
            id,
            process,
            reader: None,
            connection: Arc::clone(connection),
        };
        let mut writer = child
            .process
            .stdin
            .take()
            .ok_or_else(|| invalid_message("missing UI input pipe"))?;
        let output = child
            .process
            .stdout
            .take()
            .ok_or_else(|| invalid_message("missing UI output pipe"))?;
        write_message(
            &mut writer,
            &ParentMessage::Initialize(Box::new(InitialState {
                config: controller.config().clone(),
                snapshot: controller.snapshot(),
                geometry: controller.window_geometry(),
                tray_available: controller.tray_available(),
            })),
        )?;
        *connection
            .lock()
            .map_err(|_| io::Error::other("UI connection lock poisoned"))? =
            Some(ParentConnection {
                id,
                writer: Box::new(writer),
                acknowledged: 0,
                closing: false,
                reopen_requested: false,
            });

        // Discovery may complete between capturing Initialize and installing
        // the connection, after its wakeup was consumed without a UI attached.
        if let Some(current) = connection
            .lock()
            .map_err(|_| io::Error::other("UI connection lock poisoned"))?
            .as_mut()
        {
            current.send_state(controller)?;
        }

        let reader_controller = controller.clone();
        let reader_connection = Arc::clone(connection);
        child.reader = Some(thread::Builder::new().name("ui-commands".into()).spawn(
            move || {
                let mut output = BufReader::new(output);
                loop {
                    match read_message::<UiRequest>(&mut output) {
                        Ok(Some(request)) => {
                            let Ok(mut connection) = reader_connection.lock() else {
                                break;
                            };
                            let Some(current) =
                                connection.as_mut().filter(|current| current.id == id)
                            else {
                                break;
                            };
                            if current.apply(&reader_controller, request).is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            eprintln!("UI communication failed: {error}");
                            break;
                        }
                    }
                }
                reader_controller.ui_closed(id);
            },
        )?);
        Ok(child)
    }
}

impl Drop for UiChild {
    fn drop(&mut self) {
        if let Ok(mut connection) = self.connection.lock() {
            if connection
                .as_ref()
                .is_some_and(|current| current.id == self.id)
            {
                *connection = None;
            }
        }
        // Also handles startup failures, UI crashes, and quitting from the tray.
        // The hardware worker never lives in this disposable process.
        if !matches!(self.process.try_wait(), Ok(Some(_))) {
            let _ = self.process.kill();
        }
        let _ = self.process.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn session_loop(controller: &AppController, connection: &SharedConnection) -> io::Result<()> {
    let mut next_id = 1;
    let mut child = if !controller.config().start_hidden || !controller.tray_available() {
        Some(UiChild::start(controller, connection, next_id)?)
    } else {
        None
    };
    loop {
        match controller.wait_for_app_command() {
            Some(AppCommand::OpenWindow) => {
                if child.is_some() {
                    let mut current = connection
                        .lock()
                        .map_err(|_| io::Error::other("UI connection lock poisoned"))?;
                    if let Some(current) = current.as_mut() {
                        if current.closing {
                            current.reopen_requested = true;
                            continue;
                        }
                        if write_message(&mut current.writer, &ParentMessage::Open).is_ok() {
                            continue;
                        }
                    }
                }
                drop(child.take());
                next_id += 1;
                child = Some(UiChild::start(controller, connection, next_id)?);
            }
            Some(AppCommand::HideWindow) => {
                if let Ok(mut current) = connection.lock() {
                    if let Some(current) = current.as_mut() {
                        let _ = write_message(&mut current.writer, &ParentMessage::Hide);
                    }
                }
            }
            Some(AppCommand::UiClosed(id)) => {
                if child.as_ref().is_some_and(|child| child.id == id) {
                    let reopen = connection.lock().ok().is_some_and(|current| {
                        current
                            .as_ref()
                            .is_some_and(|current| current.id == id && current.reopen_requested)
                    });
                    drop(child.take());
                    if !controller.tray_available() {
                        return Ok(());
                    }
                    if reopen {
                        next_id += 1;
                        child = Some(UiChild::start(controller, connection, next_id)?);
                    }
                }
            }
            Some(AppCommand::Quit) | None => return Ok(()),
        }
    }
}

pub(crate) fn run_sessions(controller: &AppController) -> io::Result<()> {
    let connection: SharedConnection = Arc::new(Mutex::new(None));
    let updates_controller = controller.clone();
    let updates_connection = Arc::clone(&connection);
    let updates = thread::Builder::new()
        .name("ui-state".into())
        .spawn(move || {
            while updates_controller.wait_for_state_event() {
                if updates_controller.is_quitting() {
                    break;
                }
                if let Ok(mut current) = updates_connection.lock() {
                    if let Some(current) = current.as_mut().filter(|current| !current.closing) {
                        if current.send_state(&updates_controller).is_err() {
                            updates_controller.ui_closed(current.id);
                        }
                    }
                }
            }
        })?;
    let result = session_loop(controller, &connection);
    controller.shutdown();
    let _ = updates.join();
    result
}

fn run_child() -> io::Result<()> {
    let mut input = BufReader::new(io::stdin());
    let Some(ParentMessage::Initialize(initial)) = read_message(&mut input)? else {
        return Err(invalid_message("missing UI initialization"));
    };
    let remote = RemoteClient::new(io::stdout());
    let controller = AppController::with_remote(
        initial.config,
        initial.snapshot,
        initial.geometry,
        initial.tray_available,
        remote.clone(),
    );
    let reader_controller = controller.clone();
    thread::Builder::new()
        .name("ui-state".into())
        .spawn(move || {
            loop {
                match read_message::<ParentMessage>(&mut input) {
                    Ok(Some(ParentMessage::State(update))) => {
                        remote.receive(&reader_controller, *update)
                    }
                    Ok(Some(ParentMessage::Open)) => reader_controller.request_open_window(),
                    Ok(Some(ParentMessage::Hide)) => reader_controller.request_hide_window(),
                    _ => break,
                }
            }
            // A closed input pipe means the owner exited; never leave an orphan UI.
            reader_controller.request_quit();
        })?;
    let result = run_main_window(controller.clone(), false);
    controller.clear_repaint_context();
    result.map_err(|error| io::Error::other(error.to_string()))
}

/// Called before acquiring the single-instance mutex. UI children never create
/// a tray or hardware backend.
pub fn run_child_if_requested() -> bool {
    if crate::brightness_overlay::run_child_if_requested() {
        return true;
    }
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(CHILD_ARGUMENT)) {
        return false;
    }
    if let Err(error) = run_child() {
        eprintln!("Failed to run UI process: {error}");
        std::process::exit(1);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brightness::UnsupportedBackend;
    use std::io::Cursor;
    #[cfg(feature = "mock-backend")]
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    #[cfg(feature = "mock-backend")]
    #[derive(Clone, Default)]
    struct SharedBytes(Arc<Mutex<Vec<u8>>>);

    #[cfg(feature = "mock-backend")]
    impl Write for SharedBytes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn connection(writer: impl Write + Send + 'static) -> ParentConnection {
        ParentConnection {
            id: 1,
            writer: Box::new(writer),
            acknowledged: 0,
            closing: false,
            reopen_requested: false,
        }
    }

    #[test]
    fn protocol_preserves_message_boundaries_and_unicode() {
        let snapshot = AppSnapshot {
            last_error: Some("显示器\n\"not found\"".into()),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        write_message(
            &mut bytes,
            &ParentMessage::State(Box::new(UiState {
                snapshot,
                acknowledged: 7,
            })),
        )
        .unwrap();
        write_message(&mut bytes, &ParentMessage::Hide).unwrap();
        let mut input = Cursor::new(bytes);
        let ParentMessage::State(state) = read_message(&mut input).unwrap().unwrap() else {
            panic!("missing state")
        };
        assert_eq!(state.acknowledged, 7);
        assert_eq!(
            state.snapshot.last_error.as_deref(),
            Some("显示器\n\"not found\"")
        );
        assert!(matches!(
            read_message(&mut input).unwrap(),
            Some(ParentMessage::Hide)
        ));
        assert!(read_message::<ParentMessage>(&mut input).unwrap().is_none());
    }

    #[test]
    fn protocol_rejects_truncated_malformed_and_oversized_messages() {
        for bytes in [
            b"\"Hide\"".to_vec(),
            b"invalid\n".to_vec(),
            vec![b'x'; MAX_MESSAGE_BYTES + 1],
        ] {
            assert!(read_message::<ParentMessage>(&mut Cursor::new(bytes)).is_err());
        }
        let mut output = Vec::new();
        assert!(write_message(&mut output, &"x".repeat(MAX_MESSAGE_BYTES)).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn parent_tracks_window_geometry_and_canceled_close() {
        let controller = AppController::with_backend(
            AppConfig::default(),
            Arc::new(UnsupportedBackend::new("test")),
        );
        let mut connection = connection(io::sink());
        connection
            .apply(
                &controller,
                UiRequest {
                    sequence: 1,
                    command: UiCommand::Closing(WindowGeometry {
                        inner_size: Some([600.0, 400.0]),
                        outer_position: Some([-1200.0, 80.0]),
                    }),
                },
            )
            .unwrap();
        assert!(connection.closing);
        assert_eq!(
            controller.window_geometry().outer_position,
            Some([-1200.0, 80.0])
        );
        connection.reopen_requested = true;
        connection
            .apply(
                &controller,
                UiRequest {
                    sequence: 2,
                    command: UiCommand::Shown,
                },
            )
            .unwrap();
        assert!(!connection.closing);
        assert!(!connection.reopen_requested);
        controller.shutdown();
    }

    #[test]
    fn hidden_parent_can_quit_without_starting_a_graphics_process() {
        let controller = AppController::with_backend(
            AppConfig {
                start_hidden: true,
                ..Default::default()
            },
            Arc::new(UnsupportedBackend::new("test")),
        );
        controller.set_tray_available(true);
        controller.request_quit();
        run_sessions(&controller).unwrap();
        assert!(controller.is_quitting());
    }

    #[cfg(feature = "mock-backend")]
    fn mock_controller() -> (
        AppController,
        Arc<crate::brightness::mock::MockBrightnessBackend>,
    ) {
        let backend = Arc::new(crate::brightness::mock::MockBrightnessBackend::default());
        let controller = AppController::with_backend(AppConfig::default(), backend.clone());
        let deadline = Instant::now() + Duration::from_secs(3);
        while controller.snapshot().displays.is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        (controller, backend)
    }

    #[cfg(feature = "mock-backend")]
    #[test]
    fn remote_slider_keeps_newest_preview_until_parent_acknowledges_it() {
        use crate::brightness::BrightnessBackend;
        let (parent, backend) = mock_controller();
        let requests = SharedBytes::default();
        let remote = RemoteClient::new(requests.clone());
        let child = AppController::with_remote(
            AppConfig::default(),
            parent.snapshot(),
            WindowGeometry::default(),
            true,
            remote.clone(),
        );
        let id = DisplayId::new("mock:internal");
        child.set_brightness(id.clone(), 10);
        child.set_brightness(id.clone(), 80);
        let mut requests = Cursor::new(requests.0.lock().unwrap().clone());
        let updates = SharedBytes::default();
        let mut connection = connection(updates.clone());
        connection
            .apply(&parent, read_message(&mut requests).unwrap().unwrap())
            .unwrap();
        let ParentMessage::State(update) =
            read_message(&mut Cursor::new(updates.0.lock().unwrap().clone()))
                .unwrap()
                .unwrap()
        else {
            panic!("missing update")
        };
        remote.receive(&child, *update);
        let display = child
            .snapshot()
            .displays
            .into_iter()
            .find(|display| display.info.id == id)
            .unwrap();
        assert_eq!(display.info.current, Some(80));
        assert!(display.pending);

        connection
            .apply(&parent, read_message(&mut requests).unwrap().unwrap())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while parent
            .snapshot()
            .displays
            .iter()
            .any(|display| display.pending)
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        remote.receive(
            &child,
            UiState {
                snapshot: parent.snapshot(),
                acknowledged: 2,
            },
        );
        assert_eq!(backend.get_brightness(&id).unwrap().percent(), 80);
        let display = child
            .snapshot()
            .displays
            .into_iter()
            .find(|display| display.info.id == id)
            .unwrap();
        assert_eq!(display.info.current, Some(80));
        assert!(!display.pending);
        assert!(remote.0.state.lock().unwrap().pending.is_empty());
        // A stale transport update cannot replace an already acknowledged one.
        remote.receive(
            &child,
            UiState {
                snapshot: AppSnapshot::default(),
                acknowledged: 1,
            },
        );
        assert!(!child.snapshot().displays.is_empty());
        parent.shutdown();
    }

    #[cfg(feature = "mock-backend")]
    #[test]
    fn blocked_outgoing_pipe_does_not_block_incoming_snapshots() {
        struct BlockingWriter {
            started: mpsc::Sender<()>,
            resume: mpsc::Receiver<()>,
            blocked: bool,
        }
        impl Write for BlockingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.blocked {
                    self.blocked = true;
                    self.started.send(()).unwrap();
                    self.resume.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (parent, _) = mock_controller();
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let remote = RemoteClient::new(BlockingWriter {
            started: started_tx,
            resume: resume_rx,
            blocked: false,
        });
        let child = AppController::with_remote(
            AppConfig::default(),
            parent.snapshot(),
            WindowGeometry::default(),
            true,
            remote.clone(),
        );
        let sender = child.clone();
        let writer =
            thread::spawn(move || sender.set_brightness(DisplayId::new("mock:internal"), 80));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let receiver = child.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let snapshot = parent.snapshot();
        let reader = thread::spawn(move || {
            remote.receive(
                &receiver,
                UiState {
                    snapshot,
                    acknowledged: 0,
                },
            );
            done_tx.send(()).unwrap();
        });
        let received_while_blocked = done_rx.recv_timeout(Duration::from_secs(1));
        resume_tx.send(()).unwrap();
        writer.join().unwrap();
        reader.join().unwrap();
        assert!(received_while_blocked.is_ok());
        assert_eq!(
            child
                .snapshot()
                .displays
                .iter()
                .find(|display| display.info.id.as_str() == "mock:internal")
                .unwrap()
                .info
                .current,
            Some(80)
        );
        parent.shutdown();
    }
}
