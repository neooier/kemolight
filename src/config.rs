use std::{fs, io, path::PathBuf};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub(crate) const START_HIDDEN_ARGUMENT: &str = "--start-hidden";

pub(crate) fn start_hidden_requested() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(START_HIDDEN_ARGUMENT.as_ref())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreferredBackend {
    Auto,
    Ddcutil,
    Sysfs,
    Mock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub start_hidden: bool,
    pub brightness_step: u32,
    pub preferred_backend: PreferredBackend,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            start_hidden: false,
            brightness_step: 5,
            preferred_backend: PreferredBackend::Auto,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self, ConfigError> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };

        match fs::read_to_string(&path) {
            Ok(contents) => {
                let mut config: Self = toml::from_str(&contents)?;
                config.brightness_step = normalize_step(config.brightness_step);
                Ok(config)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(ConfigError::Io { path, source: err }),
        }
    }

    pub fn normalized_step(&self) -> u32 {
        normalize_step(self.brightness_step)
    }
}

pub fn config_path() -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| dirs.config_dir().join("kemolight").join("config.toml"))
}

pub fn normalize_step(step: u32) -> u32 {
    step.clamp(1, 100)
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path:?}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_step_is_normalized() {
        assert_eq!(normalize_step(0), 1);
        assert_eq!(normalize_step(5), 5);
        assert_eq!(normalize_step(101), 100);
    }
}
