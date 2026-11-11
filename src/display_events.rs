//! Windows broadcasts are received on a hidden top-level window, independently
//! of the tray and UI. Hardware discovery stays on the brightness worker.

use std::{
    cell::{Cell, RefCell},
    mem, ptr,
    sync::mpsc,
    thread,
};

use windows_sys::Win32::{
    Devices::Display::GUID_DEVINTERFACE_MONITOR,
    Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL},
    System::{
        LibraryLoader::GetModuleHandleW,
        Power::{
            RegisterPowerSettingNotification, UnregisterPowerSettingNotification, HPOWERNOTIFY,
            POWERBROADCAST_SETTING,
        },
        SystemServices::{GUID_CONSOLE_DISPLAY_STATE, GUID_LIDSWITCH_STATE_CHANGE},
    },
    UI::{
        HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE},
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
            VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
        },
        WindowsAndMessaging::{
            CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
            GetMessageW, GetWindowLongPtrW, KillTimer, PostMessageW, PostQuitMessage,
            RegisterClassW, RegisterDeviceNotificationW, SetTimer, SetWindowLongPtrW,
            SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, UnregisterDeviceNotification,
            CREATESTRUCTW, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVNODES_CHANGED,
            DBT_DEVTYP_DEVICEINTERFACE, DEVICE_NOTIFY_WINDOW_HANDLE,
            DEV_BROADCAST_DEVICEINTERFACE_W, GWLP_USERDATA, HC_ACTION, HDEVNOTIFY, HHOOK, MSG,
            MSLLHOOKSTRUCT, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_POWERSETTINGCHANGE,
            WHEEL_DELTA, WH_MOUSE_LL, WM_APP, WM_DESTROY, WM_DEVICECHANGE, WM_DISPLAYCHANGE,
            WM_HOTKEY, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_POWERBROADCAST, WM_TIMER,
            WNDCLASSW, WS_OVERLAPPED,
        },
    },
};

use crate::{
    app::AppController,
    brightness_overlay::{Overlay, WM_OVERLAY_READY},
    profiles::ProfileSlot,
};

const WM_STOP: u32 = WM_APP + 1;
const REFRESH_TIMER: usize = 1;
const SETTLED_REFRESH_TIMER: usize = 2;
const WHEEL_TIMER: usize = 3;
const OVERLAY_TIMER: usize = 4;
const REFRESH_DELAY_MS: u32 = 300;
const SETTLED_REFRESH_DELAY_MS: u32 = 2000;
const WHEEL_BATCH_MS: u32 = 60;
const OVERLAY_CHECK_MS: u32 = 75;
const HOTKEY_BRIGHT: i32 = 1;
const HOTKEY_DARK: i32 = 2;

struct Hotkeys {
    hwnd: HWND,
    registered: Vec<i32>,
    mouse_hook: HHOOK,
}

impl Hotkeys {
    fn register(hwnd: HWND, instance: HINSTANCE, controller: &AppController) -> Self {
        let mouse_hook =
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), instance, 0) };
        let mut hotkeys = Self {
            hwnd,
            registered: Vec::new(),
            mouse_hook,
        };
        let mut errors = Vec::new();
        if mouse_hook.is_null() {
            errors.push(format!(
                "Shift+Win+Wheel unavailable: {}",
                std::io::Error::last_os_error()
            ));
        } else {
            WHEEL_WINDOW.set(hwnd);
        }
        for (id, key, slot) in [
            (HOTKEY_BRIGHT, b'B', ProfileSlot::Bright),
            (HOTKEY_DARK, b'D', ProfileSlot::Dark),
        ] {
            if unsafe {
                RegisterHotKey(hwnd, id, MOD_SHIFT | MOD_WIN | MOD_NOREPEAT, u32::from(key))
            } != 0
            {
                hotkeys.registered.push(id);
            } else {
                errors.push(format!(
                    "{} unavailable: {}",
                    slot.shortcut(),
                    std::io::Error::last_os_error()
                ));
            }
        }
        controller.set_hotkey_error((!errors.is_empty()).then(|| errors.join("; ")));
        hotkeys
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        if !self.mouse_hook.is_null() {
            WHEEL_WINDOW.set(ptr::null_mut());
            unsafe { UnhookWindowsHookEx(self.mouse_hook) };
        }
        for id in &self.registered {
            unsafe { UnregisterHotKey(self.hwnd, *id) };
        }
    }
}

