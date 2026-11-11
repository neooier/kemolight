//! Disposable Windows brightness feedback. Only the tray process changes brightness.

use std::{
    cell::Cell,
    io,
    process::{Child, Command, Stdio},
    ptr,
    time::{Duration, Instant},
};

use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
            EndPaint, FillRect, GetMonitorInfoW, GetStockObject, InvalidateRect, RoundRect,
            SelectObject, SetBkMode, SetTextColor, SetWindowRgn, DT_CENTER, DT_SINGLELINE,
            DT_VCENTER, HDC, HMONITOR, MONITORINFO, NULL_PEN, PAINTSTRUCT, TRANSPARENT,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{
                GetDpiForWindow, SetThreadDpiAwarenessContext,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
                GetWindowLongPtrW, GetWindowThreadProcessId, PostMessageW, PostQuitMessage,
                RegisterClassW, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW,
                SetWindowPos, GWLP_USERDATA, HWND_TOP, LWA_ALPHA, MSG, SWP_NOACTIVATE, SWP_NOMOVE,
                SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WM_APP, WM_DESTROY, WM_PAINT, WM_TIMER,
                WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_EX_TRANSPARENT, WS_POPUP,
            },
        },
    },
};

use crate::app::AppController;

const CHILD_ARGUMENT: &str = "--brightness-overlay";
pub(crate) const WM_OVERLAY_READY: u32 = WM_APP + 2;
const WM_OVERLAY_VALUE: u32 = WM_APP + 3;
const DISPLAY_TIME: Duration = Duration::from_millis(850);
const ORPHAN_TIMEOUT_MS: u32 = 1500;

pub(crate) struct Overlay {
    process: Child,
    window: HWND,
    monitor: isize,
    generation: u64,
    value: u32,
    last_scroll: Instant,
}

impl Overlay {
    pub(crate) fn start(
        owner: HWND,
        monitor: isize,
        generation: u64,
        value: u32,
    ) -> io::Result<Box<Self>> {
        let process = Command::new(std::env::current_exe()?)
            .arg(CHILD_ARGUMENT)
            .arg((owner as isize).to_string())
            .arg(monitor.to_string())
            .arg(value.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Box::new(Self {
            process,
            window: ptr::null_mut(),
            monitor,
            generation,
            value,
            last_scroll: Instant::now(),
        }))
    }

    pub(crate) fn ready(&mut self, window: HWND) {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        if pid == self.process.id() {
            self.window = window;
            self.post_value();
        }
    }

    pub(crate) fn show(&mut self, monitor: isize, generation: u64, value: u32) -> bool {
        if !matches!(self.process.try_wait(), Ok(None)) {
            return false;
        }
        self.monitor = monitor;
        self.generation = generation;
        self.value = value;
        self.last_scroll = Instant::now();
        self.post_value()
    }

    pub(crate) fn refresh(&mut self, controller: &AppController) -> bool {
        if self.last_scroll.elapsed() >= DISPLAY_TIME
            || !matches!(self.process.try_wait(), Ok(None))
        {
            return false;
        }
        let Some(value) = controller.wheel_brightness(self.monitor, self.generation) else {
            return false;
        };
        if value == self.value {
            return true;
        }
        self.value = value;
        self.post_value()
    }

    fn post_value(&self) -> bool {
        self.window.is_null()
            || unsafe {
                PostMessageW(
                    self.window,
                    WM_OVERLAY_VALUE,
                    self.monitor as usize,
                    self.value as isize,
                )
            } != 0
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // The overlay holds no saved state. Keep process cleanup off the input thread's wait path.
        if matches!(self.process.try_wait(), Ok(None)) {
            let _ = self.process.kill();
        }
    }
}

pub(crate) fn run_child_if_requested() -> bool {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(CHILD_ARGUMENT.as_ref()) {
        return false;
    }
    let mut number = || {
        args.next()
            .and_then(|arg| arg.to_str()?.parse::<isize>().ok())
    };
    if let (Some(owner), Some(monitor), Some(value)) = (number(), number(), number()) {
        if (0..=100).contains(&value) {
            unsafe { run_overlay(owner as HWND, monitor, value as u32) };
        }
    }
    true
}

#[derive(Default)]
struct View {
    monitor: Cell<isize>,
    value: Cell<u32>,
    dpi: Cell<u32>,
}

unsafe fn run_overlay(owner: HWND, monitor: isize, value: u32) {
    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
    let instance = GetModuleHandleW(ptr::null());
    let class = w!("KemoLightBrightnessOverlay");
    if RegisterClassW(&WNDCLASSW {
        hInstance: instance,
        lpszClassName: class,
        lpfnWndProc: Some(window_proc),
        ..Default::default()
    }) == 0
    {
        return;
    }
    let view = View::default();
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
        class,
        class,
        WS_POPUP,
        0,
        0,
        1,
        1,
        ptr::null_mut(),
        ptr::null_mut(),
        instance,
        ptr::null(),
    );
    if hwnd.is_null() {
        return;
    }
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, &view as *const View as isize);
    SetLayeredWindowAttributes(hwnd, 0, 242, LWA_ALPHA);
    if !view.show(hwnd, monitor, value)
        || PostMessageW(owner, WM_OVERLAY_READY, hwnd as usize, 0) == 0
    {
        DestroyWindow(hwnd);
        return;
    }
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
        DispatchMessageW(&msg);
    }
}

