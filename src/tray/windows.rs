use std::{collections::HashMap, mem, ptr, sync::mpsc, thread};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{
            Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW,
            DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW,
            GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadIconW, PostMessageW, PostQuitMessage,
            RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu,
            TranslateMessage, CREATESTRUCTW, GWLP_USERDATA, HICON, HMENU, IDI_APPLICATION,
            MF_DISABLED, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG,
            TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_TOPALIGN, WM_APP, WM_CONTEXTMENU,
            WM_DESTROY, WM_LBUTTONUP, WM_NCCREATE, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
        },
    },
};

use crate::{
    app::{AppController, DisplayState},
    brightness::DisplayId,
    profiles::ProfileSlot,
};

use super::TrayHandle;

const TRAY_UID: u32 = 1;
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_TRAY_SHUTDOWN: u32 = WM_APP + 3;
const WM_TRAY_OPEN_WINDOW: u32 = WM_APP + 4;

const CMD_OPEN: u32 = 100;
const CMD_REFRESH: u32 = 101;
const CMD_QUIT: u32 = 102;
const CMD_BRIGHT: u32 = 103;
const CMD_DARK: u32 = 104;
const CMD_DYNAMIC_START: u32 = 1000;
const ICON_RESOURCE_VERSION: u32 = 0x0003_0000;
const TRAY_ICON_SIZE: u16 = 32;
const LOGO_ICO: &[u8] = include_bytes!("../../assets/logo.ico");

struct WindowsTray {
    controller: AppController,
    hwnd: HWND,
    hicon: HICON,
    icon_added: bool,
    owns_icon: bool,
}

#[derive(Clone)]
enum TrayCommand {
    Open,
    Refresh,
    Quit,
    Adjust(DisplayId, i32),
    Set(DisplayId, u32),
    ApplyProfile(ProfileSlot),
}

pub fn start_tray(controller: AppController) -> TrayHandle {
    let (hwnd_tx, hwnd_rx) = mpsc::channel();

    thread::spawn(move || {
        if let Err(err) = run_tray_thread(controller, hwnd_tx) {
            eprintln!("Windows tray unavailable, falling back to window-only mode: {err}");
        }
    });

    let hwnd = match hwnd_rx.recv() {
        Ok(Some(hwnd)) => hwnd,
        Ok(None) | Err(_) => return TrayHandle::unavailable(),
    };

    // The native menu reads a fresh snapshot when opened. No update queue or
    // forwarding thread is needed while the tray is idle.
    TrayHandle::available(None, move || unsafe {
        PostMessageW(hwnd as HWND, WM_TRAY_SHUTDOWN, 0, 0);
    })
}

fn run_tray_thread(
    controller: AppController,
    hwnd_tx: mpsc::Sender<Option<isize>>,
) -> Result<(), String> {
    let class_name = wide("KemoLightTrayWindow");
    let window_name = wide("KemoLight Tray");
    let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
    if hinstance.is_null() {
        let _ = hwnd_tx.send(None);
        return Err("GetModuleHandleW failed".to_string());
    }

    let wnd_class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: hinstance,
        lpszClassName: class_name.as_ptr(),
        ..Default::default()
    };

    unsafe {
        RegisterClassW(&wnd_class);
    }

    let tray = Box::new(WindowsTray {
        controller,
        hwnd: ptr::null_mut(),
        hicon: ptr::null_mut(),
        icon_added: false,
        owns_icon: false,
    });
    let tray_ptr = Box::into_raw(tray);

    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            tray_ptr.cast(),
        )
    };

    if hwnd.is_null() {
        unsafe {
            drop(Box::from_raw(tray_ptr));
        }
        let _ = hwnd_tx.send(None);
        return Err("CreateWindowExW failed".to_string());
    }

    if let Err(err) = unsafe { tray_from_hwnd(hwnd) }.and_then(|tray| tray.add_icon()) {
        unsafe {
            DestroyWindow(hwnd);
        }
        let _ = hwnd_tx.send(None);
        return Err(err);
    }

    let _ = hwnd_tx.send(Some(hwnd as isize));

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    Ok(())
}

impl WindowsTray {
    fn add_icon(&mut self) -> Result<(), String> {
        let mut data = self.notify_data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY_CALLBACK;
        let (hicon, owns_icon) = load_tray_icon();
        self.hicon = hicon;
        self.owns_icon = owns_icon;
        data.hIcon = self.hicon;
        set_wide_buffer(&mut data.szTip, "KemoLight");

        let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
        if ok == 0 {
            return Err("Shell_NotifyIconW(NIM_ADD) failed".to_string());
        }

        self.icon_added = true;
        Ok(())
    }

