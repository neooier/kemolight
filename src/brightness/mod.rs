use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::PreferredBackend;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(feature = "mock-backend")]
pub mod mock;
#[cfg(target_os = "windows")]
pub mod windows;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayId(String);

impl DisplayId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayKind {
    InternalBacklight,
    ExternalDdcCi,
    Unknown,
}

impl DisplayKind {
    pub fn label(self) -> &'static str {
        match self {
            DisplayKind::InternalBacklight => "Internal",
            DisplayKind::ExternalDdcCi => "External DDC/CI",
            DisplayKind::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    pub name: String,
    pub kind: DisplayKind,
    pub current: Option<u32>,
    pub max: u32,
    pub supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrightnessValue {
    pub current: u32,
    pub max: u32,
}

impl BrightnessValue {
    pub fn percent(self) -> u32 {
        if self.max == 0 {
            return 0;
        }
        ((self.current.saturating_mul(100) + (self.max / 2)) / self.max).min(100)
    }
}

pub type Result<T> = std::result::Result<T, BrightnessError>;

pub trait BrightnessBackend: Send + Sync {
    fn list_displays(&self) -> Result<Vec<DisplayInfo>>;
    fn get_brightness(&self, display_id: &DisplayId) -> Result<BrightnessValue>;
    fn set_brightness(&self, display_id: &DisplayId, value: u32) -> Result<()>;
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BrightnessError {
    #[error("brightness control is unsupported on this platform")]
    Unsupported,
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("command not found: {0}")]
    CommandNotFound(String),
    #[error("DDC/CI appears to be disabled or unsupported for this display")]
    DdcCiDisabled,
    #[error("display not found: {0}")]
    DisplayNotFound(DisplayId),
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for BrightnessError {
    fn from(value: std::io::Error) -> Self {
        if value.kind() == std::io::ErrorKind::PermissionDenied {
            BrightnessError::PermissionDenied(value.to_string())
        } else {
            BrightnessError::Io(value.to_string())
        }
    }
}

pub fn clamp_percent(value: u32) -> u32 {
    value.min(100)
}

pub fn apply_step(current: u32, delta: i32, step: u32) -> u32 {
    let current = clamp_percent(current) as i32;
    let step = step.clamp(1, 100) as i32;
    let next = current.saturating_add(delta.saturating_mul(step));
    next.clamp(0, 100) as u32
}

pub fn create_backend(preferred: PreferredBackend) -> Arc<dyn BrightnessBackend> {
    match preferred {
        #[cfg(feature = "mock-backend")]
        PreferredBackend::Mock => Arc::new(mock::MockBrightnessBackend::default()),
        #[cfg(not(feature = "mock-backend"))]
        PreferredBackend::Mock => {
            Arc::new(UnsupportedBackend::new("mock backend feature is disabled"))
        }

        #[cfg(target_os = "linux")]
        PreferredBackend::Auto | PreferredBackend::Ddcutil | PreferredBackend::Sysfs => {
            Arc::new(linux::LinuxBrightnessBackend::new(preferred))
        }

        #[cfg(target_os = "windows")]
        PreferredBackend::Auto | PreferredBackend::Ddcutil | PreferredBackend::Sysfs => {
            Arc::new(windows::WindowsBrightnessBackend)
        }

        #[cfg(target_os = "macos")]
        PreferredBackend::Auto | PreferredBackend::Ddcutil | PreferredBackend::Sysfs => {
            Arc::new(macos::MacosBrightnessBackend)
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        PreferredBackend::Auto | PreferredBackend::Ddcutil | PreferredBackend::Sysfs => {
            Arc::new(UnsupportedBackend::new("platform backend is unavailable"))
        }
    }
}

pub struct UnsupportedBackend {
    message: String,
}

impl UnsupportedBackend {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl BrightnessBackend for UnsupportedBackend {
    fn list_displays(&self) -> Result<Vec<DisplayInfo>> {
        Err(BrightnessError::BackendUnavailable(self.message.clone()))
    }

    fn get_brightness(&self, display_id: &DisplayId) -> Result<BrightnessValue> {
        let _ = display_id;
        Err(BrightnessError::Unsupported)
    }

    fn set_brightness(&self, display_id: &DisplayId, value: u32) -> Result<()> {
        let _ = (display_id, value);
        Err(BrightnessError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_percent_limits_to_0_100() {
        assert_eq!(clamp_percent(0), 0);
        assert_eq!(clamp_percent(87), 87);
        assert_eq!(clamp_percent(200), 100);
    }

    #[test]
    fn apply_step_does_not_overflow_bounds() {
        assert_eq!(apply_step(98, 1, 5), 100);
        assert_eq!(apply_step(2, -1, 5), 0);
        assert_eq!(apply_step(40, 2, 10), 60);
        assert_eq!(apply_step(40, -2, 10), 20);
    }
}