thread_local! {
    // WH_MOUSE_LL calls back on the same thread that installed the hook.
    static WHEEL_WINDOW: Cell<HWND> = const { Cell::new(ptr::null_mut()) };
}

struct WindowState {
    controller: AppController,
    scrolls: RefCell<Vec<WheelScroll>>,
    overlay: RefCell<Option<Box<Overlay>>>,
}

impl WindowState {
    fn show_overlay(&self, hwnd: HWND, monitor: isize, generation: u64, value: u32) {
        let mut overlay = self.overlay.borrow_mut();
        if overlay
            .as_mut()
            .is_some_and(|active| active.show(monitor, generation, value))
        {
            return;
        }
        *overlay = None;
        if let Ok(active) = Overlay::start(hwnd, monitor, generation, value) {
            if unsafe { SetTimer(hwnd, OVERLAY_TIMER, OVERLAY_CHECK_MS, None) } != 0 {
                *overlay = Some(active);
            }
        }
    }

    fn close_overlay(&self, hwnd: HWND) {
        unsafe { KillTimer(hwnd, OVERLAY_TIMER) };
        self.overlay.borrow_mut().take();
    }
}

struct WheelScroll {
    monitor: isize,
    generation: u64,
    remainder: i32,
    steps: i32,
}

impl WheelScroll {
    fn add_delta(&mut self, delta: i16) {
        let total = self.remainder + i32::from(delta);
        self.steps = self.steps.saturating_add(total / WHEEL_DELTA as i32);
        self.remainder = total % WHEEL_DELTA as i32;
    }
}

fn key_down(key: u16) -> bool {
    (unsafe { GetAsyncKeyState(i32::from(key)) }) < 0
}