    fn remove_icon(&mut self) {
        if !self.icon_added {
            return;
        }

        let data = self.notify_data();
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &data);
        }
        self.icon_added = false;
        self.destroy_icon();
    }

    fn destroy_icon(&mut self) {
        if self.owns_icon && !self.hicon.is_null() {
            unsafe {
                DestroyIcon(self.hicon);
            }
        }
        self.hicon = ptr::null_mut();
        self.owns_icon = false;
    }

    fn notify_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_UID,
            ..Default::default()
        }
    }

    fn show_menu(&self) {
        let Some(menu) = PopupMenu::new() else {
            return;
        };
        let mut commands = HashMap::new();
        let mut next_id = CMD_DYNAMIC_START;

        append_command(menu.handle, CMD_OPEN, "Open Window", true);
        commands.insert(CMD_OPEN, TrayCommand::Open);
        append_command(menu.handle, CMD_REFRESH, "Refresh Displays", true);
        commands.insert(CMD_REFRESH, TrayCommand::Refresh);
        append_separator(menu.handle);

        for (command_id, slot) in [
            (CMD_BRIGHT, ProfileSlot::Bright),
            (CMD_DARK, ProfileSlot::Dark),
        ] {
            append_command(
                menu.handle,
                command_id,
                &format!("{} Profile\t{}", slot.label(), slot.shortcut()),
                self.controller.can_apply_profile(slot),
            );
            commands.insert(command_id, TrayCommand::ApplyProfile(slot));
        }
        append_separator(menu.handle);

        let snapshot = self.controller.snapshot();
        for error in [&snapshot.profile_error, &snapshot.hotkey_error]
            .into_iter()
            .flatten()
        {
            append_command(menu.handle, 0, error, false);
        }
        if let Some(error) = snapshot.last_error {
            append_command(menu.handle, 0, &error, false);
            append_separator(menu.handle);
        }

        if snapshot.displays.is_empty() {
            append_command(menu.handle, 0, "No displays detected", false);
        } else {
            for display in snapshot.displays {
                self.append_display_menu(menu.handle, display, &mut next_id, &mut commands);
            }
        }

        append_separator(menu.handle);
        append_command(menu.handle, CMD_QUIT, "Quit", true);
        commands.insert(CMD_QUIT, TrayCommand::Quit);

        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point);
            SetForegroundWindow(self.hwnd);
        }
        let command_id = unsafe {
            TrackPopupMenu(
                menu.handle,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN,
                point.x,
                point.y,
                0,
                self.hwnd,
                ptr::null::<RECT>(),
            )
        };
        unsafe {
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
        }

        if command_id != 0 {
            if let Some(command) = commands.get(&(command_id as u32)).cloned() {
                self.run_command(command);
            }
        }
    }

    fn append_display_menu(
        &self,
        parent: HMENU,
        display: DisplayState,
        next_id: &mut u32,
        commands: &mut HashMap<u32, TrayCommand>,
    ) {
        let Some(submenu) = PopupMenu::new() else {
            return;
        };

        let display_id = display.info.id.clone();
        let current = display
            .info
            .current
            .map(|value| format!("{value}%"))
            .unwrap_or_else(|| "unknown".to_string());
        let label = format!("{} ({current})", display.info.name);
        let enabled = display.info.supported;

        let plus_id = allocate_command(next_id);
        append_command(submenu.handle, plus_id, "Brightness +5", enabled);
        commands.insert(plus_id, TrayCommand::Adjust(display_id.clone(), 1));

        let minus_id = allocate_command(next_id);
        append_command(submenu.handle, minus_id, "Brightness -5", enabled);
        commands.insert(minus_id, TrayCommand::Adjust(display_id.clone(), -1));

        append_separator(submenu.handle);
        for value in [25, 50, 75, 100] {
            let command_id = allocate_command(next_id);
            append_command(
                submenu.handle,
                command_id,
                &format!("Set {value}%"),
                enabled,
            );
            commands.insert(command_id, TrayCommand::Set(display_id.clone(), value));
        }

        append_submenu(parent, submenu.handle, &label, true);
        mem::forget(submenu);
    }

    fn run_command(&self, command: TrayCommand) {
        match command {
            TrayCommand::Open => self.controller.request_open_window(),
            TrayCommand::Refresh => self.controller.refresh(),
            TrayCommand::ApplyProfile(slot) => self.controller.apply_profile(slot),
            TrayCommand::Quit => self.controller.request_quit(),
            TrayCommand::Adjust(display_id, delta) => {
                self.controller.adjust_brightness(display_id, delta)
            }
            TrayCommand::Set(display_id, value) => {
                self.controller.set_brightness(display_id, value)
            }
        }
    }
}

