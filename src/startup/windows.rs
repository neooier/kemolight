use std::{env, io, os::windows::ffi::OsStrExt, path::Path, ptr};

use windows_sys::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{
            RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
            RRF_RT_ANY,
        },
    },
};

use crate::config::START_HIDDEN_ARGUMENT;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: PCWSTR = w!("KemoLight");

pub(crate) fn is_enabled() -> io::Result<bool> {
    is_registered(RUN_KEY)
}

pub(crate) fn set_enabled(enabled: bool) -> io::Result<()> {
    set_registration(RUN_KEY, enabled)
}

fn is_registered(key: PCWSTR) -> io::Result<bool> {
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            VALUE_NAME,
            RRF_RT_ANY,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    match status {
        ERROR_SUCCESS => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        error => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

fn set_registration(key: PCWSTR, enabled: bool) -> io::Result<()> {
    let status = if enabled {
        let command = startup_command(&env::current_exe()?)?;
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key,
                VALUE_NAME,
                REG_SZ,
                command.as_ptr().cast(),
                size_of_val(command.as_slice()) as u32,
            )
        }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key, VALUE_NAME) }
    };

    match status {
        ERROR_SUCCESS => Ok(()),
        ERROR_FILE_NOT_FOUND if !enabled => Ok(()),
        error => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

fn startup_command(exe: &Path) -> io::Result<Vec<u16>> {
    let suffix = format!("\" {START_HIDDEN_ARGUMENT}");
    let mut command: Vec<_> = std::iter::once(b'"' as u16)
        .chain(exe.as_os_str().encode_wide())
        .chain(suffix.encode_utf16())
        .collect();
    if command.len() > 260 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The executable path is too long for a Windows startup entry.",
        ));
    }
    command.push(0);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::RegDeleteKeyW;

    #[test]
    fn registration_can_be_enabled_and_removed() {
        struct TestKey(Vec<u16>);

        impl Drop for TestKey {
            fn drop(&mut self) {
                unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, self.0.as_ptr()) };
            }
        }

        let unique = tempfile::tempdir().unwrap();
        let suffix = unique.path().file_name().unwrap().to_str().unwrap();
        let key = TestKey(
            format!(r"Software\KemoLightAutostartTest-{suffix}")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect(),
        );
        let path = key.0.as_ptr();
        assert!(!is_registered(path).unwrap());
        set_registration(path, false).unwrap();
        set_registration(path, true).unwrap();
        assert!(is_registered(path).unwrap());
        set_registration(path, false).unwrap();
        assert!(!is_registered(path).unwrap());
    }

    #[test]
    fn startup_command_quotes_a_unicode_path_with_spaces() {
        let command = startup_command(Path::new(r"C:\工具 Box\kemolight.exe")).unwrap();
        let expected: Vec<_> = "\"C:\\工具 Box\\kemolight.exe\" --start-hidden\0"
            .encode_utf16()
            .collect();
        assert_eq!(command, expected);
    }
}