impl View {
    unsafe fn show(&self, hwnd: HWND, monitor: isize, value: u32) -> bool {
        self.value.set(value.min(100));
        let mut flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
        let (x, y, width, height) = if self.monitor.get() != monitor {
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor as HMONITOR, &mut info) == 0 {
                return false;
            }
            let work = info.rcWork;
            let center = work.left + (work.right - work.left) / 2;
            // Move onto the destination first so GetDpiForWindow uses its scaling.
            SetWindowPos(
                hwnd,
                ptr::null_mut(),
                center - 1,
                work.bottom - 80,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
            );
            self.dpi.set(GetDpiForWindow(hwnd).max(96));
            let (width, height) = (self.scale(210), self.scale(36));
            let region =
                CreateRoundRectRgn(0, 0, width + 1, height + 1, self.scale(13), self.scale(13));
            if !region.is_null() && SetWindowRgn(hwnd, region, 0) == 0 {
                DeleteObject(region);
            }
            (
                center - width / 2,
                work.bottom - self.scale(28) - height,
                width,
                height,
            )
        } else {
            flags |= SWP_NOMOVE | SWP_NOSIZE;
            (0, 0, 0, 0)
        };
        // Raise within the topmost band on every update, including on the same monitor.
        if SetWindowPos(hwnd, HWND_TOP, x, y, width, height, flags) == 0 {
            return false;
        }
        self.monitor.set(monitor);
        InvalidateRect(hwnd, ptr::null(), 0);
        SetTimer(hwnd, 1, ORPHAN_TIMEOUT_MS, None) != 0
    }

    fn scale(&self, pixels: i32) -> i32 {
        (pixels * self.dpi.get() as i32 + 48) / 96
    }

    unsafe fn paint(&self, hwnd: HWND) {
        let mut paint = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut paint);
        let rect = |left, top, right, bottom| RECT {
            left: self.scale(left),
            top: self.scale(top),
            right: self.scale(right),
            bottom: self.scale(bottom),
        };
        let background = CreateSolidBrush(0x00222120);
        FillRect(dc, &rect(0, 0, 210, 36), background);
        DeleteObject(background);

        let value = self.value.get();
        let knob = 32 + (128 * value as i32 + 50) / 100;
        let accent = 0x0086D8FF;
        let pen = SelectObject(dc, GetStockObject(NULL_PEN));
        rounded_rect(dc, rect(32, 16, 160, 20), 0x00504C46, self.scale(4));
        if knob > 32 {
            rounded_rect(dc, rect(32, 16, knob, 20), accent, self.scale(4));
        }
        rounded_rect(dc, rect(knob - 4, 14, knob + 4, 22), accent, self.scale(8));
        SelectObject(dc, pen);

        let font = CreateFontW(
            -self.scale(13),
            0,
            0,
            0,
            500,
            0,
            0,
            0,
            1,
            0,
            0,
            5,
            0,
            w!("Segoe UI"),
        );
        let previous_font = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00F4F2EF);
        let label: Vec<u16> = format!("{value}%").encode_utf16().collect();
        let flags = DT_CENTER | DT_SINGLELINE | DT_VCENTER;
        DrawTextW(
            dc,
            label.as_ptr(),
            label.len() as i32,
            &mut rect(165, 0, 204, 36),
            flags,
        );
        DrawTextW(dc, w!("☀"), 1, &mut rect(5, 0, 29, 36), flags);
        SelectObject(dc, previous_font);
        DeleteObject(font);
        EndPaint(hwnd, &paint);
    }
}

unsafe fn rounded_rect(dc: HDC, bounds: RECT, color: u32, diameter: i32) {
    let brush = CreateSolidBrush(color);
    let previous = SelectObject(dc, brush);
    RoundRect(
        dc,
        bounds.left,
        bounds.top,
        bounds.right,
        bounds.bottom,
        diameter,
        diameter,
    );
    SelectObject(dc, previous);
    DeleteObject(brush);
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let view = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const View;
    if let Some(view) = view.as_ref() {
        match message {
            WM_PAINT => {
                view.paint(hwnd);
                return 0;
            }
            WM_OVERLAY_VALUE => {
                if !view.show(hwnd, wparam as isize, lparam as u32) {
                    DestroyWindow(hwnd);
                }
                return 0;
            }
            WM_TIMER => {
                DestroyWindow(hwnd);
                return 0;
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}