struct PopupMenu {
    handle: HMENU,
}

impl PopupMenu {
    fn new() -> Option<Self> {
        let handle = unsafe { CreatePopupMenu() };
        (!handle.is_null()).then_some(Self { handle })
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        unsafe {
            DestroyMenu(self.handle);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let createstruct = lparam as *const CREATESTRUCTW;
        let tray = (*createstruct).lpCreateParams as *mut WindowsTray;
        (*tray).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, tray as isize);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    let tray = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowsTray;
    if tray.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    match message {
        WM_TRAY_CALLBACK => {
            let event = lparam as u32;
            match event {
                WM_LBUTTONUP => (*tray).controller.request_open_window(),
                WM_RBUTTONUP | WM_CONTEXTMENU => (*tray).show_menu(),
                _ => {}
            }
            0
        }
        WM_TRAY_OPEN_WINDOW => {
            (*tray).controller.request_open_window();
            0
        }
        WM_TRAY_SHUTDOWN => {
            (*tray).remove_icon();
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            (*tray).remove_icon();
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(tray));
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn tray_from_hwnd(hwnd: HWND) -> Result<&'static mut WindowsTray, String> {
    let tray = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowsTray;
    if tray.is_null() {
        Err("tray window was created without runtime state".to_string())
    } else {
        Ok(&mut *tray)
    }
}

fn allocate_command(next_id: &mut u32) -> u32 {
    let command_id = *next_id;
    *next_id += 1;
    command_id
}

fn append_command(menu: HMENU, command_id: u32, label: &str, enabled: bool) {
    let label = wide(label);
    let flags = MF_STRING
        | if enabled {
            MF_ENABLED
        } else {
            MF_DISABLED | MF_GRAYED
        };
    unsafe {
        AppendMenuW(menu, flags, command_id as usize, label.as_ptr());
    }
}

fn append_submenu(menu: HMENU, submenu: HMENU, label: &str, enabled: bool) {
    let label = wide(label);
    let flags = MF_POPUP
        | MF_STRING
        | if enabled {
            MF_ENABLED
        } else {
            MF_DISABLED | MF_GRAYED
        };
    unsafe {
        AppendMenuW(menu, flags, submenu as usize, label.as_ptr());
    }
}

fn append_separator(menu: HMENU) {
    unsafe {
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
    }
}

fn load_tray_icon() -> (HICON, bool) {
    if let Some(image) = best_ico_image(LOGO_ICO, TRAY_ICON_SIZE) {
        let hicon = unsafe {
            CreateIconFromResourceEx(
                image.as_ptr(),
                image.len() as u32,
                1,
                ICON_RESOURCE_VERSION,
                TRAY_ICON_SIZE.into(),
                TRAY_ICON_SIZE.into(),
                0,
            )
        };
        if !hicon.is_null() {
            return (hicon, true);
        }
    }

    (
        unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) },
        false,
    )
}

fn best_ico_image(ico: &[u8], desired_size: u16) -> Option<&[u8]> {
    if ico.len() < 6 || u16::from_le_bytes([ico[0], ico[1]]) != 0 {
        return None;
    }
    if u16::from_le_bytes([ico[2], ico[3]]) != 1 {
        return None;
    }

    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let entries = ico.get(6..6 + count.checked_mul(16)?)?;
    let mut best = None;
    let mut best_score = u16::MAX;

    for entry in entries.as_chunks::<16>().0 {
        let width = if entry[0] == 0 { 256 } else { entry[0] as u16 };
        let height = if entry[1] == 0 { 256 } else { entry[1] as u16 };
        if width != height {
            continue;
        }

        let size = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let image = ico.get(offset..offset.checked_add(size)?)?;
        let score = width.abs_diff(desired_size);

        if score < best_score {
            best = Some(image);
            best_score = score;
        }
    }

    best
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_wide_buffer<const N: usize>(buffer: &mut [u16; N], value: &str) {
    buffer.fill(0);
    for (slot, ch) in buffer
        .iter_mut()
        .take(N.saturating_sub(1))
        .zip(value.encode_utf16())
    {
        *slot = ch;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(
            wide("KemoLight"),
            vec![75, 101, 109, 111, 76, 105, 103, 104, 116, 0]
        );
    }

    #[test]
    fn wide_buffer_truncates_and_terminates() {
        let mut buffer = [1; 4];
        set_wide_buffer(&mut buffer, "abcdef");
        assert_eq!(buffer, [97, 98, 99, 0]);
    }

    #[test]
    fn embedded_logo_ico_has_icon_image() {
        assert!(best_ico_image(LOGO_ICO, TRAY_ICON_SIZE).is_some());
    }
}
