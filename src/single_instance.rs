#[cfg(target_os = "windows")]
mod platform {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE},
        System::Threading::{CreateMutexW, ReleaseMutex},
        UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_APP},
    };

    const MUTEX_NAME: &str = "Local\\dev.kemolight.BrightnessController.SingleInstance";
    const TRAY_WINDOW_CLASS: &str = "KemoLightTrayWindow";
    const WM_TRAY_OPEN_WINDOW: u32 = WM_APP + 4;

    pub struct SingleInstance {
        mutex: HANDLE,
    }

    impl SingleInstance {
        pub fn acquire() -> Option<Self> {
            let name = wide(MUTEX_NAME);
            let mutex = unsafe { CreateMutexW(ptr::null(), 1, name.as_ptr()) };
            if mutex.is_null() {
                return Some(Self { mutex });
            }

            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                if !crate::config::start_hidden_requested() {
                    request_existing_instance_to_open();
                }
                unsafe {
                    CloseHandle(mutex);
                }
                return None;
            }

            Some(Self { mutex })
        }
    }

    impl Drop for SingleInstance {
        fn drop(&mut self) {
            if self.mutex.is_null() {
                return;
            }

            unsafe {
                ReleaseMutex(self.mutex);
                CloseHandle(self.mutex);
            }
        }
    }

    fn request_existing_instance_to_open() {
        let class_name = wide(TRAY_WINDOW_CLASS);
        let hwnd = unsafe { FindWindowW(class_name.as_ptr(), ptr::null()) };
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_TRAY_OPEN_WINDOW, 0, 0);
            }
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub struct SingleInstance;

    impl SingleInstance {
        pub fn acquire() -> Option<Self> {
            Some(Self)
        }
    }
}

pub use platform::SingleInstance;
