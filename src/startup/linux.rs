use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use directories::BaseDirs;

use crate::config::START_HIDDEN_ARGUMENT;

fn entry_path() -> io::Result<PathBuf> {
    let dirs = BaseDirs::new().ok_or_else(|| io::Error::other("No user config directory."))?;
    Ok(dirs
        .config_dir()
        .join("autostart")
        .join("dev.kemolight.BrightnessController.desktop"))
}

pub(crate) fn is_enabled() -> io::Result<bool> {
    is_registered(&entry_path()?)
}

pub(crate) fn set_enabled(enabled: bool) -> io::Result<()> {
    let path = entry_path()?;
    let entry = if enabled {
        Some(desktop_entry(&env::current_exe()?)?)
    } else {
        None
    };
    set_registration(&path, entry.as_deref())
}

fn is_registered(path: &Path) -> io::Result<bool> {
    match fs::read_to_string(path) {
        Ok(entry) => Ok(!entry.lines().any(|line| {
            matches!(
                line.split_once('=')
                    .map(|(key, value)| (key.trim(), value.trim())),
                Some(("Hidden", "true") | ("X-GNOME-Autostart-enabled", "false"))
            )
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn set_registration(path: &Path, entry: Option<&str>) -> io::Result<()> {
    if let Some(entry) = entry {
        let directory = path
            .parent()
            .ok_or_else(|| io::Error::other("No user autostart directory."))?;
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(entry.as_bytes())?;
        file.persist(path)?;
        return Ok(());
    }

    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn desktop_entry(exe: &Path) -> io::Result<String> {
    let exe = exe.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "The executable path is not UTF-8.",
        )
    })?;
    let mut quoted = String::from("\"");
    // Desktop entry value unescaping runs before Exec argument unquoting.
    for character in exe.chars() {
        match character {
            '\\' => quoted.push_str(r"\\\\"),
            '"' | '$' | '`' => {
                quoted.push_str(r"\\");
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            '\t' => quoted.push_str(r"\t"),
            '\n' => quoted.push_str(r"\n"),
            '\r' => quoted.push_str(r"\r"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');

    // GIO looks up the first word before expanding %% in Exec.
    let launcher = if exe.contains('%') {
        if exe.contains('=') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "The executable path contains both '%' and '='.",
            ));
        }
        "/usr/bin/env -- "
    } else {
        ""
    };
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=KemoLight\nExec={launcher}{quoted} {START_HIDDEN_ARGUMENT}\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_can_be_enabled_and_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("autostart").join("kemolight.desktop");
        let entry = desktop_entry(Path::new("/opt/Kemo Light/kemolight")).unwrap();

        assert!(!is_registered(&path).unwrap());
        set_registration(&path, None).unwrap();
        set_registration(&path, Some(&entry)).unwrap();
        assert!(is_registered(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), entry);

        fs::write(&path, format!("{entry}Hidden=true\n")).unwrap();
        assert!(!is_registered(&path).unwrap());
        set_registration(&path, Some(&entry)).unwrap();
        assert!(is_registered(&path).unwrap());

        set_registration(&path, None).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn desktop_entry_escapes_exec_path() {
        let entry = desktop_entry(Path::new(r#"/opt/Kemo %f$"\Light/kemolight"#)).unwrap();
        assert!(entry.lines().any(|line| line
            == r#"Exec=/usr/bin/env -- "/opt/Kemo %%f\\$\\"\\\\Light/kemolight" --start-hidden"#));
    }
}