unsafe extern "system" fn mouse_hook_proc(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && message == WM_MOUSEWHEEL as WPARAM
        && key_down(VK_SHIFT)
        && (key_down(VK_LWIN) || key_down(VK_RWIN))
        && !key_down(VK_CONTROL)
        && !key_down(VK_MENU)
    {
        let hwnd = WHEEL_WINDOW.get();
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
        if let Some(state) = state.as_ref() {
            let event = &*(data as *const MSLLHOOKSTRUCT);
            // Windows can enter the hook with a different DPI context. Its
            // event coordinates still use the per-monitor-aware desktop.
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
            if !previous.is_null() {
                let monitor = MonitorFromPoint(event.pt, MONITOR_DEFAULTTONULL) as isize;
                SetThreadDpiAwarenessContext(previous);
                if queue_wheel(hwnd, state, monitor, (event.mouseData >> 16) as i16) {
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(ptr::null_mut(), code, message, data)
}

fn queue_wheel(hwnd: HWND, state: &WindowState, monitor: isize, delta: i16) -> bool {
    if state.controller.is_quitting() {
        return false;
    }
    let Some(generation) = state.controller.wheel_target_generation(monitor) else {
        return false;
    };
    let Ok(mut scrolls) = state.scrolls.try_borrow_mut() else {
        return false;
    };
    scrolls.retain(|scroll| scroll.generation == generation);
    let was_pending = scrolls.iter().any(|scroll| scroll.steps != 0);
    let mut scroll = scrolls
        .iter()
        .position(|scroll| scroll.monitor == monitor)
        .map(|index| scrolls.remove(index))
        .unwrap_or(WheelScroll {
            monitor,
            generation,
            remainder: 0,
            steps: 0,
        });
    scroll.add_delta(delta);
    if !was_pending
        && scroll.steps != 0
        && unsafe { SetTimer(hwnd, WHEEL_TIMER, WHEEL_BATCH_MS, None) } == 0
    {
        scrolls.clear();
        return false;
    }
    // The most recent target goes last so the overlay follows it after the batch.
    scrolls.push(scroll);
    true
}

pub(crate) struct DisplayWatcher {
    hwnd: isize,
    thread: Option<thread::JoinHandle<()>>,
}

impl DisplayWatcher {
    pub(crate) fn start(controller: AppController) -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("display-events".into())
            .spawn(move || {
                if let Err(error) = run(controller, &ready_tx) {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|err| err.to_string())?;

        match ready_rx.recv() {
            Ok(Ok(hwnd)) => Ok(Self {
                hwnd,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "display notification thread stopped during startup".into(),
                })
            }
        }
    }
}

impl Drop for DisplayWatcher {
    fn drop(&mut self) {
        unsafe { PostMessageW(self.hwnd as HWND, WM_STOP, 0, 0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct Notifications {
    lid: HPOWERNOTIFY,
    display: HPOWERNOTIFY,
    devices: HDEVNOTIFY,
}

impl Drop for Notifications {
    fn drop(&mut self) {
        unsafe {
            if self.lid != 0 {
                UnregisterPowerSettingNotification(self.lid);
            }
            if self.display != 0 {
                UnregisterPowerSettingNotification(self.display);
            }
            if !self.devices.is_null() {
                UnregisterDeviceNotification(self.devices);
            }
        }
    }
}

fn run(
    controller: AppController,
    ready_tx: &mpsc::Sender<Result<isize, String>>,
) -> Result<(), String> {
    // Several controllers can exist in tests; register the class only once.
    static REGISTERED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let name: Vec<u16> = "KemoLightDisplayEvents\0".encode_utf16().collect();
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    REGISTERED
        .get_or_init(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0 {
                Err(std::io::Error::last_os_error().to_string())
            } else {
                Ok(())
            }
        })
        .clone()?;

    // This stack value outlives the window and every callback on this thread.
    let mut state = WindowState {
        controller,
        scrolls: RefCell::new(Vec::new()),
        overlay: RefCell::new(None),
    };
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            name.as_ptr(),
            name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            (&mut state as *mut WindowState).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let mut notifications = Notifications::default();
    unsafe {
        // Windows sends the initial known lid state too, so starting with an
        // already closed lid does not require an open/close cycle.
        notifications.lid = RegisterPowerSettingNotification(
            hwnd,
            &GUID_LIDSWITCH_STATE_CHANGE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        );
        notifications.display = RegisterPowerSettingNotification(
            hwnd,
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        );
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE,
            dbcc_classguid: GUID_DEVINTERFACE_MONITOR,
            ..Default::default()
        };
        notifications.devices = RegisterDeviceNotificationW(
            hwnd,
            (&filter as *const DEV_BROADCAST_DEVICEINTERFACE_W).cast(),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        );
    }
    if notifications.lid == 0 || notifications.display == 0 || notifications.devices.is_null() {
        let error = std::io::Error::last_os_error().to_string();
        unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }

    let _hotkeys = Hotkeys::register(hwnd, instance, &state.controller);
    let _ = ready_tx.send(Ok(hwnd as isize));
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result <= 0 {
            if result < 0 {
                unsafe { DestroyWindow(hwnd) };
            }
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe fn schedule_refresh(hwnd: HWND, state: &WindowState) {
    state.controller.invalidate_monitor_targets();
    state.scrolls.borrow_mut().clear();
    KillTimer(hwnd, WHEEL_TIMER);
    state.close_overlay(hwnd);
    // Reset both timers on each event to coalesce a burst. The second pass
    // handles drivers whose WMI/DDC endpoints lag behind the display broadcast.
    SetTimer(hwnd, REFRESH_TIMER, REFRESH_DELAY_MS, None);
    SetTimer(hwnd, SETTLED_REFRESH_TIMER, SETTLED_REFRESH_DELAY_MS, None);
}

unsafe fn power_setting_value(lparam: LPARAM) -> Option<(windows_sys::core::GUID, u32)> {
    if lparam == 0 {
        return None;
    }
    let setting = &*(lparam as *const POWERBROADCAST_SETTING);
    if setting.DataLength != mem::size_of::<u32>() as u32 {
        return None;
    }
    Some((
        setting.PowerSetting,
        ptr::read_unaligned(
            (lparam as *const u8)
                .add(mem::offset_of!(POWERBROADCAST_SETTING, Data))
                .cast::<u32>(),
        ),
    ))
}

#[cfg(all(test, feature = "mock-backend"))]
mod tests {
    use super::*;
    use crate::{
        brightness::{mock::MockBrightnessBackend, BrightnessBackend, DisplayInfo, DisplayKind},
        config::AppConfig,
    };
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;

    // Same native header with enough storage for the DWORD payload.
    #[repr(C)]
    struct PowerSetting {
        guid: windows_sys::core::GUID,
        len: u32,
        value: u32,
    }

    fn send_lid(hwnd: HWND, value: u32) {
        let setting = PowerSetting {
            guid: GUID_LIDSWITCH_STATE_CHANGE,
            len: 4,
            value,
        };
        unsafe {
            SendMessageW(
                hwnd,
                WM_POWERBROADCAST,
                PBT_POWERSETTINGCHANGE as usize,
                &setting as *const PowerSetting as isize,
            );
        }
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "display notification timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn native_notifications_hide_restore_and_refresh_without_tray() {
        struct Backend {
            inner: MockBrightnessBackend,
            external_connected: AtomicBool,
        }
        impl BrightnessBackend for Backend {
            fn list_displays(&self) -> crate::brightness::Result<Vec<DisplayInfo>> {
                let mut displays = self.inner.list_displays()?;
                if !self.external_connected.load(Ordering::SeqCst) {
                    displays.retain(|display| display.kind != DisplayKind::ExternalDdcCi);
                }
                Ok(displays)
            }
            fn get_brightness(
                &self,
                id: &crate::brightness::DisplayId,
            ) -> crate::brightness::Result<crate::brightness::BrightnessValue> {
                self.inner.get_brightness(id)
            }
            fn set_brightness(
                &self,
                id: &crate::brightness::DisplayId,
                value: u32,
            ) -> crate::brightness::Result<()> {
                self.inner.set_brightness(id, value)
            }
        }
        let backend = Arc::new(Backend {
            inner: MockBrightnessBackend::default(),
            external_connected: AtomicBool::new(true),
        });
        let controller = AppController::with_backend(AppConfig::default(), backend.clone());
        let watcher = DisplayWatcher::start(controller.clone()).unwrap();
        let hwnd = watcher.hwnd as HWND;
        assert!(!controller.tray_available());

        // Deliver a closed-lid notification before any open event, as at startup.
        send_lid(hwnd, 0);
        wait_for(|| {
            let displays = controller.snapshot().displays;
            displays.len() == 1 && displays[0].info.kind == DisplayKind::ExternalDdcCi
        });
        send_lid(hwnd, 1);
        wait_for(|| controller.snapshot().displays.len() == 2);

        eprintln!(
            "Global hotkey registration: {:?}",
            controller.snapshot().hotkey_error
        );
        controller.save_profile(ProfileSlot::Bright);
        wait_for(|| controller.can_apply_profile(ProfileSlot::Bright));
        let internal = crate::brightness::DisplayId::new("mock:internal");
        let external = crate::brightness::DisplayId::new("mock:external");
        controller.set_brightness(internal.clone(), 15);
        controller.set_brightness(external.clone(), 25);
        wait_for(|| {
            controller
                .snapshot()
                .displays
                .iter()
                .all(|display| !display.pending)
        });
        controller.save_profile(ProfileSlot::Dark);
        wait_for(|| controller.can_apply_profile(ProfileSlot::Dark));
        unsafe { PostMessageW(hwnd, WM_HOTKEY, HOTKEY_BRIGHT as usize, 0) };
        wait_for(|| {
            backend.get_brightness(&internal).unwrap().percent() == 60
                && backend.get_brightness(&external).unwrap().percent() == 70
        });
        unsafe { PostMessageW(hwnd, WM_HOTKEY, HOTKEY_DARK as usize, 0) };
        wait_for(|| {
            backend.get_brightness(&internal).unwrap().percent() == 15
                && backend.get_brightness(&external).unwrap().percent() == 25
        });

        backend.external_connected.store(false, Ordering::SeqCst);
        unsafe { PostMessageW(hwnd, WM_DISPLAYCHANGE, 0, 0) };
        wait_for(|| controller.snapshot().displays.len() == 1);
        assert_eq!(
            controller.snapshot().displays[0].info.kind,
            DisplayKind::InternalBacklight
        );

        backend.external_connected.store(true, Ordering::SeqCst);
        unsafe { PostMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVNODES_CHANGED as usize, 0) };
        wait_for(|| controller.snapshot().displays.len() == 2);
        controller.shutdown();
        drop(watcher); // Stops the native message loop and unregisters notifications.
    }

    #[test]
    fn wheel_delta_accumulates_fractional_notches_and_signed_steps() {
        let mut scroll = WheelScroll {
            monitor: 1,
            generation: 0,
            remainder: 0,
            steps: 0,
        };
        scroll.add_delta(60);
        assert_eq!((scroll.steps, scroll.remainder), (0, 60));
        scroll.add_delta(180);
        assert_eq!((scroll.steps, scroll.remainder), (2, 0));
        scroll.add_delta(-150);
        assert_eq!((scroll.steps, scroll.remainder), (1, -30));
        scroll.add_delta(-90);
        assert_eq!((scroll.steps, scroll.remainder), (0, 0));
    }

    #[test]
    fn power_payload_uses_full_dword_and_rejects_missing_or_short_data() {
        let mut setting = PowerSetting {
            guid: GUID_LIDSWITCH_STATE_CHANGE,
            len: 4,
            value: 0x0100_0000,
        };
        let address = &setting as *const PowerSetting as isize;
        assert_eq!(
            unsafe { power_setting_value(address) }.unwrap().1,
            0x0100_0000
        );
        setting.len = 1;
        assert!(unsafe { power_setting_value(&setting as *const PowerSetting as isize) }.is_none());
        assert!(unsafe { power_setting_value(0) }.is_none());
    }
}

fn same_guid(left: windows_sys::core::GUID, right: windows_sys::core::GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
    let Some(state) = state.as_ref() else {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    };
    let controller = &state.controller;
    match message {
        WM_HOTKEY => {
            if !controller.is_quitting() {
                match wparam as i32 {
                    HOTKEY_BRIGHT => controller.apply_profile(ProfileSlot::Bright),
                    HOTKEY_DARK => controller.apply_profile(ProfileSlot::Dark),
                    _ => {}
                }
            }
            0
        }
        WM_DISPLAYCHANGE => {
            schedule_refresh(hwnd, state);
            0
        }
        WM_DEVICECHANGE
            if matches!(
                wparam as u32,
                DBT_DEVNODES_CHANGED | DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE
            ) =>
        {
            schedule_refresh(hwnd, state);
            1
        }
        WM_POWERBROADCAST => {
            if wparam as u32 == PBT_POWERSETTINGCHANGE {
                if let Some((guid, value)) = power_setting_value(lparam) {
                    if same_guid(guid, GUID_LIDSWITCH_STATE_CHANGE) && value <= 1 {
                        controller.set_lid_closed(value == 0);
                        schedule_refresh(hwnd, state);
                    } else if same_guid(guid, GUID_CONSOLE_DISPLAY_STATE) {
                        schedule_refresh(hwnd, state);
                    }
                }
            } else if matches!(wparam as u32, PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND) {
                schedule_refresh(hwnd, state);
            }
            1
        }
        WM_TIMER if matches!(wparam, REFRESH_TIMER | SETTLED_REFRESH_TIMER) => {
            KillTimer(hwnd, wparam);
            if !controller.is_quitting() {
                controller.refresh();
            }
            0
        }
        WM_TIMER if wparam == WHEEL_TIMER => {
            KillTimer(hwnd, WHEEL_TIMER);
            let mut feedback = None;
            for scroll in state.scrolls.borrow_mut().iter_mut() {
                let steps = mem::take(&mut scroll.steps);
                if steps != 0 && !controller.is_quitting() {
                    if let Some(value) =
                        controller.adjust_wheel_target(scroll.monitor, scroll.generation, steps)
                    {
                        feedback = Some((scroll.monitor, scroll.generation, value));
                    }
                }
            }
            if let Some((monitor, generation, value)) = feedback {
                state.show_overlay(hwnd, monitor, generation, value);
            }
            0
        }
        WM_OVERLAY_READY => {
            if let Some(overlay) = state.overlay.borrow_mut().as_mut() {
                overlay.ready(wparam as HWND);
            }
            0
        }
        WM_TIMER if wparam == OVERLAY_TIMER => {
            let alive = state
                .overlay
                .borrow_mut()
                .as_mut()
                .is_some_and(|overlay| overlay.refresh(controller));
            if !alive {
                state.close_overlay(hwnd);
            }
            0
        }
        WM_STOP => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, REFRESH_TIMER);
            KillTimer(hwnd, SETTLED_REFRESH_TIMER);
            KillTimer(hwnd, WHEEL_TIMER);
            state.close_overlay(hwnd);
            PostQuitMessage(0);
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
